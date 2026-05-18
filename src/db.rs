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

    Ok(pool)
}

// =====================================================================
// users
// =====================================================================

/// Operations on the `users` table.
pub mod users {
    use super::{anyhow, unix_now, Pool, Result};

    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

    /// Insert a new user. The plaintext password is hashed with
    /// argon2id (default params, ~64 MiB memory, 3 rounds, 4 lanes —
    /// the OWASP recommendation as of 2024).
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

        if Argon2::default()
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
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            // `password_hash::Error` doesn't impl `std::error::Error`
            // (yet), so anyhow can't auto-convert it; we map manually.
            .map_err(|e| anyhow!("argon2 hashing failure: {}", e))?
            .to_string();
        Ok(hash)
    }
}

// =====================================================================
// tokens
// =====================================================================

/// Operations on the `tokens` table. Each row pairs an `access_token`
/// (used in `Authorization: Bearer <...>`) with a `refresh_token`
/// (used to mint new tokens) and the `user_id` they belong to.
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
/// are inert); a periodic cleanup task can be added later if disk
/// pressure ever shows up.
pub mod tokens {
    use super::{random_hex, unix_now, Pool, Result};

    pub const ACCESS_TTL_SECS: i64 = 7 * 24 * 60 * 60;
    pub const REFRESH_TTL_SECS: i64 = 31 * 24 * 60 * 60;

    /// Mint a new access/refresh token pair for `user_id`. Both tokens
    /// are 20 bytes of OS-randomness, hex-encoded (40 chars).
    pub async fn create(pool: &Pool, user_id: i64) -> Result<(String, String)> {
        let access = random_hex(20);
        let refresh = random_hex(20);

        sqlx::query(
            "INSERT INTO tokens (access_token, refresh_token, user_id, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&access)
        .bind(&refresh)
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
            "SELECT user_id FROM tokens WHERE access_token = ? AND created_at >= ?",
        )
        .bind(access_token)
        .bind(cutoff)
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|(id,)| id))
    }

    /// Resolve a refresh_token to a user_id, applying the 31-day TTL.
    pub async fn user_id_for_refresh(pool: &Pool, refresh_token: &str) -> Result<Option<i64>> {
        let cutoff = unix_now() - REFRESH_TTL_SECS;
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT user_id FROM tokens WHERE refresh_token = ? AND created_at >= ?",
        )
        .bind(refresh_token)
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
/// Effects (atomic):
///   - payments row → `external_settled`, with preimage + actual fee.
///   - If `fee_reserve_msat > actual_fee_msat`, credit the difference
///     back to the user (`payment_fee_refund`).
///
/// Idempotency: the UPDATE is gated on `status = 'external_pending'`,
/// so a second call simply UPDATES zero rows. We then return an
/// error so the caller can log "already finalized" if needed. (The
/// reconciler treats this as a no-op.)
pub async fn settle_external_pay(
    pool: &Pool,
    user_id: i64,
    payment_hash: &str,
    preimage: &str,
    actual_fee_msat: i64,
    fee_reserve_msat: i64,
) -> Result<()> {
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
        tx.rollback().await?;
        return Err(anyhow::anyhow!(
            "settle_external_pay: no pending row for user {} hash {}",
            user_id,
            payment_hash
        ));
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
    Ok(())
}

/// Finalize a terminally-failed external payment.
///
/// Effects (atomic):
///   - payments row → `external_failed`.
///   - Compensating credit for the FULL reserved amount (amount + fee
///     reserve) — the user is made whole.
pub async fn fail_external_pay(
    pool: &Pool,
    user_id: i64,
    payment_hash: &str,
    amount_msat: i64,
    fee_reserve_msat: i64,
) -> Result<()> {
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
        return Err(anyhow::anyhow!(
            "fail_external_pay: no pending row for user {} hash {}",
            user_id,
            payment_hash
        ));
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
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}
