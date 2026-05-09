-- migrations/0001_initial.sql
--
-- Initial schema. Two tables:
--   - users  : one row per LndHub account. The login is plaintext (it
--              IS the username), the password is argon2id-hashed.
--   - tokens : access_token + refresh_token pairs. LndHub does not
--              expire tokens; rotation simply mints a new pair while
--              the old one keeps working. We follow that for client
--              compatibility. Cleanup policy is a future decision.
--
-- All timestamps are unix epoch seconds (INTEGER) — SQLite has no
-- native timestamp type and storing as i64 makes the wire format
-- obvious from the schema.
--
-- This file is embedded into the binary at compile time by
-- `sqlx::migrate!("./migrations")` in src/db.rs and applied
-- automatically on every plugin start.

CREATE TABLE users (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    login           TEXT    NOT NULL UNIQUE,
    password_hash   TEXT    NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE INDEX users_login_idx ON users (login);

CREATE TABLE tokens (
    access_token    TEXT    NOT NULL PRIMARY KEY,
    refresh_token   TEXT    NOT NULL UNIQUE,
    user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at      INTEGER NOT NULL
);

CREATE INDEX tokens_refresh_idx ON tokens (refresh_token);
CREATE INDEX tokens_user_id_idx ON tokens (user_id);
