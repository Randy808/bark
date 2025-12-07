# Liquid Payment Usage Examples

This document provides examples of how to use the liquid payment functionality in the Bark wallet.

## Overview

Liquid payments allow you to send payments to Liquid addresses using Ark VTXOs. This is an out-of-round payment mechanism similar to Lightning payments, but settling on the Liquid network instead.

## Basic Usage

### Sending a Liquid Payment

```rust
use ark::lightning::PaymentHash;
use bitcoin::Amount;

// Create a wallet instance (see wallet initialization docs)
let wallet = Wallet::open(/* ... */).await?;

// Prepare payment details
let liquid_address = "lq1qq...".to_string(); // Liquid address to pay to
let amount = Amount::from_sat(100_000); // Amount in satoshis
let payment_hash = PaymentHash::from_slice(&[/* 32 bytes */])?;

// Send the liquid payment
wallet.pay_liquid_address(
    liquid_address,
    amount,
    payment_hash,
).await?;

println!("Liquid payment initiated!");
```

### Checking Payment Status

```rust
use ark::lightning::PaymentHash;

// Get the payment hash from your initiated payment
let payment_hash = PaymentHash::from_slice(&[/* 32 bytes */])?;

// Retrieve the pending liquid send from the database
let payment = wallet.db.get_liquid_send(payment_hash)?;

if let Some(payment) = payment {
    // Check the payment status
    let result = wallet.check_liquid_payment(&payment).await?;

    match result {
        Some(_preimage) => println!("Payment completed!"),
        None => println!("Payment still pending or failed"),
    }
}
```

## Complete Example

```rust
use anyhow::Result;
use ark::lightning::PaymentHash;
use bitcoin::Amount;
use bark_wallet::Wallet;

async fn send_liquid_payment() -> Result<()> {
    // Initialize wallet
    let wallet = Wallet::open(/* config */).await?;

    // Payment parameters
    let liquid_address = "lq1qqw3e3mk4ng3ks43mh54udznuekaadh9lgwef3mwgzrfzakmdwcvqqve2xzutyaf7vjcap67f28q90uxec2ve95g3rpu5crapcmfr2l9xl5jzazvcpysz".to_string();
    let amount = Amount::from_sat(50_000);

    // Generate a unique payment hash for this payment
    // In production, you might derive this from invoice data or generate it randomly
    let payment_hash = PaymentHash::from_slice(&[0u8; 32])?;

    // Initiate the payment
    println!("Sending {} sats to {}", amount.to_sat(), liquid_address);
    wallet.pay_liquid_address(
        liquid_address.clone(),
        amount,
        payment_hash,
    ).await?;

    println!("Payment initiated with hash: {:?}", payment_hash);

    // Poll for payment status
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        if let Some(payment) = wallet.db.get_liquid_send(payment_hash)? {
            println!("Checking payment status...");
            let result = wallet.check_liquid_payment(&payment).await?;

            if result.is_some() || payment.confirmed {
                println!("Payment confirmed!");
                break;
            }
        } else {
            println!("Payment no longer pending (completed or revoked)");
            break;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    send_liquid_payment().await
}
```

## Payment Flow

1. **Initiation**: Call `pay_liquid_address()` with the destination address, amount, and payment hash
2. **HTLC Creation**: The wallet creates HTLC VTXOs and requests server cosignature
3. **Server Processing**: The server validates and cosigns the HTLC transaction
4. **Payment Broadcast**: The server broadcasts the payment to the Liquid network
5. **Confirmation**: The payment settles on-chain on the Liquid network
6. **Completion**: The HTLCs are marked as spent and the payment is finalized

## Payment Revocation

If a liquid payment fails or expires, the wallet can revoke the HTLC VTXOs to reclaim the funds:

```rust
// This happens automatically during check_liquid_payment() when:
// - Payment status is Failed
// - HTLC has expired (current block height > HTLC expiry height)

// The revocation process:
// 1. Requests revocation cosignature from server
// 2. Builds revocation VTXOs
// 3. Stores the revoked VTXOs as spendable
// 4. Marks the original HTLCs as spent
// 5. Marks the movement as failed
```

## Error Handling

```rust
match wallet.pay_liquid_address(liquid_address, amount, payment_hash).await {
    Ok(()) => println!("Payment initiated successfully"),
    Err(e) => {
        eprintln!("Payment failed: {}", e);

        // Common errors:
        // - "Sent amount must be at least X" - amount too small
        // - "Payment with this hash has already been initiated" - duplicate payment
        // - "Could not find enough suitable VTXOs" - insufficient balance
    }
}
```

## Database Schema

Liquid sends are stored in the `bark_liquid_send` table:

```sql
CREATE TABLE bark_liquid_send (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    liquid_address TEXT NOT NULL,
    payment_hash TEXT NOT NULL UNIQUE,
    amount_sats INTEGER NOT NULL,
    htlc_vtxo_ids TEXT NOT NULL,
    movement_id INTEGER NOT NULL,
    confirmed INTEGER NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f', 'now')),
    finished_at DATETIME
);
```

## Differences from Lightning Payments

1. **Settlement**: Liquid payments settle on the Liquid sidechain, not the Lightning Network
2. **Confirmation**: Requires on-chain Liquid confirmation instead of preimage revelation
3. **No Invoice**: Uses a Liquid address instead of a BOLT11 invoice
4. **Payment Hash**: Must be provided explicitly (not derived from invoice)
5. **Finality**: On-chain finality with Liquid block confirmations

## Subsystem Integration

Liquid payments use the `BarkSubsystem::LiquidSend` subsystem for movement tracking:

```rust
// Movement is created with the LiquidSend subsystem
let movement_id = self.movements.new_movement(
    self.subsystem_ids[&BarkSubsystem::LiquidSend],
    LiquidSendMovement::Send.to_string(),
).await?;
```

## Best Practices

1. **Unique Payment Hashes**: Always use unique payment hashes for each payment
2. **Amount Validation**: Ensure amount is above dust limit (P2TR_DUST)
3. **Status Polling**: Regularly check payment status until confirmation
4. **Error Handling**: Handle all possible error cases gracefully
5. **HTLC Expiry**: Monitor HTLC expiry and request revocation if needed

## Notes

- Liquid payments require a server that supports liquid payment processing
- The server must implement the `RequestLiquidPayHtlcCosign`, `InitiateLiquidPayment`, `CheckLiquidPayment`, and `RequestLiquidPayHtlcRevocation` RPC methods
- HTLC expiry is determined by the server's `htlc_send_expiry_delta` configuration
- Payment metadata includes HTLC VTXO IDs for tracking purposes
