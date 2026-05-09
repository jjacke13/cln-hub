// src/http/auth.rs
//
// Public auth endpoints: /create and /auth. Match the original
// LndHub semantics (random login+password, opaque server-side
// access/refresh tokens, additive rotation).

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::state::AppState;

use super::AppError;

// =====================================================================
// /create
// =====================================================================

#[derive(Serialize)]
pub(super) struct CreateResp {
    login: String,
    password: String,
}

/// `POST /create` — issue a fresh LndHub account. We accept (and
/// ignore) the `{"partnerid": ..., "accounttype": ...}` body that
/// BlueWallet/Zeus send.
pub(super) async fn create(
    State(state): State<Arc<AppState>>,
) -> Result<Json<CreateResp>, AppError> {
    let login = db::random_hex(10);
    let password = db::random_hex(10);
    db::users::create(&state.db, &login, &password).await?;
    Ok(Json(CreateResp { login, password }))
}

// =====================================================================
// /auth
// =====================================================================

#[derive(Deserialize)]
pub(super) struct AuthReq {
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Serialize)]
pub(super) struct AuthResp {
    access_token: String,
    refresh_token: String,
}

/// `POST /auth` — exchange creds OR a refresh_token for a new pair.
/// Old tokens stay valid (LndHub semantics — additive rotation).
pub(super) async fn auth(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthReq>,
) -> Result<Json<AuthResp>, AppError> {
    let user_id = match (req.login, req.password, req.refresh_token) {
        (Some(login), Some(password), _) => db::users::verify(&state.db, &login, &password)
            .await?
            .ok_or_else(|| AppError::auth("bad authentication"))?,

        (None, None, Some(refresh)) => db::tokens::user_id_for_refresh(&state.db, &refresh)
            .await?
            .ok_or_else(|| AppError::auth("bad refresh token"))?,

        _ => {
            return Err(AppError::bad_request(
                "provide either login+password or refresh_token",
            ))
        }
    };

    let (access_token, refresh_token) = db::tokens::create(&state.db, user_id).await?;
    Ok(Json(AuthResp {
        access_token,
        refresh_token,
    }))
}
