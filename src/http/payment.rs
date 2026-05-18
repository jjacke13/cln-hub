// src/http/payment.rs
//
// Payment-related authenticated endpoints:
//   - POST /payinvoice   — pay a BOLT11 (internal short-circuit only
//                          for now; external CLN `pay` is slice 5b)
//   - GET  /getbalance   — current balance, LndHub shape
//   - GET  /gettxs       — outbound payment history, LndHub shape

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cln;
use crate::db;
use crate::state::AppState;

use super::{AppError, AuthUser};

// =====================================================================
// /payinvoice
// =====================================================================

#[derive(Deserialize)]
pub(super) struct PayInvoiceReq {
    /// BOLT11 string to pay.
    invoice: String,
    /// Optional override amount in **satoshis** (used for amountless
    /// invoices). Number or string of digits, like /addinvoice.amt.
    #[serde(default)]
    amount: Value,
}

/// `POST /payinvoice` — debit the authenticated user and settle the
/// destination invoice.
///
/// **Slice 5a behaviour**: if the destination invoice is one we
/// issued (i.e. another local user is the receiver), the whole
/// thing happens inside one SQLite transaction — the sender is
/// debited, the receiver is credited, and the invoice is marked
/// settled. No fee. No CLN traffic.
///
/// If the destination invoice is **not** one of ours, we return a
/// `402 Payment Required` with `code: 7` ("external payments require
/// channels"). Slice 5b will replace that branch with a real
/// `lightning-cli pay` flow.
pub(super) async fn payinvoice(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<PayInvoiceReq>,
) -> Result<Json<Value>, AppError> {
    // ---- 1. Decode the BOLT11 by asking CLN. ----
    let decoded = cln::call(
        &state.rpc_path,
        "decode",
        json!({"string": &req.invoice}),
    )
    .await?;

    if decoded.get("valid").and_then(|v| v.as_bool()) == Some(false) {
        return Err(AppError::payment(7, "could not decode payment request"));
    }

    let payment_hash = decoded
        .get("payment_hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::payment(7, "BOLT11 missing payment_hash"))?
        .to_string();

    let memo = decoded
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let destination = decoded
        .get("payee")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // ---- 2. Resolve amount: invoice's own, or override, or error. ----
    let invoice_msat = decoded.get("amount_msat").and_then(|v| v.as_i64());
    let override_msat = parse_amount_sats_to_msat(&req.amount)?;

    let amount_msat = match (invoice_msat, override_msat) {
        (Some(amt), _) => amt,                  // invoice has an amount; use it
        (None, Some(amt)) => amt,               // amountless + override
        (None, None) => {
            return Err(AppError::bad_request(
                "amountless invoice requires an `amount` field (in sats)",
            ))
        }
    };

    if amount_msat <= 0 {
        return Err(AppError::bad_request("invoice amount must be > 0"));
    }

    // ---- 3. Try the internal short-circuit. ----
    use db::InternalPayResult::*;
    let result = db::try_settle_internal(
        &state.db,
        auth.user_id,
        &payment_hash,
        &req.invoice,
        amount_msat,
        &memo,
    )
    .await?;

    match result {
        Settled { receiver_user_id } => {
            log::info!(
                "internal payment: user {} -> user {}, {} msat, hash={}",
                auth.user_id,
                receiver_user_id,
                amount_msat,
                payment_hash,
            );
            // Internal payment — no real preimage from the network.
            Ok(Json(payment_response(
                &destination,
                &payment_hash,
                amount_msat,
                0,
                &memo,
                None,
            )))
        }

        AlreadyPaid => Err(AppError::payment(7, "invoice already paid")),

        SelfPayment => Err(AppError::bad_request("cannot pay your own invoice")),

        InsufficientBalance { balance_msat } => Err(AppError::payment(
            5,
            format!(
                "not enough balance: have {} msat, need {} msat",
                balance_msat, amount_msat
            ),
        )),

        NotOurInvoice => {
            // Slice 5b: external Lightning payment via CLN `pay`.
            external_pay(
                &state,
                auth.user_id,
                &payment_hash,
                &req.invoice,
                &memo,
                &destination,
                amount_msat,
            )
            .await
        }
    }
}

// =====================================================================
// External pay (slice 5b)
// =====================================================================

/// Fee buffer above the requested amount. The handler reserves this
/// up-front; whatever CLN actually pays in fees gets locked in on
/// settle, and the difference is refunded to the user.
///
/// Heuristic: max(1% of amount, 5 sat). Tuned to comfortably cover
/// CLN's default `maxfeepercent=0.5` + `exemptfee=5000msat` routing
/// budget. Bigger buffers mean fewer rejections; smaller buffers mean
/// less locked balance during in-flight pays.
fn compute_fee_reserve_msat(amount_msat: i64) -> i64 {
    std::cmp::max(amount_msat / 100, 5_000)
}

/// CLN `pay` RPC error codes we treat as IN-FLIGHT rather than terminal.
/// HTLCs may still be live, so we MUST NOT refund — the reconciler
/// will resolve the real state via `listpays`.
///
///   200 PAY_IN_PROGRESS         — a previous `pay` for this hash is still active
///   210 PAY_STOPPED_RETRYING    — retry_for window expired mid-attempt
///   211 PAY_STATUS_UNEXPECTED   — CLN can't determine state
fn is_in_flight_error(code: i64) -> bool {
    matches!(code, 200 | 210 | 211)
}

async fn external_pay(
    state: &Arc<AppState>,
    user_id: i64,
    payment_hash: &str,
    bolt11: &str,
    memo: &str,
    destination: &str,
    amount_msat: i64,
) -> Result<Json<Value>, AppError> {
    let fee_reserve_msat = compute_fee_reserve_msat(amount_msat);

    // ---- 1. Reserve (atomic: balance check + pending row + ledger debit). ----
    use db::ReserveResult::*;
    match db::reserve_external_pay(
        &state.db,
        user_id,
        payment_hash,
        bolt11,
        memo,
        amount_msat,
        fee_reserve_msat,
    )
    .await?
    {
        Reserved => {}
        InsufficientBalance {
            balance_msat,
            required_msat,
        } => {
            return Err(AppError::payment(
                5,
                format!(
                    "not enough balance: have {} msat, need {} msat (incl. fee reserve)",
                    balance_msat, required_msat
                ),
            ))
        }
        AlreadyAttempted => {
            return Err(AppError::payment(
                7,
                "a payment to this invoice is already in progress or completed",
            ))
        }
    }

    // ---- 2. Call CLN `pay`. ----
    //
    // Blocks (possibly tens of seconds) while CLN tries routes.
    // Default retry_for is 60s; we leave it at the default so any
    // error returned is terminal (or one of the in-flight codes we
    // explicitly recognise).
    let pay_result = cln::call_strict(
        &state.rpc_path,
        "pay",
        json!({
            "bolt11": bolt11,
        }),
    )
    .await;

    // ---- 3. Resolve (atomic settle or fail+refund). ----
    match pay_result {
        Ok(resp) => {
            // CLN pay returned terminal-success: a complete payment
            // with preimage.
            let preimage = resp["payment_preimage"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let sent_msat = cln::parse_msat(&resp["amount_sent_msat"])
                .map(|n| n as i64)
                .unwrap_or(amount_msat);
            let actual_fee_msat = (sent_msat - amount_msat).max(0);

            db::settle_external_pay(
                &state.db,
                user_id,
                payment_hash,
                &preimage,
                actual_fee_msat,
                fee_reserve_msat,
            )
            .await?;

            log::info!(
                "external pay settled: user={} hash={} amount={}msat fee={}msat",
                user_id,
                payment_hash,
                amount_msat,
                actual_fee_msat
            );

            Ok(Json(payment_response(
                destination,
                payment_hash,
                amount_msat,
                actual_fee_msat,
                memo,
                Some(&preimage),
            )))
        }

        Err(cln::CallErr::Rpc { code, message, .. }) if is_in_flight_error(code) => {
            // HTLCs may still be live. Leave the row `external_pending`
            // and rely on the reconciler to settle / fail later.
            log::warn!(
                "external pay in flight (code {}): user={} hash={} — leaving pending, reconciler will resolve",
                code, user_id, payment_hash
            );
            Err(AppError::payment(
                6,
                format!(
                    "payment in progress (CLN code {}: {}); check /gettxs shortly",
                    code, message
                ),
            ))
        }

        Err(cln::CallErr::Rpc { code, message, .. }) => {
            // Terminal failure. Refund the user.
            log::warn!(
                "external pay failed (code {}): user={} hash={} — refunding",
                code, user_id, payment_hash
            );
            db::fail_external_pay(
                &state.db,
                user_id,
                payment_hash,
                amount_msat,
                fee_reserve_msat,
            )
            .await?;
            Err(AppError::payment(
                6,
                format!("payment failed (CLN code {}: {})", code, message),
            ))
        }

        Err(cln::CallErr::Transport(e)) => {
            // Couldn't talk to lightningd or the response was
            // unparseable. We don't know the payment's real state, so
            // leave it pending. The reconciler retries.
            log::error!(
                "external pay transport error: user={} hash={}: {:#} — leaving pending",
                user_id, payment_hash, e
            );
            Err(AppError::payment(
                6,
                "transport error talking to lightningd; check /gettxs shortly",
            ))
        }
    }
}

/// Build the LndHub-shaped /payinvoice success response.
///
/// For internal payments there's no real Lightning preimage (the CLN
/// invoice on this node is never actually settled by the network);
/// we pass `None` and the response uses all-zero hex as a clearly-
/// placeholder value. For external payments we pass `Some(<hex>)`
/// from CLN's `pay` response.
fn payment_response(
    destination: &str,
    payment_hash: &str,
    amount_msat: i64,
    fee_msat: i64,
    memo: &str,
    preimage: Option<&str>,
) -> Value {
    // `unwrap_or_else` so the 64-zero string is only allocated when
    // we actually need it (the closure runs lazily).
    let preimage_str = preimage
        .map(|s| s.to_string())
        .unwrap_or_else(|| "0".repeat(64));

    json!({
        "payment_error": "",
        "payment_preimage": preimage_str,
        "payment_route": {
            "total_amt": amount_msat / 1000,
            "total_fees": fee_msat / 1000,
            "total_amt_msat": amount_msat,
            "total_fees_msat": fee_msat,
        },
        "decoded": {
            "destination": destination,
            "payment_hash": payment_hash,
            "num_satoshis": (amount_msat / 1000).to_string(),
            "num_msat": amount_msat.to_string(),
            "description": memo,
        },
    })
}

/// Parse the optional `amount` field on /payinvoice (in **sats**)
/// to msat. Accepts number, string of digits, or null/absent.
fn parse_amount_sats_to_msat(v: &Value) -> Result<Option<i64>, AppError> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => n
            .as_u64()
            .map(|sats| Some((sats as i64).saturating_mul(1000)))
            .ok_or_else(|| AppError::bad_request("amount must be a positive integer")),
        Value::String(s) => {
            if s.is_empty() {
                return Ok(None);
            }
            let sats: u64 = s
                .parse()
                .map_err(|_| AppError::bad_request("amount must be a positive integer"))?;
            Ok(Some((sats as i64).saturating_mul(1000)))
        }
        _ => Err(AppError::bad_request(
            "amount must be a number or string of digits",
        )),
    }
}

// =====================================================================
// /getbalance
// =====================================================================

/// `GET /getbalance` — return the user's current balance in the
/// classic LndHub envelope.
///
/// LndHub format:
///     {"BTC": {"AvailableBalance": <sats>}}
///
/// Sat-denominated, integer truncation. We also include a slightly
/// more useful `_msat` companion field in case any client prefers
/// the full precision.
pub(super) async fn getbalance(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<Value>, AppError> {
    let msat = db::balance_msat(&state.db, auth.user_id).await?;
    Ok(Json(json!({
        "BTC": {
            "AvailableBalance": msat / 1000,
            "AvailableBalanceMsat": msat,
        }
    })))
}

// =====================================================================
// /getpending
// =====================================================================

/// `GET /getpending` — list pending on-chain deposits.
///
/// We don't yet credit on-chain deposits (the polling task that
/// would watch CLN's `listfunds` and call `db::ledger::credit` is
/// a future slice). Returning `[]` keeps stricter LndHub clients
/// happy at session start without misrepresenting state.
pub(super) async fn getpending(_auth: AuthUser) -> Json<Value> {
    Json(Value::Array(vec![]))
}

// =====================================================================
// /getbtc
// =====================================================================

/// `GET /getbtc` — return the authenticated user's on-chain deposit
/// address.
///
/// Behaviour:
///   - First call: ask CLN for a fresh bech32 address via `newaddr`,
///     store it in our `addresses` table keyed by user_id.
///   - Subsequent calls: return the same persisted address.
///
/// Response shape (matching original LndHub):
///     [{"address": "bc1q..."}]
///
/// **Known limitation as of slice 5d**: deposits TO this address land
/// in CLN's on-chain wallet but are NOT yet credited to the user's
/// internal balance. A future slice will add a polling task that
/// watches `lightning-cli listfunds` for new confirmed outputs to
/// our addresses and writes corresponding `ledger` rows.
pub(super) async fn getbtc(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<Value>, AppError> {
    // Already minted?
    if let Some(addr) = db::addresses::for_user(&state.db, auth.user_id).await? {
        return Ok(Json(json!([{ "address": addr }])));
    }

    // Otherwise mint a fresh one. CLN's `newaddr` defaults to bech32.
    let resp = cln::call(&state.rpc_path, "newaddr", json!({"addresstype": "bech32"})).await?;

    let address = resp
        .get("bech32")
        .and_then(|v| v.as_str())
        .or_else(|| resp.get("address").and_then(|v| v.as_str()))
        .ok_or_else(|| {
            AppError::internal(anyhow::anyhow!(
                "lightningd `newaddr` response missing bech32: {:?}",
                resp
            ))
        })?
        .to_string();

    // Persist. If two concurrent /getbtc calls race, one will fail
    // the UNIQUE PRIMARY KEY constraint — fall back to a re-read so
    // both callers still get a valid address.
    if let Err(e) = db::addresses::create(&state.db, auth.user_id, &address).await {
        log::debug!(
            "addresses::create failed for user {} (likely race): {}",
            auth.user_id,
            e
        );
        if let Some(existing) = db::addresses::for_user(&state.db, auth.user_id).await? {
            return Ok(Json(json!([{ "address": existing }])));
        }
        return Err(AppError::internal(e));
    }

    log::info!(
        "minted deposit address for user {}: {} (note: deposits not yet auto-credited)",
        auth.user_id,
        address
    );

    Ok(Json(json!([{ "address": address }])))
}

// =====================================================================
// /gettxs
// =====================================================================

/// `GET /gettxs` — outbound payment history (LndHub semantics:
/// /getuserinvoices for incoming, /gettxs for outgoing).
pub(super) async fn gettxs(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<Value>, AppError> {
    let rows = db::payments::list_for_user(&state.db, auth.user_id).await?;

    let arr: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "type": "paid_invoice",
                "fee": r.fee_msat / 1000,
                "fee_msat": r.fee_msat,
                "value": r.amount_msat / 1000,
                "value_msat": r.amount_msat,
                "timestamp": r.created_at,
                "memo": r.memo,
                "payment_preimage": r.preimage.unwrap_or_else(|| "0".repeat(64)),
                "payment_hash": r.payment_hash,
                "payment_request": r.bolt11,
                "status": r.status,
                "settled_at": r.settled_at,
            })
        })
        .collect();

    Ok(Json(Value::Array(arr)))
}
