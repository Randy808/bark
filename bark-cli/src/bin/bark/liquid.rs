use std::str::FromStr;

use ark::lightning::Preimage;
use bitcoin::Amount;
use clap;
use bark::lightning::{pay_invoice, pay_lnaddr, pay_offer};
use bark::liquid::{pay};
use bark::Wallet;
use bark_json::cli::{InvoiceInfo, LightningReceiveInfo};

use crate::util::output_json;
#[derive(clap::Subcommand)]
pub enum LiquidCommand {
	/// pay a bolt11 invoice
	#[command()]
	Pay {
		/// The invoice to pay
		address: String,
		/// Conditionnally required if invoice doesn't have amount defined
		///
		/// Provided value must match format `<amount> <unit>`, where unit can be any amount denomination. Example: `250000 sats`.
		amount: Option<Amount>
	}
}


pub async fn execute_liquid_command(
	liquid_command: LiquidCommand,
	wallet: &mut Wallet,
) -> anyhow::Result<()> {
	match liquid_command {
		LiquidCommand::Pay { address, amount } => {
			// TODO: Change random preimage to something we are keeping track of so we can claim liquid payment and
			// finish sending to our recipient
			wallet.pay_liquid_address(address, amount.unwrap(), Preimage::random().compute_payment_hash()).await?;
		}
	}

	Ok(())
}
