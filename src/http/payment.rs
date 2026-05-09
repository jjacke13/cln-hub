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
            Ok(Json(payment_response(
                &destination,
                &payment_hash,
                amount_msat,
                0,
                &memo,
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
            // Slice 5b will wire the CLN `pay` call here.
            Err(AppError::payment(
                6,
                "external payments are not yet wired (slice 5b — needs channels)",
            ))
        }
    }
}

/// Build the LndHub-shaped /payinvoice success response.
///
/// Internal payments have no real Lightning preimage — the CLN
/// invoice on this node is never actually settled by the network.
/// We return all-zero hex as a clearly-placeholder preimage; clients
/// that don't verify sha256(preimage)==payment_hash treat it as
/// success, and clients that do can be told to skip the check for
/// internal payments.
fn payment_response(
    destination: &str,
    payment_hash: &str,
    amount_msat: i64,
    fee_msat: i64,
    memo: &str,
) -> Value {
    json!({
        "payment_error": "",
        "payment_preimage": "0".repeat(64),  // 32 zero bytes hex-encoded
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
