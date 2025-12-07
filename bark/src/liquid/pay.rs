use anyhow::Context;
use bitcoin::Amount;
use bitcoin::hex::DisplayHex;
use log::{debug, info, trace, warn};
use server_rpc::protos;

use ark::arkoor::ArkoorPackageBuilder;
use ark::lightning::{Preimage, PaymentHash};
use ark::{ProtocolEncoding, VtxoPolicy, VtxoRequest, musig};
use bitcoin_ext::P2TR_DUST;

use crate::Wallet;
use crate::movement::{MovementDestination, MovementStatus};
use crate::movement::update::MovementUpdate;
use crate::persist::models::LiquidSend;
use crate::subsystem::{BarkSubsystem, LightningMovement, LiquidSendMovement};


impl Wallet {

	async fn process_liquid_revocation(&self, payment: &LiquidSend) -> anyhow::Result<()> {
		let mut srv = self.require_server()?;
		let htlc_vtxos = payment.htlc_vtxos.clone().into_iter()
			.map(|v| v.vtxo).collect::<Vec<_>>();

		info!("Processing {} Liquid HTLC VTXOs for revocation", htlc_vtxos.len());

		let mut secs = Vec::with_capacity(htlc_vtxos.len());
		let mut pubs = Vec::with_capacity(htlc_vtxos.len());
		let mut keypairs = Vec::with_capacity(htlc_vtxos.len());
		for input in htlc_vtxos.iter() {
			let keypair = self.get_vtxo_key(&input)?;
			let (s, p) = musig::nonce_pair(&keypair);
			secs.push(s);
			pubs.push(p);
			keypairs.push(keypair);
		}

		let revocation = ArkoorPackageBuilder::new_htlc_revocation(&htlc_vtxos, &pubs)?;

		let req = protos::RevokeLiquidPayHtlcRequest {
			htlc_vtxo_ids: revocation.arkoors.iter()
				.map(|i| i.input.id().to_bytes().to_vec())
				.collect(),
			user_nonces: revocation.arkoors.iter()
				.map(|i| i.user_nonce.serialize().to_vec())
				.collect(),
		};
		let cosign_resp: Vec<_> = srv.client.request_liquid_pay_htlc_revocation(req).await?
			.into_inner().try_into().context("invalid server cosign response")?;
		ensure!(revocation.verify_cosign_response(&cosign_resp),
			"invalid arkoor cosignature received from server",
		);

		let (vtxos, _) = revocation.build_vtxos(&cosign_resp, &keypairs, secs)?;
		let mut revoked = Amount::ZERO;
		for vtxo in &vtxos {
			info!("Got revocation VTXO: {}: {}", vtxo.id(), vtxo.amount());
			revoked += vtxo.amount();
		}

		let count = vtxos.len();
		self.movements.update_movement(
			payment.movement_id,
			MovementUpdate::new()
				.effective_balance(-payment.amount.to_signed()? + revoked.to_signed()?)
				.produced_vtxos(&vtxos)
		).await?;
		self.store_spendable_vtxos(&vtxos)?;
		self.mark_vtxos_as_spent(&htlc_vtxos)?;
		self.movements.finish_movement(payment.movement_id, MovementStatus::Failed).await?;

		self.db.remove_liquid_send(payment.payment_hash)?;

		info!("Revoked {} Liquid HTLC VTXOs", count);

		Ok(())
	}

	/// Checks the status of a liquid payment by querying the Liquid blockchain via Esplora.
	///
	/// This is similar to check_lightning_payment but adapted for Liquid. It checks if the
	/// liquid address corresponding to the HTLC has received the expected payment.
	///
	/// # Arguments
	///
	/// * `payment` - The LiquidSend record representing the payment
	///
	/// # Returns
	///
	/// Returns `Ok(Some(Preimage))` if the payment is confirmed on-chain and preimage is found.
	/// Returns `Ok(None)` for payments still pending or failed.
	/// Returns an `Err` if an error occurs during the process.
	pub async fn check_liquid_payment(&self, payment: &LiquidSend)
		-> anyhow::Result<Option<Preimage>>
	{
		let tip = self.chain.tip().await?;
		let payment_hash = payment.payment_hash;

		let policy = payment.htlc_vtxos.first().context("no vtxo provided")?.vtxo.policy();
		debug_assert!(payment.htlc_vtxos.iter().all(|v| v.vtxo.policy() == policy),
			"All liquid htlc should have the same policy",
		);
		let policy = policy.as_server_htlc_send().context("VTXO is not an HTLC send")?;
		// if policy.payment_hash != payment_hash {
		// 	bail!("Payment hash mismatch");
		// }

		// TODO: Query Esplora for the liquid address
		// For now, we'll use a placeholder that checks the server
		let mut srv = self.require_server()?;
		let req = protos::CheckLiquidPaymentRequest {
			hash: policy.payment_hash.to_vec(),
			wait: false,
		};
		let res = srv.client.check_liquid_payment(req).await?.into_inner();

		let payment_status = protos::PaymentStatus::try_from(res.status)?;

		let should_revoke = match payment_status {
			protos::PaymentStatus::Failed => {
				info!("Payment failed ({}): revoking VTXO", res.progress_message);
				true
			},
			protos::PaymentStatus::Pending => {
				if tip > policy.htlc_expiry {
					trace!("Payment is still pending, but HTLC is expired (tip: {}, \
						expiry: {}): revoking VTXO", tip, policy.htlc_expiry);
					true
				} else {
					trace!("Payment is still pending and HTLC is not expired (tip: {}, \
						expiry: {}): doing nothing for now", tip, policy.htlc_expiry);
					false
				}
			},
			protos::PaymentStatus::Complete => {
				// For liquid payments, we complete without checking preimage
				info!("Liquid payment confirmed on-chain! Payment hash: {}",
					payment.payment_hash.as_hex());

				// Complete the payment
				self.db.finish_liquid_send(payment_hash)?;
				self.mark_vtxos_as_spent(&payment.htlc_vtxos)?;
				self.movements.finish_movement(payment.movement_id,
					MovementStatus::Finished).await?;

				return Ok(None);
			},
		};

		if should_revoke {
			if let Err(e) = self.process_liquid_revocation(payment).await {
				warn!("Failed to revoke VTXO: {}", e);

				// if one of the htlc is about to expire, we exit all of them.
				// Maybe we want a different behavior here, but we have to decide whether
				// htlc vtxos revocation is a all or nothing process.
				let min_expiry = payment.htlc_vtxos.iter()
					.map(|v| v.vtxo.spec().expiry_height).min().unwrap();

				if tip > min_expiry.saturating_sub(self.config().vtxo_refresh_expiry_threshold) {
					warn!("Some VTXO is about to expire soon, marking to exit");
					let vtxos = payment.htlc_vtxos
						.iter()
						.map(|v| v.vtxo.clone())
						.collect::<Vec<_>>();
					self.exit.write().await.mark_vtxos_for_exit(&vtxos).await?;

					let exited = vtxos.iter().map(|v| v.amount()).sum::<Amount>();
					self.movements.update_movement(
						payment.movement_id,
						MovementUpdate::new()
							.effective_balance(-payment.amount.to_signed()? + exited.to_signed()?)
							.exited_vtxos(&vtxos)
					).await?;
					self.movements.finish_movement(
						payment.movement_id, MovementStatus::Failed,
					).await?;
					// self.db.finish_liquid_send(payment.payment_hash)?;
				}
			}
		}

		Ok(None)
	}

	/// Pays to a Liquid address using Ark VTXOs. This is an out-of-round payment
	/// similar to lightning payments but settling on the Liquid network.
	///
	/// # Arguments
	///
	/// * `liquid_address` - The liquid address to pay to
	/// * `amount` - The amount to send
	/// * `payment_hash` - The payment hash to use for tracking this payment
	pub async fn pay_liquid_address(
		&self,
		liquid_address: String,
		amount: Amount,
		payment_hash: PaymentHash,
	) -> anyhow::Result<()>
	{
		let mut srv = self.require_server()?;

		if amount < P2TR_DUST {
			bail!("Sent amount must be at least {}", P2TR_DUST);
		}

		if self.db.get_liquid_send(payment_hash)?.is_some() {
			bail!("Payment with this hash has already been initiated");
		}

		let (change_keypair, _) = self.derive_store_next_keypair()?;

		let inputs = self.select_vtxos_to_cover(amount, None)
			.context("Could not find enough suitable VTXOs to cover liquid payment")?;

		let mut secs = Vec::with_capacity(inputs.len());
		let mut pubs = Vec::with_capacity(inputs.len());
		let mut keypairs = Vec::with_capacity(inputs.len());
		let mut input_ids = Vec::with_capacity(inputs.len());
		for input in inputs.iter() {
			let keypair = self.get_vtxo_key(&input)?;
			let (s, p) = musig::nonce_pair(&keypair);
			secs.push(s);
			pubs.push(p);
			keypairs.push(keypair);
			input_ids.push(input.id());
		}

		let user_generated_preimage = Preimage::random();


		let req = protos::LiquidPayHtlcCosignRequest {
			liquid_address: liquid_address.clone(),
			amount_sat: amount.to_sat(),
			input_vtxo_ids: input_ids.iter().map(|v| v.to_bytes().to_vec()).collect(),
			user_nonces: pubs.iter().map(|p| p.serialize().to_vec()).collect(),
			user_pubkey: change_keypair.public_key().serialize().to_vec(),
		};

		// Request cosignature from server
		let resp = srv.client.request_liquid_pay_htlc_cosign(req).await
			.context("htlc request failed")?.into_inner();

		// cosign from srv
		let cosign_resp = resp.sigs.into_iter().map(|i| i.try_into())
			.collect::<Result<Vec<_>, _>>()?;
		let policy = VtxoPolicy::deserialize(&resp.policy)?;

		let pay_req = match policy {
			VtxoPolicy::ServerHtlcSend(policy) => {
				ensure!(policy.user_pubkey == change_keypair.public_key(), "user pubkey mismatch");
				// ensure!(policy.payment_hash == payment_hash, "payment hash mismatch");
				VtxoRequest { amount: amount, policy: policy.into() }
			},
			_ => bail!("invalid policy returned from server"),
		};

		let builder = ArkoorPackageBuilder::new(
			&inputs, &pubs, pay_req, Some(change_keypair.public_key()),
		)?;

		ensure!(builder.verify_cosign_response(&cosign_resp),
			"invalid arkoor cosignature received from server",
		);

		let (htlc_vtxos, change_vtxo) = builder.build_vtxos(&cosign_resp, &keypairs, secs)?;

		// Validate the new vtxos. They have the same chain anchor.
		let mut effective_balance = Amount::ZERO;
		for vtxo in &htlc_vtxos {
			self.validate_vtxo(vtxo).await?;
			effective_balance += vtxo.amount();
		}

		let movement_id = self.movements.new_movement(
			self.subsystem_ids[&BarkSubsystem::LiquidSend],
			LiquidSendMovement::Send.to_string(),
		).await?;
		self.movements.update_movement(
			movement_id,
			MovementUpdate::new()
				.intended_balance(-amount.to_signed()?)
				.effective_balance(-effective_balance.to_signed()?)
				.consumed_vtxos(&inputs)
				.sent_to([MovementDestination::new(liquid_address.clone(), amount)])
		).await?;
		self.store_locked_vtxos(&htlc_vtxos, Some(movement_id))?;
		self.mark_vtxos_as_spent(&input_ids)?;

		// Validate the change vtxo. It has the same chain anchor as the last input.
		if let Some(ref change) = change_vtxo {
			let last_input = inputs.last().context("no inputs provided")?;
			let tx = self.chain.get_tx(&last_input.chain_anchor().txid).await?;
			let tx = tx.with_context(|| {
				format!("input vtxo chain anchor not found for lightning change vtxo: {}", last_input.chain_anchor().txid)
			})?;
			change.validate(&tx).context("invalid lightning change vtxo")?;
			self.store_spendable_vtxos([change])?;
		}

		self.movements.update_movement(
			movement_id,
			MovementUpdate::new()
				.produced_vtxo_if_some(change_vtxo)
				.metadata(LightningMovement::htlc_metadata(&htlc_vtxos)?)
		).await?;

		// Store the pending liquid payment
		let _payment = self.db.store_new_pending_liquid_send(
			&liquid_address,
			payment_hash,
			&amount,
			&htlc_vtxos.iter().map(|v| v.id()).collect::<Vec<_>>(),
			movement_id,
		)?;

		let req = protos::InitiateLiquidPaymentRequest {
			liquid_address: liquid_address.clone(),
			amount_sat: amount.to_sat(),
			payment_hash: payment_hash.to_vec(),
			htlc_vtxo_ids: htlc_vtxos.iter().map(|v| v.id().to_bytes().to_vec()).collect(),
			wait: true,
		};

		let res = srv.client.initiate_liquid_payment(req).await?.into_inner();
		debug!("Liquid payment initiated: {}", res.progress_message);

		// For liquid payments, we don't wait for preimage, just confirmation
		// The payment will be completed when check_liquid_payment confirms it on-chain
		Ok(())
	}
}
