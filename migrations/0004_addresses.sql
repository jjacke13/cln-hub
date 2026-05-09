-- migrations/0004_addresses.sql
--
-- Per-user on-chain deposit address.
--
-- LndHub semantics: each user has at most ONE deposit address. The
-- first /getbtc call mints a fresh CLN address (via `newaddr`) and
-- stores it. Every subsequent /getbtc call returns the same address.
--
-- IMPORTANT — known limitation as of slice 5d:
--   We hand out the address but we do NOT yet credit the user's
--   internal balance when funds arrive. A deposit goes to CLN's
--   on-chain wallet; the cln-hub `ledger` is unaffected. A future
--   slice will add a polling task that watches `lightning-cli
--   listfunds` for new confirmed outputs to addresses in this table
--   and credits the owning user.

CREATE TABLE addresses (
    user_id     INTEGER NOT NULL PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    address     TEXT    NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL
);

CREATE INDEX addresses_address_idx ON addresses (address);
