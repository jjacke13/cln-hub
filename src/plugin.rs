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

// =====================================================================
// External payment reconciler (slice 5b)
// =====================================================================

/// Every `RECONCILE_INTERVAL`, look at every `external_pending`
/// payment, ask CLN's `listpays` what really happened, and finalize
/// the row accordingly. Runs once at startup before the periodic
/// loop kicks in, so crash-mid-pay state from a previous run is
/// resolved before new requests can be served.
///
/// Why this exists:
///
///   The /payinvoice handler does {reserve → CLN pay → settle/fail}
///   synchronously. If we crash between `pay` returning and the
///   settle/fail transaction, the row is stuck in `external_pending`
///   and the user's balance is locked. The reconciler unsticks them.
///
///   It also handles the CLN error codes we treat as "in-flight"
///   (200, 210, 211): the handler returns 402 to the client, but the
///   actual HTLCs may still resolve in the background. The reconciler
///   picks those up.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// Refusal-to-refund guards for the case where CLN's `listpays`
/// returns no record at all of a hash we have in `external_pending`.
///
/// The naive thing is to assume "CLN never saw this payment, refund
/// it." That's unsafe: `listpays` can transiently return empty for a
/// payment that actually completed — CLN restart in the middle of
/// writing out the pay, a version-skew in the response schema, a
/// brief socket hiccup, etc. Refunding in those windows would
/// double-credit the user (they paid once successfully AND got the
/// reserve back) and drain the hub.
///
/// Two guards before we refund on empty `listpays`:
///   1. **Minimum age.** The row must be older than `MIN_REFUND_AGE`,
///      giving any in-flight HTLCs and CLN-side bookkeeping time to
///      surface in `listpays`.
///   2. **Minimum consecutive empty sweeps.** We require N empty
///      sightings, persisted to disk between sweeps so a process
///      restart doesn't reset the count.
///
/// Even one non-empty `listpays` response (regardless of its status)
/// resets the counter, so the refund only fires on sustained absence,
/// not transient flicker.
const MIN_REFUND_AGE: Duration = Duration::from_secs(300);
const MIN_EMPTY_SWEEPS_BEFORE_REFUND: i64 = 3;

pub async fn payment_reconciler(state: Arc<AppState>) {
    // Startup pass first — we may have pending rows from a crashed
    // previous run.
    log::info!("payment reconciler: initial startup sweep");
    if let Err(e) = reconcile_once(&state).await {
        log::warn!("payment reconciler startup sweep failed: {:#}", e);
    }

    log::info!(
        "payment reconciler: periodic sweep every {}s",
        RECONCILE_INTERVAL.as_secs()
    );

    loop {
        tokio::time::sleep(RECONCILE_INTERVAL).await;
        if let Err(e) = reconcile_once(&state).await {
            log::warn!("payment reconciler sweep failed: {:#}", e);
        }
    }
}

/// One reconciliation pass. For each pending payment row, query CLN
/// for its true state and finalize.
async fn reconcile_once(state: &AppState) -> Result<()> {
    let pending = db::list_pending_external(&state.db).await?;
    if pending.is_empty() {
        return Ok(());
    }

    log::debug!("payment reconciler: {} pending payment(s)", pending.len());

    for p in pending {
        if let Err(e) = reconcile_one(state, &p).await {
            // Don't propagate — one bad row shouldn't stop the sweep.
            log::warn!(
                "reconcile failed for user={} hash={}: {:#}",
                p.user_id,
                p.payment_hash,
                e
            );
        }
    }
    Ok(())
}

/// Ask CLN about a single payment hash. CLN's `listpays` returns one
/// or more attempt records under `.pays`; we look at the most recent
/// one and act on its `status`.
async fn reconcile_one(state: &AppState, p: &db::PendingPayment) -> Result<()> {
    let resp = cln::call(
        &state.rpc_path,
        "listpays",
        json!({"payment_hash": p.payment_hash}),
    )
    .await?;

    let pays = resp
        .get("pays")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // ---- Empty listpays: handle very conservatively. ----
    //
    // The naive read is "CLN doesn't know this hash, so refund the
    // user." That's unsafe: a transiently-empty response (CLN restart
    // mid-write, a brief schema/socket hiccup, a different CLN
    // version's response shape) would refund a payment that actually
    // completed, costing the hub the routed amount.
    //
    // We require BOTH:
    //   - the row to be older than `MIN_REFUND_AGE` (no jumpy refunds
    //     on payments that just started),
    //   - and N consecutive empty sweeps recorded in the DB (so a
    //     transient flicker doesn't accrue toward the refund).
    if pays.is_empty() {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(p.created_at);
        let age = now_secs - p.created_at;

        if age < MIN_REFUND_AGE.as_secs() as i64 {
            log::debug!(
                "reconcile: hash={} empty listpays but age {}s < {}s — leaving pending",
                p.payment_hash,
                age,
                MIN_REFUND_AGE.as_secs()
            );
            return Ok(());
        }

        let sweeps =
            db::bump_empty_listpays_sweeps(&state.db, p.user_id, &p.payment_hash).await?;

        if sweeps < MIN_EMPTY_SWEEPS_BEFORE_REFUND {
            log::info!(
                "reconcile: hash={} empty listpays sweep {} of {} — leaving pending",
                p.payment_hash,
                sweeps,
                MIN_EMPTY_SWEEPS_BEFORE_REFUND
            );
            return Ok(());
        }

        log::warn!(
            "reconcile: hash={} empty listpays for {} sweeps (age {}s); refunding",
            p.payment_hash,
            sweeps,
            age
        );
        let refunded = db::fail_external_pay(
            &state.db,
            p.user_id,
            &p.payment_hash,
            p.amount_msat,
            p.fee_reserve_msat,
        )
        .await?;
        if !refunded {
            log::info!(
                "reconcile: hash={} fail no-op — row finalized by another path",
                p.payment_hash
            );
        }
        return Ok(());
    }

    // CLN returned a real listpays entry — clear the empty-sweep
    // counter regardless of status. We only refund on SUSTAINED
    // empty streaks, not interleaved flicker.
    db::reset_empty_listpays_sweeps(&state.db, p.user_id, &p.payment_hash).await?;

    // Take the LAST attempt (most recent). CLN appends; oldest first.
    // `let-else` so a future refactor that breaks the `is_empty` guard
    // can't turn this into a panic in a background task.
    let Some(last) = pays.last() else {
        log::warn!(
            "reconcile: hash={} pays array unexpectedly empty after non-empty check; skipping",
            p.payment_hash
        );
        return Ok(());
    };
    let status = last
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match status {
        "complete" => {
            let preimage = last["preimage"].as_str().unwrap_or("").to_string();
            let sent_msat = cln::parse_msat(&last["amount_sent_msat"])
                .map(|n| n as i64)
                .unwrap_or(p.amount_msat);
            let actual_fee_msat = (sent_msat - p.amount_msat).max(0);

            let settled = db::settle_external_pay(
                &state.db,
                p.user_id,
                &p.payment_hash,
                &preimage,
                actual_fee_msat,
                p.fee_reserve_msat,
            )
            .await?;

            if settled {
                log::info!(
                    "reconcile: settled hash={} user={} amount={}msat fee={}msat",
                    p.payment_hash,
                    p.user_id,
                    p.amount_msat,
                    actual_fee_msat
                );
            } else {
                log::debug!(
                    "reconcile: settle no-op for hash={} (already finalized)",
                    p.payment_hash
                );
            }
        }
        "failed" => {
            let failed = db::fail_external_pay(
                &state.db,
                p.user_id,
                &p.payment_hash,
                p.amount_msat,
                p.fee_reserve_msat,
            )
            .await?;
            if failed {
                log::info!(
                    "reconcile: failed hash={} user={} — refunded",
                    p.payment_hash,
                    p.user_id
                );
            } else {
                log::debug!(
                    "reconcile: fail no-op for hash={} (already finalized)",
                    p.payment_hash
                );
            }
        }
        "pending" => {
            // Still in flight. Next sweep.
            log::debug!(
                "reconcile: hash={} user={} still pending on CLN",
                p.payment_hash,
                p.user_id
            );
        }
        other => {
            log::warn!(
                "reconcile: hash={} unexpected status '{}', leaving pending",
                p.payment_hash,
                other
            );
        }
    }

    Ok(())
}

// =====================================================================
// Deposit watcher helpers
// =====================================================================

/// One pass of the watcher: enumerate outputs, credit any we haven't
/// seen yet. Errors are propagated to `deposit_watcher`'s loop, which
/// logs them and keeps going.
///
/// === Why we re-query the chain tip every pass ===
///
/// CLN's `listfunds` reports each UTXO's `blockheight` (where it
/// landed) but NOT its current depth. To enforce
/// `min_deposit_confs` we compare against the current tip from
/// `getinfo`. One extra unix-socket round trip per scan;
/// imperceptible cost.
async fn scan_once(state: &AppState) -> Result<()> {
    let info = cln::call(&state.rpc_path, "getinfo", json!({})).await?;
    let tip = info
        .get("blockheight")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

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

        // Depth = tip - blockheight + 1 (inclusive).
        // Skip until we have at least `min_deposit_confs`.
        let blockheight = output.get("blockheight").and_then(|v| v.as_i64());
        let depth = blockheight.map(|bh| tip - bh + 1).unwrap_or(0);
        if depth < state.min_deposit_confs {
            log::debug!(
                "deposit watcher: skipping txid={:?} (depth {} < min {})",
                output.get("txid").and_then(|v| v.as_str()),
                depth,
                state.min_deposit_confs
            );
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
