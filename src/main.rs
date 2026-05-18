// src/main.rs
//
// Entry point for the cln-hub plugin.
//
// Lifecycle:
//   1. `lightningd` starts cln-hub as a child process when it boots.
//   2. We declare options + subscriptions, then call `.configure()`
//      to perform the first half of the handshake. This gives us a
//      `ConfiguredPlugin` from which we can read option values and
//      `lightning_dir`/`rpc_file`/`network` — we need them BEFORE
//      we can build `AppState`, hence the two-phase pattern.
//   3. Build `AppState { rpc_path, db }` (opens SQLite, runs
//      migrations) and wrap it in `Arc`.
//   4. Pass that `Arc<AppState>` to `.start()` to finalise the
//      handshake. From this point on, the plugin's notification
//      callbacks (e.g. `invoice_payment`) can reach `AppState`
//      via `plugin.state()`.
//   5. Spawn the HTTP server (axum) on the configured port,
//      sharing the same `Arc<AppState>` so handlers see the same
//      database that notification callbacks write to.
//   6. `.join()` blocks until lightningd closes our stdin
//      (shutdown / unload).

mod cln;
mod db;
mod http;
mod plugin;
mod state;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use cln_plugin::{options::DefaultStringConfigOption, Builder};

use crate::http::ratelimit::RateLimiter;
use crate::state::AppState;

// =====================================================================
// Plugin options
// =====================================================================

const BIND_OPTION: DefaultStringConfigOption = DefaultStringConfigOption::new_str_with_default(
    "cln-hub-bind",
    "127.0.0.1:3000",
    "host:port for the LndHub HTTP API server. Defaults to localhost:3000. \
     Bind to 0.0.0.0 only behind a TLS-terminating reverse proxy.",
);

const DB_OPTION: DefaultStringConfigOption = DefaultStringConfigOption::new_str_with_default(
    "cln-hub-db",
    "",
    "filesystem path to cln-hub's SQLite database. \
     Defaults to <lightning-dir>/cln-hub.db.",
);

#[tokio::main]
async fn main() -> Result<()> {
    // ---- Phase 1: declare + configure (read options) ----
    //
    // `configure()` is the first half of the plugin init handshake:
    // it negotiates the manifest with lightningd and absorbs the
    // option values lightningd is forwarding to us, but does NOT
    // commit a state object yet. That's by design — we need the
    // option values to build the state.
    let configured = Builder::new(tokio::io::stdin(), tokio::io::stdout())
        .option(BIND_OPTION)
        .option(DB_OPTION)
        .subscribe("invoice_payment", plugin::invoice_payment)
        .dynamic()
        .configure()
        .await?;

    let Some(configured) = configured else {
        return Ok(()); // info-only invocation; exit cleanly.
    };

    // ---- Read configuration ----
    let bind_str: String = configured.option(&BIND_OPTION)?;
    let db_str: String = configured.option(&DB_OPTION)?;

    let conf = configured.configuration();
    let rpc_path = PathBuf::from(&conf.lightning_dir).join(&conf.rpc_file);
    let db_path = if db_str.is_empty() {
        PathBuf::from(&conf.lightning_dir).join("cln-hub.db")
    } else {
        PathBuf::from(db_str)
    };

    log::info!(
        "cln-hub starting on network={}, lightning-rpc={:?}, db={:?}",
        conf.network,
        rpc_path,
        db_path,
    );

    // ---- Open database (creates + migrates if needed) ----
    let pool = db::init(&db_path).await?;
    log::info!("cln-hub database ready at {:?}", db_path);

    // ---- Build shared state ----
    //
    // `Arc::new(AppState { ... })` heap-allocates the state with a
    // refcount of 1. `Arc::clone(&state)` bumps the count so we can
    // hand a copy each to: the HTTP router AND the plugin runtime.
    // Both share the SAME inner data — there's only one database
    // pool, one rpc_path, etc.
    let state = Arc::new(AppState {
        rpc_path,
        db: pool,
    });

    // Rate-limit buckets. Kept as standalone `Arc`s (rather than
    // fields on AppState) because they're only consumed by the
    // rate-limit middleware, which uses them as its own state via
    // `from_fn_with_state`. Defaults (per remote IP):
    //   - /create : burst 5, then 5/min sustained
    //   - /auth   : burst 10, then 30/min sustained (legitimate
    //               clients refresh tokens fairly often)
    let create_limiter = Arc::new(RateLimiter::new(5, 5));
    let auth_limiter = Arc::new(RateLimiter::new(10, 30));

    // ---- Bind HTTP listener ----
    let listener = tokio::net::TcpListener::bind(&bind_str).await?;
    log::info!("cln-hub HTTP listening on {}", bind_str);

    let router = http::router(Arc::clone(&state), create_limiter, auth_limiter);

    // Run the HTTP server concurrently with the plugin loop.
    //
    // `into_make_service_with_connect_info::<SocketAddr>()` is what
    // makes the `ConnectInfo<SocketAddr>` extractor work in our rate-
    // limit middleware. Without it, axum has no way to surface the
    // peer's IP to handlers.
    tokio::spawn(async move {
        let make_svc = router.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, make_svc).await {
            log::error!("HTTP server crashed: {}", e);
        }
    });

    // ---- Periodic token cleanup ----
    //
    // Every hour, delete `tokens` rows where the refresh half has
    // expired (created_at older than 31 days). These rows already
    // can't authenticate anything — TTL is enforced at lookup time —
    // but a long-running busy hub would accumulate rows forever
    // without this. We do it as a fire-and-forget tokio task; it
    // dies along with the runtime when lightningd shuts us down.
    //
    // === Rust note: `tokio::spawn` ownership ===
    //
    // `state` is an `Arc<AppState>`, so `Arc::clone(&state)` is
    // cheap — just bumps the refcount. The cloned Arc moves into
    // the async task and lives as long as the task does.
    let cleanup_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        // First tick fires immediately; consume it so we wait an hour
        // before the first cleanup pass.
        interval.tick().await;
        loop {
            interval.tick().await;
            match db::tokens::cleanup_expired(&cleanup_state.db).await {
                Ok(0) => log::debug!("token cleanup: 0 expired rows"),
                Ok(n) => log::info!("token cleanup: removed {} expired rows", n),
                Err(e) => log::warn!("token cleanup failed: {}", e),
            }
        }
    });

    // ---- On-chain deposit watcher ----
    //
    // Polls `lightning-cli listfunds` periodically; any confirmed
    // UTXO sent to an address we minted via /getbtc gets credited
    // to the owning user. See `plugin::deposit_watcher` for details.
    let watcher_state = Arc::clone(&state);
    tokio::spawn(plugin::deposit_watcher(watcher_state));

    // ---- External-payment reconciler ----
    //
    // Resolves any `external_pending` payments — either from a crash
    // during the synchronous /payinvoice flow, or from CLN responses
    // that came back with an "in-flight" error code (200/210/211).
    // The reconciler does one synchronous startup pass + a 60s
    // periodic loop. See `plugin::payment_reconciler` for details.
    let reconciler_state = Arc::clone(&state);
    tokio::spawn(plugin::payment_reconciler(reconciler_state));

    // ---- Phase 2: start (commit the state, begin event loop) ----
    //
    // After this, lightningd considers us "running" and can deliver
    // notifications. The plugin runtime stores our `Arc<AppState>`
    // and hands a reference to each notification callback via
    // `plugin.state()` (see plugin.rs).
    let plugin = configured.start(state).await?;

    // Block until lightningd shuts us down.
    plugin.join().await?;
    Ok(())
}
