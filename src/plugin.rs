// src/plugin.rs
//
// CLN-side handlers — code that runs in response to events from
// `lightningd`, not in response to incoming HTTP.
//
// In slice 4 there's exactly one of these: a subscription to the
// `invoice_payment` notification, which fires on this node every
// time a BOLT11 invoice settles. We use it to credit the owning
// user's ledger.
//
// In future slices this module will likely grow to host additional
// hooks (e.g. `htlc_accepted` if we want to mediate payments) and
// notifications.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use cln_plugin::Plugin;
use serde_json::{json, Value};

use crate::cln;
use crate::db;
use crate::state::AppState;

/// `invoice_payment` notification handler.
///
/// CLN sends this when a BOLT11 invoice on this node settles. The
/// `Request` `Value` looks roughly like:
///
///     {"invoice_payment": {"label": "...", "preimage": "...", "msat": <amount>}}
///
/// Notes:
///   - The label we look up is the one **we** chose at /addinvoice
///     time (`cln-hub:<random_hex>`). If it's a label we didn't
///     create, the invoice belongs to someone else (e.g. the
///     operator running `lightning-cli invoice ...` directly) and
///     we ignore it.
///   - The `msat` field's wire format has shifted between CLN
///     versions: older CLN sends a string like `"1000msat"`, newer
///     versions send a plain integer `1000`. We accept both.
///   - The credit itself is idempotent (see `db::settle_invoice`),
///     so duplicate notifications cannot double-credit a user.
pub async fn invoice_payment(plugin: Plugin<Arc<AppState>>, request: Value) -> Result<()> {
    // CLN wraps the body under the topic name. Some test harnesses
    // pass the unwrapped body directly; we tolerate both.
    let payload = request.get("invoice_payment").unwrap_or(&request);

    let label = payload["label"]
        .as_str()
        .ok_or_else(|| anyhow!("invoice_payment notification: missing label"))?;

    let msat = cln::parse_msat(&payload["msat"]).ok_or_else(|| {
        anyhow!(
            "invoice_payment notification: cannot parse msat from {:?}",
            payload["msat"]
        )
    })?;

    // === Rust note: `plugin.state()` ===
    //
    // `Plugin<Arc<AppState>>::state()` returns `&Arc<AppState>` —
    // a reference to our shared state. We can call `.db` on it
    // directly (Arc derefs to its inner), or `Arc::clone` it if
    // we needed to move ownership somewhere.
    let state = plugin.state();

    let invoice = match db::invoices::find_by_label(&state.db, label).await? {
        Some(i) => i,
        None => {
            // Not one of our invoices (e.g. operator-created via CLI).
            // Silently ignore.
            return Ok(());
        }
    };

    // `as i64` because SQLite's INTEGER is signed 64-bit. msat fits
    // comfortably (max payable Lightning amount is ~21M BTC ≈ 2.1e15
    // msat, well below i64::MAX).
    let credited =
        db::settle_invoice(&state.db, &invoice.payment_hash, msat as i64).await?;

    if credited {
        log::info!(
            "credited {} msat to user {} for invoice {} (label={})",
            msat,
            invoice.user_id,
            invoice.payment_hash,
            label
        );
    } else {
        log::debug!(
            "invoice_payment: {} already settled, ignoring duplicate",
            invoice.payment_hash
        );
    }

    Ok(())
}

// =====================================================================
// Deposit watcher
// =====================================================================

/// Background polling loop. Every `POLL_INTERVAL`, asks CLN for the
/// list of on-chain UTXOs (`listfunds`); for any confirmed output to
/// an address in our `addresses` table that we haven't already
/// credited, atomically:
///   - inserts a row into `onchain_credits` (idempotency key)
///   - inserts a `kind='onchain_in'` ledger row
///
/// Spawned from main.rs as a fire-and-forget tokio task. The function
/// loops forever; it returns only if `Arc<AppState>` is dropped (i.e.
/// the runtime is shutting down).
///
/// === Why polling instead of a CLN notification? ===
///
/// CLN does emit a `block_added` notification we could subscribe to,
/// but the cleaner question is "which of my outputs are confirmed?".
/// `listfunds` answers that directly without us having to derive
/// address ownership from raw transactions. Polling is wasteful
/// against an idle node, but the overhead is one local JSON-RPC
/// every 30 seconds — negligible.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

pub async fn deposit_watcher(state: Arc<AppState>) {
    log::info!(
        "deposit watcher: polling lightningd `listfunds` every {}s",
        POLL_INTERVAL.as_secs()
    );

    loop {
        if let Err(e) = scan_once(&state).await {
            log::warn!("deposit watcher scan failed: {:#}", e);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One pass of the watcher: enumerate outputs, credit any we haven't
/// seen yet. Errors are propagated to `deposit_watcher`'s loop, which
/// logs them and keeps going.
async fn scan_once(state: &AppState) -> Result<()> {
    let resp = cln::call(&state.rpc_path, "listfunds", json!({})).await?;

    let Some(outputs) = resp.get("outputs").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    for output in outputs {
        // Only confirmed deposits get credited. CLN reports
        // unconfirmed outputs too, but we'd rather wait — a
        // reorg-and-double-spend would otherwise leave us with a
        // ledger credit but no real coins.
        let status = output.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status != "confirmed" {
            continue;
        }

        let address = match output.get("address").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => continue, // some outputs (e.g. spent-to-self channel funding) lack an address
        };

        // Is this address one we minted via /getbtc?
        let user_id_row: Option<(i64,)> =
            sqlx::query_as("SELECT user_id FROM addresses WHERE address = ?")
                .bind(address)
                .fetch_optional(&state.db)
                .await?;

        let Some((user_id,)) = user_id_row else {
            continue; // not ours
        };

        let txid = output
            .get("txid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let vout = output.get("output").and_then(|v| v.as_i64()).unwrap_or(0);
        let amount_msat = cln::parse_msat(output.get("amount_msat").unwrap_or(&Value::Null))
            .unwrap_or(0) as i64;
        let blockheight = output.get("blockheight").and_then(|v| v.as_i64());

        if amount_msat <= 0 || txid.is_empty() {
            continue;
        }

        match db::credit_onchain(&state.db, &txid, vout, user_id, address, amount_msat, blockheight)
            .await
        {
            Ok(true) => log::info!(
                "on-chain credit: user={} {}msat at {}:{} (block {:?})",
                user_id,
                amount_msat,
                txid,
                vout,
                blockheight
            ),
            Ok(false) => {
                // Already credited — silently skip.
            }
            Err(e) => log::warn!(
                "credit_onchain failed for {}:{} (user {}): {:#}",
                txid,
                vout,
                user_id,
                e
            ),
        }
    }

    Ok(())
}
