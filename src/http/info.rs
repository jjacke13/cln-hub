// src/http/info.rs
//
// Read-only / informational endpoints:
//   - /decodeinvoice          — passthrough to CLN's `decode`
//   - /checkpayment/:hash     — has the local invoice settled yet?

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cln;
use crate::state::AppState;

use super::AppError;

// =====================================================================
// /decodeinvoice
// =====================================================================

#[derive(Deserialize)]
pub(super) struct DecodeReq {
    invoice: String,
}

/// `POST /decodeinvoice` — decode a BOLT11 string by asking CLN.
///
/// Returns CLN's decoded body, plus a small set of LndHub-flavoured
/// aliased fields (`destination`, `num_satoshis`, `num_msat`,
/// `timestamp`) so existing wallet apps see the keys they expect.
pub(super) async fn decodeinvoice(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DecodeReq>,
) -> Result<Json<Value>, AppError> {
    // Hard cap inbound BOLT11 length before relaying it to CLN's
    // unix-socket RPC. Keeps a malicious client from shoving a 2 MB
    // request straight into lightningd's single-threaded RPC loop.
    if req.invoice.len() > 4096 {
        return Err(AppError::bad_request("invoice too long"));
    }

    let resp = cln::call(
        &state.rpc_path,
        "decode",
        json!({"string": req.invoice}),
    )
    .await?;

    if resp.get("valid").and_then(|v| v.as_bool()) == Some(false) {
        return Err(AppError::bad_request("could not decode payment request"));
    }

    // Build a derived shape with LndHub-named aliases. We mutate a
    // clone of CLN's response so callers still get every original
    // field (newer CLN versions add more).
    let mut out = resp.clone();
    if let Some(payee) = resp.get("payee").and_then(|v| v.as_str()) {
        out["destination"] = json!(payee);
    }
    // `amount_msat` may be a JSON integer (newer CLN) or a `"<n>msat"`
    // string (older CLN). `cln::parse_msat` handles both shapes — a
    // bare `as_i64` would silently report amountless on the string
    // form.
    if let Some(amount_msat) = resp.get("amount_msat").and_then(cln::parse_msat) {
        let amount_msat = amount_msat as i64;
        out["num_satoshis"] = json!((amount_msat / 1000).to_string());
        out["num_msat"] = json!(amount_msat.to_string());
    }
    if let Some(created_at) = resp.get("created_at").and_then(|v| v.as_i64()) {
        out["timestamp"] = json!(created_at.to_string());
    }
    if let Some(expiry) = resp.get("expiry").and_then(|v| v.as_i64()) {
        out["expire_time"] = json!(expiry);
    }

    Ok(Json(out))
}

// =====================================================================
// /checkpayment/:hash
// =====================================================================

/// `GET /checkpayment/:hash` — has the invoice with this payment_hash
/// (which we issued) been paid yet?
///
/// LndHub returns `{"paid": <bool>}`.
///
/// === Rust note: `Path<T>` ===
///
/// `Path<String>` is axum's URL-parameter extractor. The `:hash`
/// placeholder in the route declaration matches whatever's in the
/// URL after `/checkpayment/`, and axum hands it to us as a `String`.
/// `Path((a, b))` works for multi-segment routes like `/foo/:a/bar/:b`.
pub(super) async fn checkpayment(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<Json<Value>, AppError> {
    // We re-use the by-label lookup helper would be wrong (label !=
    // payment_hash). Inline the by-payment_hash query here — it's a
    // single statement.
    let row: Option<(Option<i64>,)> =
        sqlx::query_as("SELECT settled_at FROM invoices WHERE payment_hash = ?")
            .bind(&hash)
            .fetch_optional(&state.db)
            .await
            .map_err(AppError::internal)?;

    let paid = row.and_then(|(s,)| s).is_some();
    Ok(Json(json!({ "paid": paid })))
}
