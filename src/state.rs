// src/state.rs
//
// Shared application state. Wrapped in `Arc<AppState>` and handed to:
//
//   - the axum HTTP router  (via `.with_state(state)`); each handler
//     receives it through axum's `State<Arc<AppState>>` extractor.
//   - the cln_plugin runtime (via `Builder::start(state)`); each
//     subscription/hook callback receives it through `plugin.state()`.

use std::path::PathBuf;

use crate::db;

pub struct AppState {
    /// Filesystem path to lightningd's RPC socket (lightning-rpc).
    pub rpc_path: PathBuf,

    /// SQLite connection pool. `db::Pool` is `Arc`-internal, so
    /// we don't need another `Arc` layer around it.
    pub db: db::Pool,
}
