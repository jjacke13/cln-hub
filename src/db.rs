// src/db.rs
//
// SQLite persistence layer. Owns:
//   - the connection pool (`Pool` = `sqlx::SqlitePool`)
//   - migration runner (auto-applies `migrations/*.sql` at startup)
//   - per-table CRUD helpers, grouped into submodules (`users`,
//     `tokens`) so the call sites read like `db::users::create(...)`.
//
// Pool, not connection: sqlx hands us a small pool of reusable
// connections under the hood. Cloning a `SqlitePool` is cheap (it's
// `Arc` internally), so we just include it in `AppState` and hand
// references to it from each handler.

use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// Public alias so the rest of the code refers to `db::Pool` rather
/// than the wordy fully-qualified `sqlx::SqlitePool`.
pub type Pool = SqlitePool;

/// Open (or create) the SQLite database at `path`, run any pending
/// migrations, and return the pool.
///
/// Called once at startup from main.rs.
pub async fn init(path: &Path) -> Result<Pool> {
    // === Rust note: `format!` ===
    // `format!("...{}...", x)` is the string-building macro — same
    // template syntax as `println!`/`log::info!`, but produces a
    // `String` instead of writing to stdout.
    let url = format!("sqlite://{}", path.display());

    // `from_str` here is the ConnectOptions parser. We then layer on
    // two important options:
    //   - `create_if_missing(true)` so first launch creates the DB.
    //   - `foreign_keys(true)` so SQLite actually enforces our
    //     `REFERENCES users(id) ON DELETE CASCADE` constraint —
    //     SQLite ignores foreign keys by default, a footgun.
    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(10)
        .connect_with(opts)
        .await?;

    // `sqlx::migrate!` is a macro that walks `./migrations/` at
    // **compile time**, embeds every `*.sql` into the binary, and
    // returns a `Migrator`. `.run(&pool)` applies any not-yet-applied
    // ones. sqlx tracks them in an internal `_sqlx_migrations` table.
    sqlx::migrate!("./migrations").run(&pool).await?;

    // Tighten file permissions on the DB. SQLite creates files with
    // the process umask, typically 0644 — world-readable. The DB
    // holds hashed (but still sensitive) credentials and the
    // append-only ledger; nothing else on the host needs to read it.
    // Best-effort: log on failure rather than abort, since on
    // exotic filesystems the chmod may be unsupported. Same
    // treatment for WAL / SHM sidecar files if they exist.
    #[cfg(unix)]
    secure_db_perms(path);

    Ok(pool)
}

/// Apply `0o600` (`rw-------`) permissions to the DB file and its
/// SQLite-managed sidecar files (`.db-wal`, `.db-shm`). Logs at
/// `warn` on failure but does not propagate the error — a missing
/// chmod is a hardening miss, not a reason to refuse to serve.
#[cfg(unix)]
fn secure_db_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let chmod = |p: &Path| {
        if !p.exists() {
            return;
        }
        match std::fs::metadata(p).and_then(|m| {
            let mut perms = m.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(p, perms)
        }) {
            Ok(()) => {}
            Err(e) => log::warn!("could not set 0600 on {:?}: {}", p, e),
        }
    };
    chmod(path);
    // sqlx's default journal mode is DELETE (no WAL files) but if
    // anyone flips it on later, harden those too — and SQLite's
    // rollback-journal `<db>-journal` lives in the same directory.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = parent.join(format!("{}{}", file_name, suffix));
        chmod(&sidecar);
    }
}

// =====================================================================
// users
// =====================================================================

/// Operations on the `users` table.
pub mod users {
    use super::{anyhow, unix_now, Pool, Result};

    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};

    /// Argon2id parameters used for password hashing.
    ///
    /// `Argon2::default()` in the `argon2` 0.5.x crate produces
    /// (m=19 MiB, t=2, p=1) — already an OWASP-2023-recommended
    /// preset. We pin the stronger alternative (m=46 MiB, t=1, p=1):
    /// one pass over a larger memory region. Wall-clock cost is
    /// similar to the default but the brute-force memory budget for
    /// an attacker is ~2.4x larger.
    ///
    /// Existing password hashes from earlier defaults keep validating
    /// because each PHC-encoded hash string carries its own params
    /// and `PasswordHash::new` reads them back at verify time.
    /// Re-hashing on next login is a future option; not done today.
    ///
    /// 46 MiB per concurrent /create or /auth call. /create is
    /// rate-limited to 5/min/IP and /auth to 30/min/IP, so peak
    /// memory pressure is bounded for any one client.
    const ARGON2_M_COST_KIB: u32 = 46 * 1024;
    const ARGON2_T_COST: u32 = 1;
    const ARGON2_P_COST: u32 = 1;

    /// Build the Argon2 hasher with the pinned params. Centralised so
    /// the create + verify paths can't drift apart.
    fn argon2() -> Argon2<'static> {
        // `Params::new` validates ranges; with the constants above
        // (well inside argon2's allowed bounds) it cannot fail. We
        // still `expect` rather than `unwrap` to give a clear panic
        // message if someone bumps the constants out of range later.
        let params = Params::new(ARGON2_M_COST_KIB, ARGON2_T_COST, ARGON2_P_COST, None)
            .expect("argon2 params out of range");
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    }

    /// Insert a new user. The plaintext password is hashed with
    /// argon2id (see ARGON2_* constants above).
    ///
    /// Returns the new user's id (SQLite rowid).
    pub async fn create(pool: &Pool, login: &str, password: &str) -> Result<i64> {
        let hash = hash_password(password)?;

        // === Rust note: `sqlx::query` ===
        //
        // `sqlx::query("...")` is the runtime-checked query builder.
        // `.bind(x)` adds a positional parameter (matches a `?` in the
        // SQL). This avoids string interpolation, which would be a
        // SQL-injection footgun.
        let result = sqlx::query(
            "INSERT INTO users (login, password_hash, created_at) VALUES (?, ?, ?)",
        )
        .bind(login)
        .bind(&hash)
        .bind(unix_now())
        .execute(pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    /// Verify a login/password pair. Returns:
    ///   - `Ok(Some(id))` on a match
    ///   - `Ok(None)` on either "no such login" or "wrong password"
    ///     (we collapse them to avoid leaking which one of the two
    ///     was wrong — common credential-stuffing mitigation)
    ///   - `Err(_)` on a database/cryptography error
    pub async fn verify(pool: &Pool, login: &str, password: &str) -> Result<Option<i64>> {
        // `query_as::<_, (i64, String)>` says "decode each row as a
        // tuple of these types". The `_` is the database type, which
        // sqlx infers from the pool — neat type-inference trick.
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT id, password_hash FROM users WHERE login = ?")
                .bind(login)
                .fetch_optional(pool)
                .await?;

        let Some((id, hash)) = row else {
            return Ok(None); // no such login
        };

        let parsed = PasswordHash::new(&hash)
            .map_err(|e| anyhow!("password hash parse failure: {}", e))?;

        // `verify_password` re-derives the hasher from the PHC string,
        // so old hashes created with previous params still verify
        // correctly even after we bump `argon2()`'s constants.
        if argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
        {
            Ok(Some(id))
        } else {
            Ok(None) // wrong password
        }
    }

    fn hash_password(password: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = argon2()
            .hash_password(password.as_bytes(), &salt)
            // `password_hash::Error` doesn't impl `std::error::Error`
            // (yet), so anyhow can't auto-convert it; we map manually.
            .map_err(|e| anyhow!("argon2 hashing failure: {}", e))?
            .to_string();
        Ok(hash)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Confirms the constants we ship match the doc-comment:
        /// (m=46 MiB, t=1, p=1) — argon2id, version 0x13.
        ///
        /// This test is a tripwire: if anyone bumps the constants
        /// without updating the comment + this assert, the build
        /// goes red instead of silently drifting.
        #[test]
        fn argon2_params_are_owasp_compliant() {
            assert_eq!(ARGON2_M_COST_KIB, 46 * 1024);
            assert_eq!(ARGON2_T_COST, 1);
            assert_eq!(ARGON2_P_COST, 1);

            let a = argon2();
            assert_eq!(a.params().m_cost(), 46 * 1024);
            assert_eq!(a.params().t_cost(), 1);
            assert_eq!(a.params().p_cost(), 1);
        }

        /// Old hashes (produced with argon2 0.5.x defaults of m=19 MiB
        /// t=2 p=1) still verify under the new hasher, because PHC
        /// strings carry their own params. Without this property,
        /// bumping the constants would lock out every pre-bump user.
        #[test]
        fn old_default_param_hash_still_verifies() {
            // Hash produced by `Argon2::default().hash_password(...)`
            // in argon2 0.5.x against password "s3cret". Captured
            // verbatim — the PHC string encodes m=19456, t=2, p=1.
            let phc = "$argon2id$v=19$m=19456,t=2,p=1$0ZiZsGrsNc6XnFyEv8aPEQ$aVKb2RkFjQEW7lIkWR6DBiA2X1pk3owcUhiMz/4GRcY";
            let parsed = PasswordHash::new(phc).expect("parse old PHC");
            assert!(argon2()
                .verify_password(b"s3cret", &parsed)
                .is_ok(),
                "old hashes must keep verifying after param bump");
            assert!(argon2()
                .verify_password(b"WRONG", &parsed)
                .is_err(),
                "wrong password against old hash must still fail");
        }
    }
}

// =====================================================================
// tokens
// =====================================================================

/// Operations on the `tokens` table. Each row pairs an `access_token`
/// (used in `Authorization: Bearer <...>`) with a `refresh_token`
/// (used to mint new tokens) and the `user_id` they belong to.
///
/// === Storage: hashed, not plaintext ===
///
/// Migration 0008 switched this table to storing `sha256(token)`
/// instead of the bearer-string itself. Why:
///
///   - **Timing-oracle hardening.** SQLite's `=` text comparison
///     short-circuits on the first byte mismatch. Comparing the raw
///     bearer string would leak prefix information to anyone able
///     to measure response latency (especially via the
///     unauthenticated `/auth` refresh-token path). Hashing produces
///     a uniform-prefix distribution; the mismatch position carries
///     no useful signal about the original token.
///   - **Defense-in-depth at rest.** A DB dump no longer hands the
///     attacker a usable bearer credential. SHA-256 is not slow
///     enough to be a real KDF, but tokens are 160-bit hex random
///     strings — there's no preimage attack to worry about.
///
/// Wire / response format unchanged: clients still receive the raw
/// hex token. The hash only lives on disk.
///
/// === Token expiry ===
///
/// Matching original LndHub semantics:
///   - `access_token`  expires after 7 days  (`ACCESS_TTL_SECS`)
///   - `refresh_token` expires after 31 days (`REFRESH_TTL_SECS`)
///
/// We don't store `expires_at` columns — the constants are folded
/// into the SELECT predicate so an expired row simply won't match
/// at lookup time. No background pruner needed (rows linger but
/// are inert); a periodic cleanup task removes long-expired rows
/// for disk-usage hygiene.
pub mod tokens {
    use super::{random_hex, unix_now, Pool, Result};

    use sha2::{Digest, Sha256};

    pub const ACCESS_TTL_SECS: i64 = 7 * 24 * 60 * 60;
    pub const REFRESH_TTL_SECS: i64 = 31 * 24 * 60 * 60;

    /// Hash a bearer-string the same way for both insert and lookup.
    /// SHA-256 hex digest, lowercase, 64 chars.
    ///
    /// `pub(crate)` so unit tests in this module's `tests` submodule
    /// can stage matching-hash rows without rederiving the function.
    pub(crate) fn hash_token(token: &str) -> String {
        let mut h = Sha256::new();
        h.update(token.as_bytes());
        hex::encode(h.finalize())
    }

    /// Mint a new access/refresh token pair for `user_id`. Both tokens
    /// are 20 bytes of OS-randomness, hex-encoded (40 chars). The DB
    /// stores their SHA-256 hex digests; the caller receives the raw
    /// tokens for return to the HTTP client.
    pub async fn create(pool: &Pool, user_id: i64) -> Result<(String, String)> {
        let access = random_hex(20);
        let refresh = random_hex(20);

        sqlx::query(
            "INSERT INTO tokens (access_token_hash, refresh_token_hash, user_id, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(hash_token(&access))
        .bind(hash_token(&refresh))
        .bind(user_id)
        .bind(unix_now())
        .execute(pool)
        .await?;

        Ok((access, refresh))
    }

    /// Resolve an access_token to a user_id. Returns `Ok(None)` if no
    /// such token exists OR the token has expired.
    ///
    /// The `created_at >= ?` clause filters out tokens past their
    /// 7-day TTL. To the caller, an expired token looks identical
    /// to a forged one — both yield 401 from the AuthUser extractor.
    pub async fn user_id_for_access(pool: &Pool, access_token: &str) -> Result<Option<i64>> {
        let cutoff = unix_now() - ACCESS_TTL_SECS;
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT user_id FROM tokens WHERE access_token_hash = ? AND created_at >= ?",
        )
        .bind(hash_token(access_token))
        .bind(cutoff)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Resolve a refresh_token to a user_id, applying the 31-day TTL.
    pub async fn user_id_for_refresh(pool: &Pool, refresh_token: &str) -> Result<Option<i64>> {
        let cutoff = unix_now() - REFRESH_TTL_SECS;
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT user_id FROM tokens WHERE refresh_token_hash = ? AND created_at >= ?",
        )
        .bind(hash_token(refresh_token))
        .bind(cutoff)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Delete tokens whose refresh half has expired (i.e. nothing in
    /// the row can possibly authenticate any more). Returns the number
    /// of rows removed.
    ///
    /// Called periodically by `main`'s background task — see the
    /// `tokio::spawn` near the bottom of `main.rs`.
    pub async fn cleanup_expired(pool: &Pool) -> Result<u64> {
        let cutoff = unix_now() - REFRESH_TTL_SECS;
        let result = sqlx::query("DELETE FROM tokens WHERE created_at < ?")
            .bind(cutoff)
            .execute(pool)
            .await?;
        Ok(result.rows_affected())
    }
}

// =====================================================================
// invoices
// =====================================================================

/// Operations on the `invoices` table — BOLT11 invoices we issued
/// via `/addinvoice`.
pub mod invoices {
    use super::{unix_now, Pool, Result};

    /// In-memory mirror of one row from `invoices`.
    ///
    /// We declare a plain struct (rather than using sqlx's `FromRow`
    /// derive) so the compile-time deps stay light. The query
    /// helpers below decode columns positionally into a tuple, then
    /// build this struct.
    ///
    /// `#[allow(dead_code)]` on the whole struct because some fields
    /// (e.g. `label`) are only used internally by lookup queries and
    /// never surfaced to callers — but we still want them mirrored
    /// here for completeness.
    #[allow(dead_code)]
    pub struct Row {
        pub payment_hash: String,
        pub label: String,
        pub user_id: i64,
        pub amount_msat: i64,
        pub memo: String,
        pub bolt11: String,
        pub expires_at: i64,
        pub created_at: i64,
        pub settled_at: Option<i64>,
        pub settled_msat: Option<i64>,
    }

    // The big tuple type we decode rows into. Defined once so the
    // signatures of `find_by_label` and `list_for_user` line up.
    type RowTuple = (
        String,
        String,
        i64,
        i64,
        String,
        String,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
    );

    fn from_tuple(t: RowTuple) -> Row {
        Row {
            payment_hash: t.0,
            label: t.1,
            user_id: t.2,
            amount_msat: t.3,
            memo: t.4,
            bolt11: t.5,
            expires_at: t.6,
            created_at: t.7,
            settled_at: t.8,
            settled_msat: t.9,
        }
    }

    const SELECT_COLS: &str = "payment_hash, label, user_id, amount_msat, memo, bolt11, \
                               expires_at, created_at, settled_at, settled_msat";

    /// Insert a new invoice record. Called from /addinvoice right
    /// after CLN returns a BOLT11.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        pool: &Pool,
        payment_hash: &str,
        label: &str,
        user_id: i64,
        amount_msat: i64,
        memo: &str,
        bolt11: &str,
        expires_at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO invoices \
                (payment_hash, label, user_id, amount_msat, memo, bolt11, expires_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(payment_hash)
        .bind(label)
        .bind(user_id)
        .bind(amount_msat)
        .bind(memo)
        .bind(bolt11)
        .bind(expires_at)
        .bind(unix_now())
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Find one invoice by the label we gave CLN. Used by the
    /// `invoice_payment` notification handler to map the inbound
    /// settle event back to its owner.
    pub async fn find_by_label(pool: &Pool, label: &str) -> Result<Option<Row>> {
        let q = format!("SELECT {SELECT_COLS} FROM invoices WHERE label = ?");
        let row: Option<RowTuple> = sqlx::query_as(&q).bind(label).fetch_optional(pool).await?;
        Ok(row.map(from_tuple))
    }

    /// List a user's invoices, newest first.
    pub async fn list_for_user(pool: &Pool, user_id: i64) -> Result<Vec<Row>> {
        let q = format!(
            "SELECT {SELECT_COLS} FROM invoices WHERE user_id = ? ORDER BY created_at DESC"
        );
        let rows: Vec<RowTuple> = sqlx::query_as(&q).bind(user_id).fetch_all(pool).await?;
        Ok(rows.into_iter().map(from_tuple).collect())
    }
}

// =====================================================================
// payments
// =====================================================================

/// Operations on the `payments` table — outbound payments the user
/// has initiated via `/payinvoice`.
pub mod payments {
    use super::Pool;

    use anyhow::Result;

    /// One row from `payments`.
    #[allow(dead_code)]
    pub struct Row {
        pub id: i64,
        pub payment_hash: String,
        pub user_id: i64,
        pub bolt11: String,
        pub amount_msat: i64,
        pub fee_msat: i64,
        pub preimage: Option<String>,
        pub status: String,
        pub memo: String,
        pub created_at: i64,
        pub settled_at: Option<i64>,
    }

    type RowTuple = (
        i64,
        String,
        i64,
        String,
        i64,
        i64,
        Option<String>,
        String,
        String,
        i64,
        Option<i64>,
    );

    fn from_tuple(t: RowTuple) -> Row {
        Row {
            id: t.0,
            payment_hash: t.1,
            user_id: t.2,
            bolt11: t.3,
            amount_msat: t.4,
            fee_msat: t.5,
            preimage: t.6,
            status: t.7,
            memo: t.8,
            created_at: t.9,
            settled_at: t.10,
        }
    }

    const SELECT_COLS: &str = "id, payment_hash, user_id, bolt11, amount_msat, fee_msat, \
                               preimage, status, memo, created_at, settled_at";

    /// List a user's outbound payments, newest first.
    pub async fn list_for_user(pool: &Pool, user_id: i64) -> Result<Vec<Row>> {
        let q = format!(
            "SELECT {SELECT_COLS} FROM payments WHERE user_id = ? ORDER BY created_at DESC"
        );
        let rows: Vec<RowTuple> = sqlx::query_as(&q).bind(user_id).fetch_all(pool).await?;
        Ok(rows.into_iter().map(from_tuple).collect())
    }
}

// =====================================================================
// addresses (on-chain deposit)
// =====================================================================

/// Per-user on-chain deposit address. Each user has at most one,
/// minted lazily on first /getbtc call.
pub mod addresses {
    use super::{unix_now, Pool, Result};

    /// Look up the user's existing deposit address.
    pub async fn for_user(pool: &Pool, user_id: i64) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT address FROM addresses WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(pool)
                .await?;
        Ok(row.map(|(a,)| a))
    }

    /// Insert a freshly-minted address for a user. Errors if the user
    /// already has one (the UNIQUE PRIMARY KEY on user_id catches it),
    /// which the caller should handle by re-reading via `for_user`.
    pub async fn create(pool: &Pool, user_id: i64, address: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO addresses (user_id, address, created_at) VALUES (?, ?, ?)",
        )
        .bind(user_id)
        .bind(address)
        .bind(unix_now())
        .execute(pool)
        .await?;
        Ok(())
    }
}

// =====================================================================
// onchain_credits (read-side helper)
// =====================================================================

/// Read-side helpers for the `onchain_credits` table. Writes happen
/// in `credit_onchain` (atomic with the ledger row) — there is no
/// public mutator here.
pub mod onchain_credits {
    use super::{Pool, Result};

    /// `#[allow(dead_code)]` because callers currently only read a
    /// subset of fields; the rest are loaded for symmetry / future
    /// diagnostics.
    #[allow(dead_code)]
    pub struct Row {
        pub txid: String,
        pub vout: i64,
        pub user_id: i64,
        pub address: String,
        pub amount_msat: i64,
        pub blockheight: Option<i64>,
        pub credited_at: i64,
    }

    /// Tuple shape of one row, matching `SELECT_COLS` below. Aliased
    /// so the sqlx generic doesn't bloat the function signature
    /// (matches the pattern used by the other modules in this file).
    type RowTuple = (String, i64, i64, String, i64, Option<i64>, i64);

    fn from_tuple(t: RowTuple) -> Row {
        Row {
            txid: t.0,
            vout: t.1,
            user_id: t.2,
            address: t.3,
            amount_msat: t.4,
            blockheight: t.5,
            credited_at: t.6,
        }
    }

    const SELECT_COLS: &str =
        "txid, vout, user_id, address, amount_msat, blockheight, credited_at";

    /// List one user's confirmed on-chain credits, newest first.
    pub async fn list_for_user(pool: &Pool, user_id: i64) -> Result<Vec<Row>> {
        let q = format!(
            "SELECT {SELECT_COLS} FROM onchain_credits WHERE user_id = ? ORDER BY credited_at DESC"
        );
        let rows: Vec<RowTuple> = sqlx::query_as(&q).bind(user_id).fetch_all(pool).await?;
        Ok(rows.into_iter().map(from_tuple).collect())
    }
}

// =====================================================================
// Balance
// =====================================================================

/// Compute a user's current balance in msat.
///
/// The ledger is append-only: every credit (positive amount) and
/// debit (negative amount) gets its own row. Balance is just the sum.
/// SQLite returns NULL for SUM-of-no-rows, so we COALESCE to 0.
pub async fn balance_msat(pool: &Pool, user_id: i64) -> Result<i64> {
    let (sum,): (i64,) =
        sqlx::query_as("SELECT COALESCE(SUM(amount_msat), 0) FROM ledger WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    Ok(sum)
}

// =====================================================================
// Internal payment (one local user pays another's invoice)
// =====================================================================

/// Outcome of an internal-payment attempt.
///
/// `try_settle_internal` returns one of these instead of bubbling
/// every condition up as an `Err`, so the HTTP handler can map each
/// case to the right LndHub status code and message.
pub enum InternalPayResult {
    /// Settled atomically. Receiver got `amount_msat`; their invoice
    /// is now marked paid.
    Settled { receiver_user_id: i64 },
    /// `payment_hash` is not in our `invoices` table — caller should
    /// fall through to the external CLN `pay` path (slice 5b).
    NotOurInvoice,
    /// Invoice exists but has already been settled.
    AlreadyPaid,
    /// Sender and receiver are the same user. Refused.
    SelfPayment,
    /// Sender's balance is below the invoice amount.
    InsufficientBalance { balance_msat: i64 },
}

/// Try to settle a payment entirely inside our database, without
/// asking CLN to pay anything.
///
/// === The transaction ===
///
/// All steps run inside a single `BEGIN IMMEDIATE` SQLite transaction
/// so a crash partway through (or two concurrent payments racing)
/// cannot leave the ledger inconsistent. Either everything commits
/// or nothing does.
///
/// 1. Look up the invoice by `payment_hash`. Bail with `NotOurInvoice`
///    if absent (caller goes to the external path).
/// 2. Check it's unpaid. If not, `AlreadyPaid`.
/// 3. Refuse self-payment.
/// 4. Compute the payer's balance INSIDE the transaction. If it's less
///    than the amount, `InsufficientBalance`.
/// 5. Mark the invoice settled (`UPDATE ... WHERE settled_at IS NULL`).
///    This re-checks idempotency at commit time; if a concurrent
///    notification raced us, our UPDATE matches zero rows and we
///    return `AlreadyPaid`.
/// 6. Insert a `payments` row owned by the payer (status='internal').
/// 7. Insert two ledger entries: `internal_out` (debit payer) and
///    `internal_in` (credit receiver).
/// 8. Commit.
pub async fn try_settle_internal(
    pool: &Pool,
    payer_user_id: i64,
    payment_hash: &str,
    bolt11: &str,
    amount_msat: i64,
    memo: &str,
) -> Result<InternalPayResult> {
    let mut tx = pool.begin().await?;

    // (1) Find the invoice.
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT user_id, settled_at FROM invoices WHERE payment_hash = ?",
    )
    .bind(payment_hash)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((receiver_user_id, settled_at)) = row else {
        tx.rollback().await?;
        return Ok(InternalPayResult::NotOurInvoice);
    };

    // (2) Already paid?
    if settled_at.is_some() {
        tx.rollback().await?;
        return Ok(InternalPayResult::AlreadyPaid);
    }

    // (3) Self-pay refused.
    if receiver_user_id == payer_user_id {
        tx.rollback().await?;
        return Ok(InternalPayResult::SelfPayment);
    }

    // (4) Balance check inside the transaction.
    let (balance,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(amount_msat), 0) FROM ledger WHERE user_id = ?",
    )
    .bind(payer_user_id)
    .fetch_one(&mut *tx)
    .await?;

    if balance < amount_msat {
        tx.rollback().await?;
        return Ok(InternalPayResult::InsufficientBalance {
            balance_msat: balance,
        });
    }

    let now = unix_now();

    // (5) Mark invoice settled. The conditional WHERE re-asserts
    //     idempotency in case a concurrent settle raced us.
    let updated = sqlx::query(
        "UPDATE invoices SET settled_at = ?, settled_msat = ? \
         WHERE payment_hash = ? AND settled_at IS NULL",
    )
    .bind(now)
    .bind(amount_msat)
    .bind(payment_hash)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if updated == 0 {
        // Lost the race.
        tx.rollback().await?;
        return Ok(InternalPayResult::AlreadyPaid);
    }

    // (6) Record the payment from the payer's POV.
    sqlx::query(
        "INSERT INTO payments \
            (payment_hash, user_id, bolt11, amount_msat, fee_msat, preimage, status, memo, created_at, settled_at) \
         VALUES (?, ?, ?, ?, 0, NULL, 'internal', ?, ?, ?)",
    )
    .bind(payment_hash)
    .bind(payer_user_id)
    .bind(bolt11)
    .bind(amount_msat)
    .bind(memo)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    // (7a) Debit the payer.
    sqlx::query(
        "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
         VALUES (?, 'internal_out', ?, ?, ?, ?)",
    )
    .bind(payer_user_id)
    .bind(-amount_msat)
    .bind(payment_hash)
    .bind(memo)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    // (7b) Credit the receiver.
    sqlx::query(
        "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
         VALUES (?, 'internal_in', ?, ?, ?, ?)",
    )
    .bind(receiver_user_id)
    .bind(amount_msat)
    .bind(payment_hash)
    .bind(memo)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    // (8) All-or-nothing commit.
    tx.commit().await?;

    Ok(InternalPayResult::Settled { receiver_user_id })
}

// =====================================================================
// External Lightning payment (slice 5b)
// =====================================================================

/// Outcome of `reserve_external_pay`.
///
/// The function is non-fallible at the business-logic level — every
/// scenario (success, low balance, in-progress duplicate) has a
/// dedicated variant — so the HTTP handler can map each cleanly to
/// the matching LndHub error code. Database / SQLite failures still
/// bubble up via the outer `Result`.
pub enum ReserveResult {
    /// Reserved successfully. Caller MUST eventually finalize via
    /// `settle_external_pay` or `fail_external_pay`.
    Reserved,
    /// Balance check failed inside the reserve transaction.
    InsufficientBalance {
        balance_msat: i64,
        required_msat: i64,
    },
    /// A row for this (user, payment_hash) already exists in
    /// 'external_pending' or terminal state. Refuse a second attempt.
    AlreadyAttempted,
}

/// Reserve funds for an external (CLN `pay`) Lightning payment.
///
/// In one atomic SQLite transaction:
///   1. Check the user's balance covers `amount_msat + fee_reserve_msat`.
///   2. Insert a `payments` row with status `external_pending`. The
///      partial UNIQUE index from migration 0006 forbids two
///      concurrent pendings for the same (user, hash).
///   3. Debit the ledger by the full reserved total.
///
/// Why reserve a fee *upfront* (rather than debiting only the actual
/// fee after pay succeeds)? Because a custodial user must not be
/// allowed to spend right up to their balance and then have a route
/// fee push them negative. We over-reserve, then refund the unused
/// portion in `settle_external_pay`.
///
/// Returns the matching `ReserveResult` variant — the HTTP handler
/// branches on it.
#[allow(clippy::too_many_arguments)]
pub async fn reserve_external_pay(
    pool: &Pool,
    user_id: i64,
    payment_hash: &str,
    bolt11: &str,
    memo: &str,
    amount_msat: i64,
    fee_reserve_msat: i64,
) -> Result<ReserveResult> {
    let mut tx = pool.begin().await?;
    let now = unix_now();
    let total = amount_msat + fee_reserve_msat;

    // (1) Balance check INSIDE the tx — without this, two concurrent
    //     reserves could each pass and we'd overdraft.
    let (balance,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(amount_msat), 0) FROM ledger WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;

    if balance < total {
        tx.rollback().await?;
        return Ok(ReserveResult::InsufficientBalance {
            balance_msat: balance,
            required_msat: total,
        });
    }

    // (2) Insert pending row. We store `fee_reserve_msat` in the
    //     `fee_msat` column temporarily — `settle_external_pay`
    //     overwrites with the actual fee on success.
    let insert = sqlx::query(
        "INSERT INTO payments \
            (payment_hash, user_id, bolt11, amount_msat, fee_msat, preimage, status, memo, created_at, settled_at) \
         VALUES (?, ?, ?, ?, ?, NULL, 'external_pending', ?, ?, NULL)",
    )
    .bind(payment_hash)
    .bind(user_id)
    .bind(bolt11)
    .bind(amount_msat)
    .bind(fee_reserve_msat)
    .bind(memo)
    .bind(now)
    .execute(&mut *tx)
    .await;

    match insert {
        Ok(_) => {}
        // The partial UNIQUE index from migration 0003 / 0006 means
        // this row already exists in some state (pending or settled).
        // `dbe.is_unique_violation()` is sqlx's portable way to
        // recognise this across drivers.
        Err(sqlx::Error::Database(dbe)) if dbe.is_unique_violation() => {
            tx.rollback().await?;
            return Ok(ReserveResult::AlreadyAttempted);
        }
        Err(e) => return Err(e.into()),
    }

    // (3) Debit. Single ledger row covers amount + reserve; the
    //     unused-reserve refund (if any) goes in `settle_external_pay`.
    sqlx::query(
        "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
         VALUES (?, 'payment_sent', ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(-total)
    .bind(payment_hash)
    .bind(memo)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(ReserveResult::Reserved)
}

/// Finalize a successful external payment. Called from the
/// `/payinvoice` handler when CLN's `pay` returns Ok, AND from the
/// reconciler when it finds a previously-pending payment that has
/// since become `complete` in `listpays`.
///
/// Effects (atomic, only when the row is still `external_pending`):
///   - payments row → `external_settled`, with preimage + actual fee.
///   - If `fee_reserve_msat > actual_fee_msat`, credit the difference
///     back to the user (`payment_fee_refund`).
///
/// Idempotency: the UPDATE is gated on `status = 'external_pending'`,
/// so a second call simply UPDATES zero rows and we return
/// `Ok(false)` — no error. The handler and the reconciler can both
/// race to finalize the same row; whichever loses the race learns
/// that the work was already done and reports success to its caller.
///
/// Returns:
///   - `Ok(true)`  — this call settled the row.
///   - `Ok(false)` — the row was already finalized (settled or failed)
///     by a concurrent caller; no further work happened.
///   - `Err(_)`    — database error.
pub async fn settle_external_pay(
    pool: &Pool,
    user_id: i64,
    payment_hash: &str,
    preimage: &str,
    actual_fee_msat: i64,
    fee_reserve_msat: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let now = unix_now();

    let updated = sqlx::query(
        "UPDATE payments \
         SET status = 'external_settled', preimage = ?, fee_msat = ?, settled_at = ? \
         WHERE payment_hash = ? AND user_id = ? AND status = 'external_pending'",
    )
    .bind(preimage)
    .bind(actual_fee_msat)
    .bind(now)
    .bind(payment_hash)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if updated == 0 {
        // Already finalized by a concurrent path (the
        // /payinvoice in-band handler and the reconciler can race
        // when CLN settles slowly). No error; caller picks up
        // current row state if it needs to.
        tx.rollback().await?;
        return Ok(false);
    }

    // Refund unused fee reserve. Negative `unused` (over-spend) would
    // be a bug — `pay` should refuse routes whose fee exceeds the
    // request, and our reserve is a buffer above expected fees — so
    // we treat that as an exception worth logging at the call site.
    let unused = fee_reserve_msat - actual_fee_msat;
    if unused > 0 {
        sqlx::query(
            "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
             VALUES (?, 'payment_fee_refund', ?, ?, 'unused fee reserve refund', ?)",
        )
        .bind(user_id)
        .bind(unused)
        .bind(payment_hash)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(true)
}

/// Finalize a terminally-failed external payment.
///
/// Effects (atomic, only when the row is still `external_pending`):
///   - payments row → `external_failed`.
///   - Compensating credit for the FULL reserved amount (amount + fee
///     reserve) — the user is made whole.
///
/// Same idempotency contract as `settle_external_pay`: `Ok(false)`
/// when another caller already finalized the row.
pub async fn fail_external_pay(
    pool: &Pool,
    user_id: i64,
    payment_hash: &str,
    amount_msat: i64,
    fee_reserve_msat: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let now = unix_now();
    let total = amount_msat + fee_reserve_msat;

    let updated = sqlx::query(
        "UPDATE payments \
         SET status = 'external_failed', settled_at = ? \
         WHERE payment_hash = ? AND user_id = ? AND status = 'external_pending'",
    )
    .bind(now)
    .bind(payment_hash)
    .bind(user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if updated == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    sqlx::query(
        "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
         VALUES (?, 'payment_refund', ?, ?, 'refund: external pay terminal failure', ?)",
    )
    .bind(user_id)
    .bind(total)
    .bind(payment_hash)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

/// Increment the empty-listpays-sweep counter on a pending payment
/// row and return the new value. Used by the reconciler when CLN's
/// `listpays` returns no record of a payment hash — we wait for
/// several consecutive empty sightings before assuming the payment
/// truly never left the node and refunding the user.
///
/// No-op if the row is no longer `external_pending` (a concurrent
/// settle/fail already finalized it); the inner UPDATE matches zero
/// rows and the subsequent SELECT just returns whatever count is on
/// disk. The reconciler treats a no-op as "we lost the race, skip".
pub async fn bump_empty_listpays_sweeps(
    pool: &Pool,
    user_id: i64,
    payment_hash: &str,
) -> Result<i64> {
    sqlx::query(
        "UPDATE payments \
         SET empty_listpays_sweeps = empty_listpays_sweeps + 1 \
         WHERE user_id = ? AND payment_hash = ? AND status = 'external_pending'",
    )
    .bind(user_id)
    .bind(payment_hash)
    .execute(pool)
    .await?;

    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT empty_listpays_sweeps FROM payments \
         WHERE user_id = ? AND payment_hash = ?",
    )
    .bind(user_id)
    .bind(payment_hash)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(n,)| n).unwrap_or(0))
}

/// Reset the empty-listpays-sweep counter when CLN reports any non-
/// empty `listpays` payload for this payment hash. Even if the
/// status is `pending`, we don't want a transient empty response
/// later to count toward the refund threshold.
pub async fn reset_empty_listpays_sweeps(
    pool: &Pool,
    user_id: i64,
    payment_hash: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE payments \
         SET empty_listpays_sweeps = 0 \
         WHERE user_id = ? AND payment_hash = ? AND status = 'external_pending'",
    )
    .bind(user_id)
    .bind(payment_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Snapshot of a pending external payment used by the reconciler.
///
/// `#[allow(dead_code)]` on `created_at` because it's selected for
/// future diagnostics (age-of-pending logging) but isn't currently
/// read by the reconciler.
pub struct PendingPayment {
    pub user_id: i64,
    pub payment_hash: String,
    pub amount_msat: i64,
    /// The reserved fee buffer (stored in `payments.fee_msat` until
    /// settle replaces it with the actual fee).
    pub fee_reserve_msat: i64,
    #[allow(dead_code)]
    pub created_at: i64,
}

/// All payments still in `external_pending`. The reconciler enumerates
/// these and asks CLN's `listpays` what really happened to each.
pub async fn list_pending_external(pool: &Pool) -> Result<Vec<PendingPayment>> {
    let rows: Vec<(i64, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT user_id, payment_hash, amount_msat, fee_msat, created_at \
         FROM payments WHERE status = 'external_pending'",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(user_id, payment_hash, amount_msat, fee_reserve_msat, created_at)| PendingPayment {
                user_id,
                payment_hash,
                amount_msat,
                fee_reserve_msat,
                created_at,
            },
        )
        .collect())
}

// =====================================================================
// Atomic credit-on-deposit (slice 5e)
// =====================================================================

/// Credit a confirmed on-chain UTXO to the owning user.
///
/// Idempotent: the `(txid, vout)` PRIMARY KEY in `onchain_credits`
/// plus the `INSERT OR IGNORE` mean a re-scan that re-encounters the
/// same UTXO is a no-op. Returns `Ok(true)` if a credit happened,
/// `Ok(false)` if the UTXO had already been processed.
///
/// Both INSERTs run in a single transaction so a crash mid-way
/// cannot leave us with the credit recorded but the ledger row
/// missing (or vice-versa).
pub async fn credit_onchain(
    pool: &Pool,
    txid: &str,
    vout: i64,
    user_id: i64,
    address: &str,
    amount_msat: i64,
    blockheight: Option<i64>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let now = unix_now();

    // (1) Mark the UTXO as credited. `INSERT OR IGNORE` returns 0
    //     rows-affected if (txid, vout) already exists.
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO onchain_credits \
            (txid, vout, user_id, address, amount_msat, blockheight, credited_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(txid)
    .bind(vout)
    .bind(user_id)
    .bind(address)
    .bind(amount_msat)
    .bind(blockheight)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if inserted == 0 {
        tx.rollback().await?;
        return Ok(false);
    }

    // (2) Credit the ledger.
    sqlx::query(
        "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
         VALUES (?, 'onchain_in', ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(amount_msat)
    .bind(txid)
    .bind(format!("on-chain deposit (vout {})", vout))
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

// =====================================================================
// Atomic credit-on-settle (slice 4)
// =====================================================================

/// Mark an invoice settled AND credit the owning user's ledger,
/// inside a single SQLite transaction. Returns `Ok(true)` if the
/// credit happened, `Ok(false)` if there was nothing to do (the
/// invoice doesn't exist or was already settled).
///
/// === Rust note: `sqlx::Transaction` ===
///
/// `pool.begin().await?` returns a `Transaction<'_, Sqlite>` that
/// borrows the pool. While it's alive, queries we run via
/// `&mut *tx` go through that single connection inside a `BEGIN
/// IMMEDIATE` transaction. Calling `.commit().await?` finalises;
/// `.rollback()` (or simply dropping the value) discards.
///
/// === Idempotency ===
///
/// The conditional `WHERE settled_at IS NULL` makes this safe to
/// retry. If the notification fires twice (or we crash and restart),
/// the second pass UPDATEs zero rows, so we skip the credit and
/// nobody gets double-paid.
pub async fn settle_invoice(
    pool: &Pool,
    payment_hash: &str,
    settled_msat: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let now = unix_now();

    let updated = sqlx::query(
        "UPDATE invoices SET settled_at = ?, settled_msat = ? \
         WHERE payment_hash = ? AND settled_at IS NULL",
    )
    .bind(now)
    .bind(settled_msat)
    .bind(payment_hash)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if updated == 0 {
        // Either not in our table or already settled. No-op.
        tx.rollback().await?;
        return Ok(false);
    }

    // Look up the owning user inside the same tx so we know the
    // row is the one we just updated.
    let (user_id,): (i64,) =
        sqlx::query_as("SELECT user_id FROM invoices WHERE payment_hash = ?")
            .bind(payment_hash)
            .fetch_one(&mut *tx)
            .await?;

    sqlx::query(
        "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
         VALUES (?, 'invoice_settled', ?, ?, NULL, ?)",
    )
    .bind(user_id)
    .bind(settled_msat)
    .bind(payment_hash)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(true)
}

// =====================================================================
// Helpers
// =====================================================================

/// Generate `n` cryptographically-secure random bytes, hex-encoded.
/// Used for fresh logins, passwords, tokens, and invoice labels.
pub fn random_hex(n: usize) -> String {
    use rand::rngs::OsRng;
    use rand::RngCore;

    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    hex::encode(&buf)
}

/// Current unix epoch seconds. Wrapped here so every "now()" in the
/// codebase agrees on one definition.
///
/// `pub(crate)` so the startup clock-sanity check in `main.rs` can
/// also call into this single source of truth — refusing to serve
/// on a broken clock that returns 0 (which would otherwise reset
/// every token's TTL math and let expired tokens pass).
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

// =====================================================================
// Tests
// =====================================================================
//
// Unit tests for every atomic / safety-critical function in this
// module. Each test gets a fresh in-memory SQLite database with
// migrations applied, so there's no cross-test contamination and no
// filesystem state to clean up.
//
// === Rust note: `#[cfg(test)]` ===
//
// The whole module is gated on `cfg(test)`, meaning the compiler
// only compiles it when running `cargo test`. Zero overhead on
// release builds.
//
// === Rust note: `#[tokio::test]` ===
//
// Like `#[tokio::main]` for tests — sets up a tokio runtime for
// each test so we can use `await` inside the test body.

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// Build a fresh in-memory SQLite pool with migrations applied.
    ///
    /// `max_connections=1` is important: the in-memory database
    /// lives inside a single connection's address space. A pool
    /// with multiple connections would see each as a separate
    /// empty DB, which would defeat the migration setup.
    async fn fresh_pool() -> Pool {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("create in-memory pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn make_user(pool: &Pool, login: &str) -> i64 {
        users::create(pool, login, "pw").await.unwrap()
    }

    /// Direct ledger insert to set up balance for spend-path tests.
    /// Bypasses every business-logic check — that's the point: we
    /// want to drive the path under test, not the seed mechanism.
    async fn seed_balance(pool: &Pool, user_id: i64, amount_msat: i64) {
        sqlx::query(
            "INSERT INTO ledger (user_id, kind, amount_msat, ref_hash, description, created_at) \
             VALUES (?, 'seed', ?, NULL, 'test seed', ?)",
        )
        .bind(user_id)
        .bind(amount_msat)
        .bind(unix_now())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn make_invoice(pool: &Pool, owner_id: i64, payment_hash: &str, amount_msat: i64) {
        invoices::create(
            pool,
            payment_hash,
            &format!("test-{}", payment_hash),
            owner_id,
            amount_msat,
            "memo",
            "lnbc-test",
            unix_now() + 3600,
        )
        .await
        .unwrap();
    }

    // ---------- users ----------

    #[tokio::test]
    async fn users_create_and_verify_round_trip() {
        let pool = fresh_pool().await;
        let id = users::create(&pool, "alice", "s3cret").await.unwrap();
        assert!(id > 0);
        assert_eq!(
            users::verify(&pool, "alice", "s3cret").await.unwrap(),
            Some(id),
            "correct password should match"
        );
        assert_eq!(
            users::verify(&pool, "alice", "WRONG").await.unwrap(),
            None,
            "wrong password should fail"
        );
        assert_eq!(
            users::verify(&pool, "bob", "s3cret").await.unwrap(),
            None,
            "unknown login should fail"
        );
    }

    // ---------- balance ----------

    #[tokio::test]
    async fn balance_zero_for_new_user() {
        let pool = fresh_pool().await;
        let id = make_user(&pool, "alice").await;
        assert_eq!(balance_msat(&pool, id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn balance_sums_ledger_entries() {
        let pool = fresh_pool().await;
        let id = make_user(&pool, "alice").await;
        seed_balance(&pool, id, 100_000).await;
        seed_balance(&pool, id, 50_000).await;
        seed_balance(&pool, id, -30_000).await;
        assert_eq!(balance_msat(&pool, id).await.unwrap(), 120_000);
    }

    // ---------- try_settle_internal ----------

    #[tokio::test]
    async fn internal_pay_success() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        let bob = make_user(&pool, "bob").await;
        seed_balance(&pool, alice, 100_000).await;
        make_invoice(&pool, bob, "h1", 30_000).await;

        let result = try_settle_internal(&pool, alice, "h1", "lnbc", 30_000, "memo")
            .await
            .unwrap();
        match result {
            InternalPayResult::Settled { receiver_user_id } => assert_eq!(receiver_user_id, bob),
            _ => panic!("expected Settled"),
        }
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 70_000);
        assert_eq!(balance_msat(&pool, bob).await.unwrap(), 30_000);
    }

    #[tokio::test]
    async fn internal_pay_not_our_invoice() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        // No invoice exists for this hash.
        let result = try_settle_internal(&pool, alice, "stranger", "lnbc", 10_000, "m")
            .await
            .unwrap();
        assert!(matches!(result, InternalPayResult::NotOurInvoice));
        // Balance untouched.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 100_000);
    }

    #[tokio::test]
    async fn internal_pay_already_paid() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        let bob = make_user(&pool, "bob").await;
        seed_balance(&pool, alice, 100_000).await;
        make_invoice(&pool, bob, "h2", 30_000).await;
        // First pay settles.
        try_settle_internal(&pool, alice, "h2", "lnbc", 30_000, "m")
            .await
            .unwrap();
        // Second attempt sees settled invoice.
        let result = try_settle_internal(&pool, alice, "h2", "lnbc", 30_000, "m")
            .await
            .unwrap();
        assert!(matches!(result, InternalPayResult::AlreadyPaid));
        // No additional movement.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 70_000);
        assert_eq!(balance_msat(&pool, bob).await.unwrap(), 30_000);
    }

    #[tokio::test]
    async fn internal_pay_self_payment_refused() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        make_invoice(&pool, alice, "h3", 10_000).await;
        let result = try_settle_internal(&pool, alice, "h3", "lnbc", 10_000, "m")
            .await
            .unwrap();
        assert!(matches!(result, InternalPayResult::SelfPayment));
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 100_000);
    }

    #[tokio::test]
    async fn internal_pay_insufficient_balance() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        let bob = make_user(&pool, "bob").await;
        seed_balance(&pool, alice, 5_000).await;
        make_invoice(&pool, bob, "h4", 30_000).await;
        let result = try_settle_internal(&pool, alice, "h4", "lnbc", 30_000, "m")
            .await
            .unwrap();
        match result {
            InternalPayResult::InsufficientBalance { balance_msat: bal, .. } => {
                assert_eq!(bal, 5_000)
            }
            _ => panic!("expected InsufficientBalance"),
        }
        // Both balances untouched.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 5_000);
        assert_eq!(balance_msat(&pool, bob).await.unwrap(), 0);
    }

    // ---------- reserve / settle / fail external pay ----------

    #[tokio::test]
    async fn reserve_external_success() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        let result = reserve_external_pay(&pool, alice, "rh1", "lnbc", "m", 50_000, 1_000)
            .await
            .unwrap();
        assert!(matches!(result, ReserveResult::Reserved));
        // Reserve debited amount + fee_reserve.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 49_000);
    }

    #[tokio::test]
    async fn reserve_external_insufficient() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 10_000).await;
        let result = reserve_external_pay(&pool, alice, "rh2", "lnbc", "m", 50_000, 1_000)
            .await
            .unwrap();
        match result {
            ReserveResult::InsufficientBalance {
                balance_msat: b,
                required_msat: r,
            } => {
                assert_eq!(b, 10_000);
                assert_eq!(r, 51_000);
            }
            _ => panic!("expected InsufficientBalance"),
        }
        // No movement on rejected reserve.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 10_000);
    }

    #[tokio::test]
    async fn reserve_external_duplicate_rejected_by_unified_index() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 200_000).await;
        // First reserve goes through.
        reserve_external_pay(&pool, alice, "rh3", "lnbc", "m", 50_000, 1_000)
            .await
            .unwrap();
        // Second reserve for same (user, hash) trips the unified
        // partial UNIQUE index from migration 0006.
        let result = reserve_external_pay(&pool, alice, "rh3", "lnbc", "m", 50_000, 1_000)
            .await
            .unwrap();
        assert!(matches!(result, ReserveResult::AlreadyAttempted));
        // Only the first reserve's debit landed.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 149_000);
    }

    #[tokio::test]
    async fn settle_external_refunds_unused_reserve() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        reserve_external_pay(&pool, alice, "rh4", "lnbc", "m", 50_000, 10_000)
            .await
            .unwrap();
        // After reserve: 100k - (50k + 10k) = 40k.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 40_000);

        let settled = settle_external_pay(&pool, alice, "rh4", "preimage_hex", 3_000, 10_000)
            .await
            .unwrap();
        assert!(settled, "first settle should report true");
        // 7k of 10k reserve refunded → 40k + 7k = 47k.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 47_000);
    }

    #[tokio::test]
    async fn settle_external_no_refund_when_reserve_exhausted() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        reserve_external_pay(&pool, alice, "rh5", "lnbc", "m", 50_000, 10_000)
            .await
            .unwrap();
        let settled = settle_external_pay(&pool, alice, "rh5", "preimage", 10_000, 10_000)
            .await
            .unwrap();
        assert!(settled);
        // 10k reserve all consumed by fees → no refund. 40k stands.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 40_000);
    }

    #[tokio::test]
    async fn settle_external_second_call_returns_ok_false() {
        // The handler and the reconciler may both call settle on the
        // same row. The loser must not error — return Ok(false) and
        // leave balances unchanged.
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        reserve_external_pay(&pool, alice, "rh4b", "lnbc", "m", 50_000, 10_000)
            .await
            .unwrap();
        let first = settle_external_pay(&pool, alice, "rh4b", "pre", 3_000, 10_000)
            .await
            .unwrap();
        assert!(first);
        let before = balance_msat(&pool, alice).await.unwrap();

        let second = settle_external_pay(&pool, alice, "rh4b", "pre", 3_000, 10_000)
            .await
            .unwrap();
        assert!(!second, "second settle should report false (already finalized)");
        // Balance must not change on the second call.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), before);
    }

    #[tokio::test]
    async fn fail_external_refunds_full_reserve() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        reserve_external_pay(&pool, alice, "rh6", "lnbc", "m", 50_000, 10_000)
            .await
            .unwrap();
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 40_000);

        let failed = fail_external_pay(&pool, alice, "rh6", 50_000, 10_000)
            .await
            .unwrap();
        assert!(failed);
        // Full refund: 100k restored.
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 100_000);
    }

    #[tokio::test]
    async fn fail_external_second_call_returns_ok_false() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        reserve_external_pay(&pool, alice, "rh6b", "lnbc", "m", 50_000, 10_000)
            .await
            .unwrap();
        let first = fail_external_pay(&pool, alice, "rh6b", 50_000, 10_000)
            .await
            .unwrap();
        assert!(first);
        let before = balance_msat(&pool, alice).await.unwrap();

        let second = fail_external_pay(&pool, alice, "rh6b", 50_000, 10_000)
            .await
            .unwrap();
        assert!(!second, "second fail should be a no-op");
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), before);
    }

    #[tokio::test]
    async fn bump_and_reset_empty_listpays_sweeps() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        reserve_external_pay(&pool, alice, "rh7", "lnbc", "m", 50_000, 10_000)
            .await
            .unwrap();

        let a = bump_empty_listpays_sweeps(&pool, alice, "rh7").await.unwrap();
        let b = bump_empty_listpays_sweeps(&pool, alice, "rh7").await.unwrap();
        let c = bump_empty_listpays_sweeps(&pool, alice, "rh7").await.unwrap();
        assert_eq!((a, b, c), (1, 2, 3));

        reset_empty_listpays_sweeps(&pool, alice, "rh7")
            .await
            .unwrap();
        let after_reset = bump_empty_listpays_sweeps(&pool, alice, "rh7")
            .await
            .unwrap();
        assert_eq!(after_reset, 1, "reset should zero the counter");
    }

    #[tokio::test]
    async fn bump_empty_listpays_no_op_on_finalized_row() {
        // Once the row is no longer external_pending, the sweep counter
        // should not advance. Otherwise a slow reconciler could mark a
        // settled row "ready to refund".
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        seed_balance(&pool, alice, 100_000).await;
        reserve_external_pay(&pool, alice, "rh8", "lnbc", "m", 50_000, 10_000)
            .await
            .unwrap();
        settle_external_pay(&pool, alice, "rh8", "pre", 3_000, 10_000)
            .await
            .unwrap();

        let n = bump_empty_listpays_sweeps(&pool, alice, "rh8").await.unwrap();
        assert_eq!(n, 0, "settled row's sweep counter must stay at 0");
    }

    // ---------- onchain_credits::list_for_user ----------

    #[tokio::test]
    async fn onchain_credits_list_for_user_orders_newest_first() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        addresses::create(&pool, alice, "bc1qfake").await.unwrap();

        // Three rows with hand-stamped credited_at so the sort is
        // deterministic regardless of test-run timing.
        sqlx::query(
            "INSERT INTO onchain_credits (txid, vout, user_id, address, amount_msat, blockheight, credited_at) \
             VALUES ('aaa', 0, ?, 'bc1qfake', 100, NULL, 1000), \
                    ('bbb', 0, ?, 'bc1qfake', 200, NULL, 2000), \
                    ('ccc', 0, ?, 'bc1qfake', 300, NULL, 1500);",
        )
        .bind(alice)
        .bind(alice)
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();

        let rows = onchain_credits::list_for_user(&pool, alice).await.unwrap();
        let order: Vec<&str> = rows.iter().map(|r| r.txid.as_str()).collect();
        assert_eq!(
            order,
            vec!["bbb", "ccc", "aaa"],
            "list_for_user should sort by credited_at DESC"
        );

        // Other user sees their own (empty) list.
        let bob = make_user(&pool, "bob").await;
        let bob_rows = onchain_credits::list_for_user(&pool, bob).await.unwrap();
        assert!(bob_rows.is_empty());
    }

    // ---------- credit_onchain ----------

    #[tokio::test]
    async fn credit_onchain_basic_and_idempotent() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        // FK on onchain_credits.user_id requires the user; address
        // FK is implicit through application logic, not SQL.
        addresses::create(&pool, alice, "bc1qtest").await.unwrap();

        // First credit lands.
        let first = credit_onchain(&pool, "tx-aaa", 0, alice, "bc1qtest", 100_000, Some(150))
            .await
            .unwrap();
        assert!(first);
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 100_000);

        // Same (txid, vout) — should be a no-op.
        let second = credit_onchain(&pool, "tx-aaa", 0, alice, "bc1qtest", 100_000, Some(150))
            .await
            .unwrap();
        assert!(!second);
        assert_eq!(
            balance_msat(&pool, alice).await.unwrap(),
            100_000,
            "balance must not double on replay"
        );

        // Different vout of same tx — distinct UTXO, should credit.
        let third = credit_onchain(&pool, "tx-aaa", 1, alice, "bc1qtest", 50_000, Some(150))
            .await
            .unwrap();
        assert!(third);
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 150_000);
    }

    // ---------- settle_invoice (slice 4 credit-on-settle) ----------

    #[tokio::test]
    async fn settle_invoice_idempotent() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        make_invoice(&pool, alice, "inv1", 50_000).await;

        let first = settle_invoice(&pool, "inv1", 50_000).await.unwrap();
        assert!(first);
        assert_eq!(balance_msat(&pool, alice).await.unwrap(), 50_000);

        let second = settle_invoice(&pool, "inv1", 50_000).await.unwrap();
        assert!(!second);
        assert_eq!(
            balance_msat(&pool, alice).await.unwrap(),
            50_000,
            "duplicate notification must not double-credit"
        );
    }

    #[tokio::test]
    async fn settle_invoice_unknown_hash_is_noop() {
        let pool = fresh_pool().await;
        let credited = settle_invoice(&pool, "nope", 1_000).await.unwrap();
        assert!(!credited);
    }

    // ---------- token TTL + cleanup ----------

    #[tokio::test]
    async fn token_create_and_lookup() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        let (access, refresh) = tokens::create(&pool, alice).await.unwrap();

        assert_eq!(
            tokens::user_id_for_access(&pool, &access).await.unwrap(),
            Some(alice)
        );
        assert_eq!(
            tokens::user_id_for_refresh(&pool, &refresh).await.unwrap(),
            Some(alice)
        );
        assert_eq!(
            tokens::user_id_for_access(&pool, "bogus").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn token_ttl_filters_at_lookup() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        let (access, refresh) = tokens::create(&pool, alice).await.unwrap();

        // Backdate just past access TTL (7d). refresh (31d) still ok.
        // After migration 0008 the column is `access_token_hash` and
        // the stored value is `sha256(access)` — match by hash.
        sqlx::query("UPDATE tokens SET created_at = created_at - (7*24*3600 + 60) WHERE access_token_hash = ?")
            .bind(tokens::hash_token(&access))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            tokens::user_id_for_access(&pool, &access).await.unwrap(),
            None,
            "expired access should not authenticate"
        );
        assert_eq!(
            tokens::user_id_for_refresh(&pool, &refresh).await.unwrap(),
            Some(alice),
            "refresh still valid 7 days in"
        );

        // Backdate further: 32 days total. Refresh now also expired.
        sqlx::query("UPDATE tokens SET created_at = created_at - (25*24*3600) WHERE refresh_token_hash = ?")
            .bind(tokens::hash_token(&refresh))
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            tokens::user_id_for_refresh(&pool, &refresh).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn token_cleanup_removes_only_post_refresh_ttl() {
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        let (access_old, _) = tokens::create(&pool, alice).await.unwrap();
        let (_, _) = tokens::create(&pool, alice).await.unwrap(); // fresh

        // Push old token's created_at past the 31-day refresh TTL.
        sqlx::query("UPDATE tokens SET created_at = created_at - (32*24*3600) WHERE access_token_hash = ?")
            .bind(tokens::hash_token(&access_old))
            .execute(&pool)
            .await
            .unwrap();

        let removed = tokens::cleanup_expired(&pool).await.unwrap();
        assert_eq!(removed, 1);

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1, "fresh token must remain");
    }

    #[tokio::test]
    async fn token_not_stored_in_plaintext() {
        // Confirm migration 0008's hash-at-rest property: the raw
        // bearer string must not appear in the DB; only its digest.
        let pool = fresh_pool().await;
        let alice = make_user(&pool, "alice").await;
        let (access, refresh) = tokens::create(&pool, alice).await.unwrap();

        let row: (String, String) = sqlx::query_as(
            "SELECT access_token_hash, refresh_token_hash FROM tokens WHERE user_id = ?",
        )
        .bind(alice)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_ne!(row.0, access, "DB must not store access plaintext");
        assert_ne!(row.1, refresh, "DB must not store refresh plaintext");
        assert_eq!(row.0, tokens::hash_token(&access));
        assert_eq!(row.1, tokens::hash_token(&refresh));
    }
}
