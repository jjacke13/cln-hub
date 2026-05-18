// src/http/mod.rs
//
// HTTP layer entry point. Owns:
//   - the `router()` builder that wires every endpoint
//   - `AppError`        — our error type, with LndHub-shaped JSON body
//   - `AuthUser`        — the "must be signed in" extractor
//
// The handlers themselves live in topical submodules:
//   - info     : /getinfo, /decodeinvoice, /checkpayment
//   - auth     : /create, /auth
//   - invoice  : /addinvoice, /getuserinvoices
//   - payment  : /payinvoice, /getbalance, /gettxs
//
// Splitting up vs leaving as one file: each submodule stays under
// ~150 lines, the router gives a one-glance view of the whole API
// surface, and adding new endpoints later doesn't bloat any one file.
//
// === Rust note: `pub(crate) mod` vs `mod` ===
//
// Submodule declarations here use plain `mod` because they're only
// referenced from within this `http` module — sibling files don't
// need them. `pub use` re-exports below make `AppError` and `AuthUser`
// visible from outside `http` (so e.g. `crate::http::AppError`
// works).

mod auth;
mod info;
mod invoice;
mod payment;
pub mod ratelimit;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    async_trait,
    extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Request, State},
    http::{request::Parts, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use crate::cln;
use crate::db;
use crate::state::AppState;

// =====================================================================
// Router
// =====================================================================

/// Build the full HTTP router. Every route in the API is listed here
/// in one place so the API surface is at-a-glance auditable.
///
/// `create_limiter` and `auth_limiter` are passed in (rather than
/// constructed here) so main.rs owns the lifetimes and so future
/// admin endpoints could inspect or reset them.
///
/// === Rust note: `MethodRouter::layer` ===
///
/// Calling `.layer(LAYER)` on a `MethodRouter` (the value returned
/// by `post(...)` / `get(...)`) wraps just that one route's handler.
/// Calling `.layer(LAYER)` on the `Router` itself wraps every route.
/// We use the per-route form for rate-limit middleware so only
/// `/create` and `/auth` are throttled; read-only endpoints don't
/// need it.
///
/// === Rust note: `from_fn_with_state` vs `from_fn` ===
///
/// `from_fn_with_state(s, f)` baked-in state `s` lives alongside
/// `f`'s extractors — the middleware can pull `State<typeof(s)>`.
/// We use it here because the middleware needs its own state (the
/// `Arc<RateLimiter>`) that's separate from the route handler's
/// `Arc<AppState>`. `from_fn` (no state) wouldn't compose with
/// `with_state` in the way we want.
pub fn router(
    state: Arc<AppState>,
    create_limiter: Arc<ratelimit::RateLimiter>,
    auth_limiter: Arc<ratelimit::RateLimiter>,
) -> Router {
    Router::new()
        // ---- Manifest / liveness ----
        .route("/", get(root))
        .route("/version", get(root))
        // ---- Public ----
        .route("/getinfo", get(getinfo))
        .route("/decodeinvoice", post(info::decodeinvoice))
        .route("/checkpayment/:hash", get(info::checkpayment))
        // /create and /auth get per-IP rate limiting.
        .route(
            "/create",
            post(auth::create)
                .layer(middleware::from_fn_with_state(create_limiter, rate_limit)),
        )
        .route(
            "/auth",
            post(auth::auth)
                .layer(middleware::from_fn_with_state(auth_limiter, rate_limit)),
        )
        // ---- Authenticated ----
        .route("/addinvoice", post(invoice::addinvoice))
        .route("/getuserinvoices", get(invoice::getuserinvoices))
        .route("/payinvoice", post(payment::payinvoice))
        .route("/getbalance", get(payment::getbalance))
        // `/balance` is an older LndHub alias some clients still send.
        .route("/balance", get(payment::getbalance))
        .route("/gettxs", get(payment::gettxs))
        // `/getpending` lists pending on-chain deposits. We don't
        // accept on-chain yet, so always empty — but having it return
        // 200+`[]` rather than 404 keeps stricter clients happy.
        .route("/getpending", get(payment::getpending))
        // `/getbtc` returns the user's on-chain deposit address
        // (minted on first call, persistent after).
        .route("/getbtc", get(payment::getbtc))
        // Single state arc shared by every handler.
        .with_state(state)
        // Request logger sits OUTSIDE `.with_state` so it logs every
        // request (including the 404s for routes we don't have yet).
        .layer(middleware::from_fn(log_request))
        // Global request-body cap. axum's default is 2 MB which is
        // wildly above what any LndHub-shaped JSON request needs
        // (a few KB at worst). Tightening it to 64 KB removes a
        // cheap memory-pressure amplifier from a misbehaving client.
        // The /payinvoice + /decodeinvoice handlers separately
        // bounds the inbound `invoice` string at 4 KB.
        .layer(DefaultBodyLimit::max(64 * 1024))
        // Anything we *don't* explicitly route falls through to here.
        // We log the path so unknown clients tell us what they tried.
        .fallback(unknown_route)
}

// =====================================================================
// Manifest / fallback handlers
// =====================================================================

/// `GET /` and `GET /version` — return a small manifest blob. Some
/// LndHub clients ping these to check liveness.
async fn root() -> Json<Value> {
    Json(json!({
        "name": "cln-hub",
        "version": env!("CARGO_PKG_VERSION"),
        "node": "core-lightning",
    }))
}

/// Default 404 handler with a structured body so any client that
/// tries to display the error body sees something readable.
async fn unknown_route(req: Request) -> Response {
    let path = req.uri().path().to_string();
    log::warn!("404: {} {}", req.method(), path);
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": true,
            "code": 0,
            "message": format!("no such endpoint: {}", path),
        })),
    )
        .into_response()
}

// =====================================================================
// Request logging middleware
// =====================================================================

/// One log line per request: `HTTP <method> <path> -> <status>`.
///
/// === Rust note: `axum::middleware::from_fn` ===
///
/// axum lets you write middleware as a plain async fn that takes
/// the inbound `Request` plus a `Next` (which represents the rest
/// of the handler chain). Call `next.run(req).await` to invoke
/// downstream and get the `Response`; we capture the status code
/// before returning. This is the simplest middleware shape — no
/// trait impls, no wrappers.
async fn log_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    log::info!("HTTP {} {} -> {}", method, path, resp.status().as_u16());
    resp
}

// =====================================================================
// Rate-limit middleware
// =====================================================================
//
// One middleware, parameterised by which `RateLimiter` it consults
// (passed in via `from_fn_with_state` at the call site). The same
// function powers both /create and /auth — they get different
// limiter instances with different rates.
//
// === Rust note: `ConnectInfo<SocketAddr>` ===
//
// `ConnectInfo<T>` is an axum extractor that surfaces metadata the
// server attached at connection time. For TCP it carries the peer
// `SocketAddr`. To make it available, the server has to be started
// with `into_make_service_with_connect_info::<SocketAddr>()` (see
// main.rs). Without that wiring, this extractor returns 500.

async fn rate_limit(
    State(limiter): State<Arc<ratelimit::RateLimiter>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    if !limiter.try_acquire(addr.ip()) {
        return Err(AppError::too_many(
            "rate limit exceeded; try again later",
        ));
    }
    Ok(next.run(req).await)
}

/// `GET /getinfo` — passthrough to lightningd's `getinfo`.
///
/// Lives in `mod.rs` rather than `info.rs` because it's the canonical
/// example of "extract State, call CLN, return Json"; everything more
/// elaborate can be understood as variations on this skeleton.
async fn getinfo(State(state): State<Arc<AppState>>) -> Result<Json<Value>, AppError> {
    let info = cln::call(&state.rpc_path, "getinfo", json!({})).await?;
    Ok(Json(info))
}

// =====================================================================
// AuthUser extractor
// =====================================================================

/// Putting this in a handler's signature means "must arrive with a
/// valid `Authorization: Bearer <access_token>`; otherwise reject
/// with our 401 before the handler body runs."
pub struct AuthUser {
    pub user_id: i64,
}

#[async_trait]
impl FromRequestParts<Arc<AppState>> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("authorization")
            .ok_or_else(|| AppError::auth("missing Authorization header"))?
            .to_str()
            .map_err(|_| AppError::auth("invalid Authorization header bytes"))?;

        let token = header
            .strip_prefix("Bearer ")
            .ok_or_else(|| AppError::auth("Authorization scheme must be Bearer"))?;

        let user_id = db::tokens::user_id_for_access(&state.db, token)
            .await
            .map_err(AppError::internal)?
            .ok_or_else(|| AppError::auth("invalid or revoked token"))?;

        Ok(AuthUser { user_id })
    }
}

// =====================================================================
// Errors
// =====================================================================

/// Application-level error type. Each variant maps to a specific
/// HTTP status and LndHub error code.
pub enum AppError {
    /// 400 Bad Request — malformed input.
    BadRequest(String),
    /// 401 Unauthorized — bad creds, missing token, etc.
    Auth(String),
    /// 402 Payment-shaped errors (insufficient balance, bad invoice,
    /// already paid, etc.). LndHub uses HTTP 200 with code-in-body
    /// for these in some forks; we use 402 because it's HTTP-correct
    /// and clients still parse the body.
    Payment {
        code: i32, // LndHub error code (5 = no balance, 7 = bad invoice, ...)
        message: String,
    },
    /// 429 Too Many Requests — rate-limit refusal.
    TooMany(String),
    /// 500 Internal Server Error — anything we'd rather not show.
    Internal(anyhow::Error),
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn auth(msg: impl Into<String>) -> Self {
        Self::Auth(msg.into())
    }
    pub fn payment(code: i32, msg: impl Into<String>) -> Self {
        Self::Payment {
            code,
            message: msg.into(),
        }
    }
    pub fn too_many(msg: impl Into<String>) -> Self {
        Self::TooMany(msg.into())
    }
    pub fn internal(err: impl Into<anyhow::Error>) -> Self {
        Self::Internal(err.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, 0, m),
            AppError::Auth(m) => (StatusCode::UNAUTHORIZED, 1, m),
            AppError::Payment { code, message } => (StatusCode::PAYMENT_REQUIRED, code, message),
            // LndHub error code 9 is the conventional "too many requests".
            AppError::TooMany(m) => (StatusCode::TOO_MANY_REQUESTS, 9, m),
            AppError::Internal(e) => {
                log::error!("internal error: {:#}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    0,
                    "internal server error".to_string(),
                )
            }
        };

        let body = Json(json!({
            "error": true,
            "code": code,
            "message": message,
        }));
        (status, body).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        AppError::Internal(err.into())
    }
}
