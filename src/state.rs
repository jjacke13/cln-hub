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
    /// the `cln-hub-min-deposit-confs` plugin option (default 6 —
    /// the industry-custody norm for mainnet).
    ///
    /// Lower values trade UX latency for reorg exposure: 1 mirrors
    /// CLN's own "confirmed" semantics but credits before a single
    /// block can be re-orged out; 2–3 sit near typical LndHub-fork
    /// policy. Anything below 3 emits a startup WARN. Regtest /
    /// experiment networks routinely use 1–2 — fine, no real funds
    /// at risk.
    pub min_deposit_confs: i64,
}
