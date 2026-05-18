-- migrations/0008_tokens_hashed.sql
--
-- Store tokens as SHA-256 hex digests, not plaintext.
--
-- Why: the old schema stored tokens verbatim and the lookup query
-- compared them byte-for-byte via SQLite's `=` operator. That's a
-- timing oracle — short-circuiting on the first mismatched byte leaks
-- prefix information. With `/auth`'s refresh-token path being
-- unauthenticated, a co-located attacker could in principle inch their
-- way to a valid token by measuring response latency.
--
-- After this migration, the DB stores `sha256(token)` and the lookup
-- path hashes the supplied token before comparing. The hash space has
-- a uniform-prefix distribution, so byte-by-byte mismatch position
-- carries no useful signal about the underlying token value. The
-- token itself never lives on disk; a DB dump leaks nothing directly
-- usable as a bearer credential.
--
-- Pre-mainnet, all existing tokens are invalidated. Users (the
-- regtest harness's Zeus session, mainly) must re-`/auth` to mint a
-- fresh hashed pair.

DROP TABLE tokens;

CREATE TABLE tokens (
    access_token_hash   TEXT    NOT NULL PRIMARY KEY,
    refresh_token_hash  TEXT    NOT NULL UNIQUE,
    user_id             INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at          INTEGER NOT NULL
);

CREATE INDEX tokens_user_id_idx ON tokens (user_id);
