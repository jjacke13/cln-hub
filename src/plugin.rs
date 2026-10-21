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

use std::collections::HashMap;
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

    let msat = crate::amount::from_msat(msat)
        .ok_or_else(|| anyhow!("invoice_payment amount exceeds monetary bounds"))?;
    let credited = db::settle_invoice(&state.db, &invoice.payment_hash, msat).await?;

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
/// list of on-chain outputs (`listfunds spent=true`); for any sufficiently confirmed output to
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
    if let Err(error) = reconcile_incoming_once(state).await {
        // A bad invoice/RPC must not prevent outgoing-payment recovery.
        log::warn!("incoming invoice reconciliation failed: {:#}", error);
    }
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

/// Replay CLN's durable paid state rather than relying on live notifications.
async fn reconcile_incoming_once(state: &AppState) -> Result<()> {
    for invoice in db::invoices::list_unsettled(&state.db).await? {
        if let Err(error) = reconcile_incoming_invoice(state, &invoice).await {
            log::warn!(
                "incoming reconciliation failed for {}: {:#}",
                invoice.payment_hash,
                error
            );
        }
    }
    Ok(())
}

async fn reconcile_incoming_invoice(state: &AppState, invoice: &db::invoices::Row) -> Result<()> {
    let response = cln::call(
        &state.rpc_path,
        "listinvoices",
        json!({"label": invoice.label}),
    )
    .await?;
    let invoices = response["invoices"]
        .as_array()
        .ok_or_else(|| anyhow!("listinvoices response missing invoices array"))?;
    for paid in invoices {
        if paid["status"].as_str() != Some("paid") {
            continue;
        }
        anyhow::ensure!(
            paid["payment_hash"].as_str() == Some(&invoice.payment_hash),
            "listinvoices returned a different payment hash"
        );
        let received = cln::parse_msat(&paid["amount_received_msat"])
            .and_then(crate::amount::from_msat)
            .ok_or_else(|| anyhow!("paid invoice has invalid amount_received_msat"))?;
        db::settle_invoice(&state.db, &invoice.payment_hash, received).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spent_deposit_recovery_uses_original_height_and_is_idempotent() {
        let pool = db::tests::fresh_pool().await;
        let user = db::tests::make_user(&pool, "receiver").await;
        db::addresses::create(&pool, user, "regtest-deposit")
            .await
            .unwrap();
        let spent_hash = "de".repeat(32);
        let confirmed_hash = "cf".repeat(32);
        let outputs = json!({"outputs": [
            {"txid": spent_hash, "output": 0, "address": "regtest-deposit",
             "amount_msat": 30_000, "status": "spent"},
            {"txid": confirmed_hash, "output": 1, "address": "regtest-deposit",
             "amount_msat": 20_000, "status": "confirmed", "blockheight": 100}
        ]});
        let mut steps = Vec::new();
        for _ in 0..2 {
            steps.extend([
                ("getinfo", json!({}), json!({"blockheight": 110})),
                ("listfunds", json!({"spent": true}), outputs.clone()),
                (
                    "listtransactions",
                    json!({}),
                    json!({"transactions": [
                        {"hash": spent_hash, "blockheight": 100}
                    ]}),
                ),
            ]);
        }
        let (path, server) = crate::test_support::rpc(steps).await;
        let state = AppState {
            rpc_path: path,
            db: pool.clone(),
            min_deposit_confs: 6,
        };
        scan_once(&state).await.unwrap();
        scan_once(&state).await.unwrap();
        server.await.unwrap();
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 50_000);
        let deposits = db::onchain_credits::list_for_user(&pool, user)
            .await
            .unwrap();
        assert_eq!(deposits.len(), 2);
        assert!(deposits
            .iter()
            .all(|deposit| deposit.blockheight == Some(100)));
    }

    #[tokio::test]
    async fn spent_output_still_requires_a_known_sufficiently_confirmed_deposit() {
        let pool = db::tests::fresh_pool().await;
        let user = db::tests::make_user(&pool, "receiver").await;
        db::addresses::create(&pool, user, "regtest-deposit")
            .await
            .unwrap();
        let output = json!({"txid": "unknown", "output": 0, "address": "regtest-deposit",
            "amount_msat": 30_000, "status": "spent"});
        let mut outputs = Vec::new();
        for (hash, height) in [
            ("unknown", None),
            ("unconfirmed", Some(0)),
            ("shallow", Some(108)),
            ("future", Some(111)),
            ("negative", Some(-1)),
        ] {
            let mut entry = output.clone();
            entry["txid"] = json!(hash);
            if let Some(height) = height {
                entry["blockheight"] = json!(height);
            }
            outputs.push(entry);
        }
        let mut no_index = output.clone();
        no_index.as_object_mut().unwrap().remove("output");
        no_index["blockheight"] = json!(100);
        outputs.push(no_index);
        let (path, server) = crate::test_support::rpc(vec![
            ("getinfo", json!({}), json!({"blockheight": 110})),
            (
                "listfunds",
                json!({"spent": true}),
                json!({"outputs": outputs}),
            ),
            (
                "listtransactions",
                json!({}),
                json!({"transactions": [
                    {"hash": "unconfirmed", "blockheight": 0},
                    {"hash": "unrelated-spend", "blockheight": 100}
                ]}),
            ),
        ])
        .await;
        let state = AppState {
            rpc_path: path,
            db: pool.clone(),
            min_deposit_confs: 6,
        };
        scan_once(&state).await.unwrap();
        server.await.unwrap();
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 0);
        assert!(db::onchain_credits::list_for_user(&pool, user)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn malformed_transaction_history_cannot_supply_deposit_confirmations() {
        let pool = db::tests::fresh_pool().await;
        let (path, server) = crate::test_support::rpc(vec![
            ("getinfo", json!({}), json!({"blockheight": 110})),
            (
                "listfunds",
                json!({"spent": true}),
                json!({"outputs": [
                    {"txid": "unknown", "status": "spent"}
                ]}),
            ),
            ("listtransactions", json!({}), json!({"transactions": null})),
        ])
        .await;
        let state = AppState {
            rpc_path: path,
            db: pool,
            min_deposit_confs: 6,
        };
        assert!(scan_once(&state).await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reconciler_matches_the_current_attempt_label() {
        let pool = db::tests::fresh_pool().await;
        let user = db::tests::make_user(&pool, "payer").await;
        db::tests::seed_balance(&pool, user, 100_000).await;
        db::reserve_external_pay(&pool, user, "same", "lnbc", "", 30_000, 5000)
            .await
            .unwrap();
        let old = db::list_pending_external(&pool)
            .await
            .unwrap()
            .pop()
            .unwrap();
        db::fail_external_pay(&pool, old.id).await.unwrap();
        db::reserve_external_pay(&pool, user, "same", "lnbc", "", 70_000, 5000)
            .await
            .unwrap();
        let new = db::list_pending_external(&pool)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let old_status =
            json!({"payment_hash": "same", "label": old.cln_label, "status": "failed"});
        let paid_status = json!({"payment_hash": "same", "label": new.cln_label, "status": "complete",
            "preimage": "ab".repeat(32), "amount_sent_msat": 72_000});
        let (path, server) = crate::test_support::rpc(vec![
            (
                "listpays",
                json!({"payment_hash": "same"}),
                json!({"pays": [old_status.clone()]}),
            ),
            (
                "listpays",
                json!({"payment_hash": "same"}),
                json!({"pays": [old_status, paid_status]}),
            ),
        ])
        .await;
        let state = AppState {
            rpc_path: path,
            db: pool.clone(),
            min_deposit_confs: 6,
        };
        reconcile_one(&state, &new).await.unwrap();
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 25_000);
        assert_eq!(
            db::list_pending_external(&pool).await.unwrap()[0].id,
            new.id
        );
        reconcile_one(&state, &new).await.unwrap();
        server.await.unwrap();
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 28_000);
        assert!(db::list_pending_external(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_listpays_never_refunds_or_counts_as_empty() {
        let pool = db::tests::fresh_pool().await;
        let user = db::tests::make_user(&pool, "payer").await;
        db::tests::seed_balance(&pool, user, 100_000).await;
        db::reserve_external_pay(&pool, user, "hash", "lnbc", "", 30_000, 5000)
            .await
            .unwrap();
        sqlx::query("UPDATE payments SET created_at=?, empty_listpays_sweeps=2")
            .bind(db::unix_now() - 600)
            .execute(&pool)
            .await
            .unwrap();
        let pending = db::list_pending_external(&pool)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let responses = [
            json!({}),
            json!({"pays": null}),
            json!({"pays": {}}),
            json!({"pays": [{"payment_hash": "other", "status": "failed"}]}),
            json!({"pays": [{"payment_hash": "hash"}]}),
        ];
        let steps = responses
            .into_iter()
            .map(|response| ("listpays", json!({"payment_hash": "hash"}), response))
            .collect();
        let (path, server) = crate::test_support::rpc(steps).await;
        let state = AppState {
            rpc_path: path,
            db: pool.clone(),
            min_deposit_confs: 6,
        };
        for _ in 0..5 {
            assert!(reconcile_one(&state, &pending).await.is_err());
        }
        server.await.unwrap();
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 65_000);
        assert_eq!(db::list_pending_external(&pool).await.unwrap().len(), 1);
        let (sweeps,): (i64,) = sqlx::query_as("SELECT empty_listpays_sweeps FROM payments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sweeps, 0);
    }

    #[tokio::test]
    async fn valid_empty_listpays_still_requires_three_old_sweeps() {
        let pool = db::tests::fresh_pool().await;
        let user = db::tests::make_user(&pool, "payer").await;
        db::tests::seed_balance(&pool, user, 100_000).await;
        db::reserve_external_pay(&pool, user, "hash", "lnbc", "", 30_000, 5000)
            .await
            .unwrap();
        sqlx::query("UPDATE payments SET created_at=?")
            .bind(db::unix_now() - 600)
            .execute(&pool)
            .await
            .unwrap();
        let pending = db::list_pending_external(&pool)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let (path, server) = crate::test_support::rpc(
            (0..3)
                .map(|_| {
                    (
                        "listpays",
                        json!({"payment_hash": "hash"}),
                        json!({"pays": []}),
                    )
                })
                .collect(),
        )
        .await;
        let state = AppState {
            rpc_path: path,
            db: pool.clone(),
            min_deposit_confs: 6,
        };
        for _ in 0..2 {
            reconcile_one(&state, &pending).await.unwrap();
            assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 65_000);
        }
        reconcile_one(&state, &pending).await.unwrap();
        server.await.unwrap();
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 100_000);
    }

    #[tokio::test]
    async fn missed_invoice_notification_is_recovered_exactly_once() {
        let pool = db::tests::fresh_pool().await;
        let user = db::tests::make_user(&pool, "receiver").await;
        // A notification arriving before its ownership row exists is also recoverable.
        assert!(!db::settle_invoice(&pool, "incoming", 30_000).await.unwrap());
        db::tests::make_invoice(&pool, user, "incoming", 30_000).await;
        let (path, server) = crate::test_support::rpc(vec![(
            "listinvoices",
            json!({"label": "test-incoming"}),
            json!({"invoices": [{"payment_hash": "incoming", "status": "paid",
                "amount_received_msat": "31000msat"}]}),
        )])
        .await;
        let state = AppState {
            rpc_path: path,
            db: pool.clone(),
            min_deposit_confs: 6,
        };
        reconcile_incoming_once(&state).await.unwrap();
        server.await.unwrap();
        reconcile_incoming_once(&state).await.unwrap();
        assert!(!db::settle_invoice(&pool, "incoming", 31_000).await.unwrap());
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 31_000);
    }

    #[tokio::test]
    async fn malformed_paid_invoice_cannot_credit_balance() {
        let pool = db::tests::fresh_pool().await;
        let user = db::tests::make_user(&pool, "receiver").await;
        db::tests::make_invoice(&pool, user, "incoming", 30_000).await;
        let (path, server) = crate::test_support::rpc(vec![(
            "listinvoices",
            json!({"label": "test-incoming"}),
            json!({"invoices": [{"payment_hash": "incoming", "status": "paid"}]}),
        )])
        .await;
        let state = AppState {
            rpc_path: path,
            db: pool.clone(),
            min_deposit_confs: 6,
        };
        reconcile_incoming_once(&state).await.unwrap();
        server.await.unwrap();
        assert_eq!(db::balance_msat(&pool, user).await.unwrap(), 0);
        assert_eq!(db::invoices::list_unsettled(&pool).await.unwrap().len(), 1);
    }
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

    let Some(pays) = resp.get("pays").and_then(Value::as_array) else {
        db::reset_empty_listpays_sweeps(&state.db, p.id).await?;
        anyhow::bail!("listpays response missing pays array; leaving payment pending");
    };
    if pays.iter().any(|pay| {
        pay["payment_hash"].as_str() != Some(p.payment_hash.as_str())
            || !matches!(
                pay["status"].as_str(),
                Some("complete" | "failed" | "pending")
            )
    }) {
        db::reset_empty_listpays_sweeps(&state.db, p.id).await?;
        anyhow::bail!("listpays returned an invalid payment record; leaving payment pending");
    }

    // A failed previous attempt is not evidence that this attempt failed.
    let matching: Vec<&Value> = pays
        .iter()
        .filter(|pay| {
            p.cln_label
                .as_deref()
                .is_none_or(|label| pay["label"].as_str() == Some(label))
        })
        .collect();
    if p.cln_label.is_some() && matching.is_empty() && pays.iter().any(|pay| pay["label"].is_null())
    {
        db::reset_empty_listpays_sweeps(&state.db, p.id).await?;
        anyhow::bail!("listpays omitted attempt labels; leaving payment pending");
    }
    let pays = matching;

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

        let sweeps = db::bump_empty_listpays_sweeps(&state.db, p.id).await?;

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
        let refunded = db::fail_external_pay(&state.db, p.id).await?;
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
    db::reset_empty_listpays_sweeps(&state.db, p.id).await?;

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
            // Validate preimage shape before settling. CLN's
            // `listpays` historically reports the preimage on completed
            // pays; a missing or malformed one is a CLN bug or
            // version-skew. Leaving the row pending makes the
            // reconciler retry on the next sweep — better than
            // poisoning the payments row with an unusable receipt.
            let Some(preimage) = cln::validate_preimage(&last["preimage"]) else {
                log::warn!(
                    "reconcile: hash={} listpays.complete missing/invalid preimage; \
                     leaving pending for retry. raw last={:?}",
                    p.payment_hash,
                    last
                );
                return Ok(());
            };
            let actual_fee_msat = cln::payment_fee_msat(last, p.amount_msat, p.fee_reserve_msat)?;

            let settled =
                db::settle_external_pay(&state.db, p.id, &preimage, actual_fee_msat).await?;

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
            let failed = db::fail_external_pay(&state.db, p.id).await?;
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
/// Confirmation depth comes from the current tip and the deposit's height,
/// never its spending transaction. Spent outputs may omit their height;
/// recover that from CLN's transaction history before crediting anything.
async fn scan_once(state: &AppState) -> Result<()> {
    let info = cln::call(&state.rpc_path, "getinfo", json!({})).await?;
    let tip = info
        .get("blockheight")
        .and_then(|v| v.as_i64())
        .filter(|height| *height > 0)
        .ok_or_else(|| anyhow!("getinfo returned no valid chain height"))?;

    let resp = cln::call(&state.rpc_path, "listfunds", json!({"spent": true})).await?;

    let outputs = resp
        .get("outputs")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("listfunds response missing outputs array"))?;
    let historical_heights: HashMap<String, i64> = if outputs.iter().any(|output| {
        output["status"].as_str() == Some("spent")
            && output["blockheight"]
                .as_i64()
                .filter(|height| *height > 0)
                .is_none()
    }) {
        let history = cln::call(&state.rpc_path, "listtransactions", json!({})).await?;
        let transactions = history["transactions"]
            .as_array()
            .ok_or_else(|| anyhow!("listtransactions response missing transactions array"))?;
        transactions
            .iter()
            .filter_map(|tx| {
                let hash = tx["hash"].as_str()?;
                let height = tx["blockheight"].as_i64().filter(|height| *height > 0)?;
                Some((hash.to_owned(), height))
            })
            .collect()
    } else {
        HashMap::new()
    };

    for output in outputs {
        // Spending a deposit does not erase its owner's right to the credit.
        let status = output.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(status, "confirmed" | "spent") {
            continue;
        }

        let Some(txid) = output["txid"].as_str().filter(|hash| !hash.is_empty()) else {
            continue;
        };
        let Some(vout) = output["output"]
            .as_u64()
            .and_then(|vout| u32::try_from(vout).ok())
        else {
            continue;
        };
        // Depth = tip - blockheight + 1 (inclusive).
        // Skip until we have at least `min_deposit_confs`.
        let blockheight = output["blockheight"]
            .as_i64()
            .filter(|height| *height > 0)
            .or_else(|| historical_heights.get(txid).copied());
        let Some(height) = blockheight.filter(|height| *height <= tip) else {
            continue;
        };
        let depth = tip - height + 1;
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

        let amount_msat = cln::parse_msat(output.get("amount_msat").unwrap_or(&Value::Null))
            .and_then(crate::amount::from_msat)
            .unwrap_or(0);

        if amount_msat <= 0 {
            continue;
        }

        match db::credit_onchain(
            &state.db,
            txid,
            i64::from(vout),
            user_id,
            address,
            amount_msat,
            blockheight,
        )
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
