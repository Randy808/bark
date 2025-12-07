use anyhow::Context;
use rusqlite::Transaction;

use super::Migration;

pub struct Migration0023 {}

impl Migration for Migration0023 {
	fn name(&self) -> &str {
		"Add liquid send table"
	}

	fn to_version(&self) -> i64 { 23 }

	fn do_migration(&self, conn: &Transaction) -> anyhow::Result<()> {
		let query = "
			CREATE TABLE IF NOT EXISTS bark_liquid_send (
				id INTEGER PRIMARY KEY AUTOINCREMENT,
				liquid_address TEXT NOT NULL,
				payment_hash TEXT NOT NULL UNIQUE,
				amount_sats INTEGER NOT NULL,
				htlc_vtxo_ids TEXT NOT NULL,
				movement_id INTEGER NOT NULL,
				confirmed INTEGER NOT NULL DEFAULT 0,
				created_at DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')),
				finished_at DATETIME
			)
		";

		conn.execute(query, ()).context("failed to create bark_liquid_send table")?;

		Ok(())
	}
}
