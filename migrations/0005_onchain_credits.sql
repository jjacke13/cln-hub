-- migrations/0005_onchain_credits.sql
--
-- One row per UTXO we've credited to a user. The (txid, vout)
-- primary key plus the application-side INSERT-OR-IGNORE pattern
-- make the credit-on-deposit path idempotent: if the watcher
-- restarts and replays old listfunds output, we won't double-credit.
--
-- The corresponding `ledger` entry (kind='onchain_in') is inserted
-- in the same SQLite transaction as this row, so a crash partway
-- through cannot leave the bookkeeping inconsistent.

CREATE TABLE onchain_credits (
    txid          TEXT    NOT NULL,
    vout          INTEGER NOT NULL,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    address       TEXT    NOT NULL,
    amount_msat   INTEGER NOT NULL,
    blockheight   INTEGER,
    credited_at   INTEGER NOT NULL,
    PRIMARY KEY (txid, vout)
);

CREATE INDEX onchain_credits_user_id_idx ON onchain_credits (user_id);
CREATE INDEX onchain_credits_address_idx ON onchain_credits (address);
