use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use bitcoin::Amount;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::secp256k1::PublicKey;
use log::{info, trace, warn};
use parking_lot::Mutex;

use ark::{musig, ProtocolEncoding, Vtxo, VtxoId, VtxoPolicy, VtxoRequest};
use ark::arkoor::ArkoorPackageBuilder;
use ark::lightning::{PaymentHash, Preimage};
use bitcoin_ext::{AmountExt, BlockHeight};
use bitcoin_ext::rpc::RpcApi;
use server_rpc::protos;

use crate::Server;

/// In-memory tracking of liquid payments
#[derive(Debug, Clone)]
pub struct LiquidPayment {
	pub liquid_address: String,
	pub amount: Amount,
	pub payment_hash: PaymentHash,
	pub htlc_vtxo_ids: Vec<VtxoId>,
	pub status: LiquidPaymentStatus,
	pub liquid_txid: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiquidPaymentStatus {
	Pending,
	Sent,
	Confirmed,
	Failed,
}

impl Server {
	/// Cosign a liquid pay HTLC package
	pub async fn cosign_liquid_pay_htlc(
		&self,
		inputs: Vec<VtxoId>,
		user_nonces: Vec<musig::PublicNonce>,
		user_pubkey: PublicKey,
		amount: Amount,
		payment_hash: PaymentHash,
	) -> anyhow::Result<(Vec<ark::arkoor::ArkoorCosignResponse>, VtxoPolicy)> {
		// Get the input VTXOs
		let input_vtxos = self.db.get_vtxos_by_id(&inputs).await?;
		let vtxos: Vec<Vtxo> = input_vtxos.iter().map(|v| v.vtxo.clone()).collect();

		self.check_vtxos_not_exited(&vtxos).await?;

		// Calculate expiry height
		let expiry = {
			let tip = self.bitcoind.get_block_count()? as BlockHeight;
			let expiry_delta = self.config.htlc_send_expiry_delta as u64;
			tip + expiry_delta as BlockHeight
		};

		// Create ServerHtlcSend policy (same as lightning)
		let policy = VtxoPolicy::new_server_htlc_send(user_pubkey, payment_hash, expiry);
		let pay_req = VtxoRequest { amount, policy: policy.clone() };

		// Build and cosign the arkoor package
		let package = ArkoorPackageBuilder::new(&vtxos, &user_nonces, pay_req, Some(user_pubkey))
			.context("error creating arkoor package")?;

		let cosign_resp = self.cosign_oor_package_with_builder(&package).await?;

		Ok((cosign_resp, policy))
	}

	/// Initiate a liquid payment by sending to the liquid HTLC address
	pub async fn initiate_liquid_payment(
		&self,
		liquid_address: String,
		amount: Amount,
		payment_hash: PaymentHash,
		htlc_vtxo_ids: Vec<VtxoId>,
		wait: bool,
	) -> anyhow::Result<protos::LiquidPaymentResult> {
		// Verify we have an Elements client configured
		let elementsd = self.elementsd.as_ref()
			.context("No Elements daemon configured for liquid payments")?;

		// Validate the VTXOs
		let htlc_vtxos = self.db.get_vtxos_by_id(&htlc_vtxo_ids).await?;
		for htlc_vtxo in &htlc_vtxos {
			if !htlc_vtxo.is_spendable() {
				return badarg!("input vtxo is already spent");
			}

			let policy = htlc_vtxo.vtxo.policy();
			let htlc_policy = policy.as_server_htlc_send()
				.context("VTXO is not a ServerHtlcSend policy")?;

			// if htlc_policy.payment_hash != payment_hash {
			// 	return badarg!("VTXO payment hash doesn't match");
			// }
		}

		// Get the payment tracking structure
		let payment = LiquidPayment {
			liquid_address: liquid_address.clone(),
			amount,
			payment_hash,
			htlc_vtxo_ids: htlc_vtxo_ids.clone(),
			status: LiquidPaymentStatus::Pending,
			liquid_txid: None,
		};

		// Store in server's liquid payment tracker
		self.store_liquid_payment(payment.clone()).await;

		// Send the payment via Elements RPC
		info!("Sending {} sats to liquid address {}", amount.to_sat(), liquid_address);

		// Use sendtoaddress RPC call
		let amount_btc = amount.to_sat() as f64 / 100_000_000.0;
		match elementsd.call::<String>("sendtoaddress", &[
			serde_json::json!(liquid_address),
			serde_json::json!(amount_btc),
		]) {
			Ok(txid) => {
				info!("Liquid payment sent! txid: {}", txid);

				// Update payment status
				let mut payment = payment;
				payment.status = LiquidPaymentStatus::Sent;
				payment.liquid_txid = Some(txid.clone());
				self.store_liquid_payment(payment).await;

				Ok(protos::LiquidPaymentResult {
					progress_message: format!("Payment sent to liquid address. txid: {}", txid),
					status: protos::PaymentStatus::Pending as i32,
					payment_hash: payment_hash.to_vec(),
				})
			}
			Err(e) => {
				warn!("Failed to send liquid payment: {}", e);

				// Mark as failed
				let mut payment = payment;
				payment.status = LiquidPaymentStatus::Failed;
				self.store_liquid_payment(payment).await;

				Ok(protos::LiquidPaymentResult {
					progress_message: format!("Payment failed: {}", e),
					status: protos::PaymentStatus::Failed as i32,
					payment_hash: payment_hash.to_vec(),
				})
			}
		}
	}

	/// Check the status of a liquid payment
	pub async fn check_liquid_payment(
		&self,
		payment_hash: PaymentHash,
		wait: bool,
	) -> anyhow::Result<protos::LiquidPaymentResult> {
		let payment = self.get_liquid_payment(payment_hash).await
			.context("Payment not found")?;

		match payment.status {
			LiquidPaymentStatus::Pending => {
				Ok(protos::LiquidPaymentResult {
					progress_message: "Payment is pending".to_string(),
					status: protos::PaymentStatus::Pending as i32,
					payment_hash: payment_hash.to_vec(),
				})
			}
			LiquidPaymentStatus::Sent => {
				// For POC, we'll check if the transaction has confirmations
				if let Some(ref elementsd) = self.elementsd {
					if let Some(ref txid) = payment.liquid_txid {
						// Check if transaction is confirmed using gettransaction
						match elementsd.call::<serde_json::Value>("gettransaction", &[serde_json::json!(txid)]) {
							Ok(tx_info) => {
								let confs = tx_info.get("confirmations")
									.and_then(|v| v.as_u64())
									.unwrap_or(0);

								if confs >= 1 {
									info!("Liquid payment confirmed! txid: {}", txid);

									// Update status to confirmed
									let mut updated_payment = payment.clone();
									updated_payment.status = LiquidPaymentStatus::Confirmed;
									self.store_liquid_payment(updated_payment).await;

									Ok(protos::LiquidPaymentResult {
										progress_message: format!("Payment confirmed with {} confirmations", confs),
										status: protos::PaymentStatus::Complete as i32,
										payment_hash: payment_hash.to_vec(),
									})
								} else {
									Ok(protos::LiquidPaymentResult {
										progress_message: format!("Payment sent, waiting for confirmation ({} confs)", confs),
										status: protos::PaymentStatus::Pending as i32,
										payment_hash: payment_hash.to_vec(),
									})
								}
							}
							Err(e) => {
								warn!("Error checking liquid transaction: {}", e);
								Ok(protos::LiquidPaymentResult {
									progress_message: format!("Error checking transaction: {}", e),
									status: protos::PaymentStatus::Pending as i32,
									payment_hash: payment_hash.to_vec(),
								})
							}
						}
					} else {
						Ok(protos::LiquidPaymentResult {
							progress_message: "Payment sent but no txid available".to_string(),
							status: protos::PaymentStatus::Pending as i32,
							payment_hash: payment_hash.to_vec(),
						})
					}
				} else {
					Ok(protos::LiquidPaymentResult {
						progress_message: "No Elements daemon available".to_string(),
						status: protos::PaymentStatus::Failed as i32,
						payment_hash: payment_hash.to_vec(),
					})
				}
			}
			LiquidPaymentStatus::Confirmed => {
				Ok(protos::LiquidPaymentResult {
					progress_message: "Payment confirmed".to_string(),
					status: protos::PaymentStatus::Complete as i32,
					payment_hash: payment_hash.to_vec(),
				})
			}
			LiquidPaymentStatus::Failed => {
				Ok(protos::LiquidPaymentResult {
					progress_message: "Payment failed".to_string(),
					status: protos::PaymentStatus::Failed as i32,
					payment_hash: payment_hash.to_vec(),
				})
			}
		}
	}

	// Helper methods for payment tracking
	async fn store_liquid_payment(&self, payment: LiquidPayment) {
		let mut payments = self.liquid_payments.lock();
		payments.insert(payment.payment_hash, payment);
	}

	async fn get_liquid_payment(&self, payment_hash: PaymentHash) -> anyhow::Result<LiquidPayment> {
		let payments = self.liquid_payments.lock();
		payments.get(&payment_hash)
			.cloned()
			.context("Payment not found")
	}
}

