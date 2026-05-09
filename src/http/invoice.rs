// src/http/invoice.rs
//
// Invoice-related authenticated endpoints:
//   - POST /addinvoice         — issue a fresh BOLT11 for the user
//   - GET  /getuserinvoices    — list the user's invoices, newest first

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cln;
use crate::db;
use crate::state::AppState;

use super::{AppError, AuthUser};

// =====================================================================
// /addinvoice
// =====================================================================

#[derive(Deserialize)]
pub(super) struct AddInvoiceReq {
    /// We accept this as a `Value` and parse it ourselves so both
    /// `{"amt": 100}` and `{"amt": "100"}` work — BlueWallet sends
    /// the latter.
    #[serde(default)]
    amt: Value,
    #[serde(default)]
    memo: String,
}

#[derive(Serialize)]
pub(super) struct AddInvoiceResp {
    r_hash: String,
    payment_request: String,
    pay_req: String,
    add_index: String,
}

/// `POST /addinvoice` — create a BOLT11 invoice on this CLN node
/// and persist it as belonging to the authenticated user.
pub(super) async fn addinvoice(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
    Json(req): Json<AddInvoiceReq>,
) -> Result<Json<AddInvoiceResp>, AppError> {
    // Parse `amt` (number or string of digits).
    let amt: u64 = match &req.amt {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| AppError::bad_request("amt must be a positive integer"))?,
        Value::String(s) => s
            .parse()
            .map_err(|_| AppError::bad_request("amt must be a positive integer"))?,
        Value::Null => return Err(AppError::bad_request("amt is required")),
        _ => return Err(AppError::bad_request("amt must be a number or string")),
    };
    if amt == 0 {
        return Err(AppError::bad_request("amt must be > 0"));
    }
    let amount_msat: i64 = (amt as i64).saturating_mul(1000);

    // Generate a unique label so the invoice_payment notification
    // can route the settle event back to this user.
    let label = format!("cln-hub:{}", db::random_hex(8));

    // Ask CLN to mint the invoice.
    let resp = cln::call(
        &state.rpc_path,
        "invoice",
        json!({
            "amount_msat": amount_msat,
            "label": &label,
            "description": &req.memo,
        }),
    )
    .await?;

    let bolt11 = resp["bolt11"]
        .as_str()
        .ok_or_else(|| {
            AppError::internal(anyhow::anyhow!(
                "lightningd response missing bolt11: {:?}",
                resp
            ))
        })?
        .to_string();
    let payment_hash = resp["payment_hash"]
        .as_str()
        .ok_or_else(|| {
            AppError::internal(anyhow::anyhow!(
                "lightningd response missing payment_hash: {:?}",
                resp
            ))
        })?
        .to_string();
    let expires_at = resp["expires_at"].as_i64().unwrap_or(0);

    db::invoices::create(
        &state.db,
        &payment_hash,
        &label,
        auth.user_id,
        amount_msat,
        &req.memo,
        &bolt11,
        expires_at,
    )
    .await?;

    Ok(Json(AddInvoiceResp {
        r_hash: payment_hash,
        payment_request: bolt11.clone(),
        pay_req: bolt11,
        add_index: String::new(),
    }))
}

// =====================================================================
// /getuserinvoices
// =====================================================================

/// `GET /getuserinvoices` — list the authenticated user's invoices,
/// newest first, in LndHub's array shape.
pub(super) async fn getuserinvoices(
    State(state): State<Arc<AppState>>,
    auth: AuthUser,
) -> Result<Json<Value>, AppError> {
    let rows = db::invoices::list_for_user(&state.db, auth.user_id).await?;

    let arr: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "r_hash": r.payment_hash,
                "payment_request": r.bolt11,
                "ispaid": r.settled_at.is_some(),
                "type": "user_invoice",
                "amt": r.amount_msat / 1000,
                "amt_msat": r.amount_msat,
                "settled_amt_msat": r.settled_msat,
                "expire_time": r.expires_at - r.created_at,
                "timestamp": r.created_at,
                "description": r.memo,
            })
        })
        .collect();

    Ok(Json(Value::Array(arr)))
}
