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

use anyhow::{anyhow, Result};
use cln_plugin::Plugin;
use serde_json::Value;

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

    let msat = parse_msat(&payload["msat"]).ok_or_else(|| {
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

/// Parse an msat value as CLN sends it on the wire.
///
/// Variants we accept:
///   - integer: `1000`
///   - string with suffix: `"1000msat"`
///   - bare numeric string: `"1000"`
fn parse_msat(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        let trimmed = s.strip_suffix("msat").unwrap_or(s);
        return trimmed.parse().ok();
    }
    None
}
