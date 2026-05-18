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

    /// Minimum on-chain confirmations before the deposit watcher
    /// credits a UTXO to the owning user's ledger. Configured via
    /// the `cln-hub-min-deposit-confs` plugin option (default 2).
    ///
    /// Setting this to 1 mirrors CLN's own "confirmed" semantics
    /// (fast UX, exposed to single-block reorgs). Setting it to
    /// 3+ matches typical LndHub-fork policy.
    pub min_deposit_confs: i64,
}
