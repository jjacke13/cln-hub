-- migrations/0003_payments.sql
--
-- Slice 5a schema: outbound payments.
--
-- A payment row is created when a user calls /payinvoice. Status
-- starts as one of:
--   - 'internal'         : settled atomically against another local
--                          user's invoice, never touched the network
--   - 'external_pending' : sent to CLN's `pay`, awaiting result (5b)
--
-- and may transition to:
--   - 'external_settled' : `pay` returned success
--   - 'external_failed'  : `pay` returned failure (and we've written
--                          a compensating credit to the ledger)
--
-- (id, not payment_hash, is the PK so a user can have multiple
-- attempts against the same invoice when the first fails.)
--
-- The UNIQUE (user_id, payment_hash) where status IN ('internal',
-- 'external_settled') would be ideal but SQLite supports only
-- partial unique indexes via separate CREATE INDEX. We enforce
-- "no double-settle" application-side via the atomic transaction
-- in `db::try_settle_internal`.

CREATE TABLE payments (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    payment_hash    TEXT    NOT NULL,
    user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    bolt11          TEXT    NOT NULL,
    amount_msat     INTEGER NOT NULL,
    fee_msat        INTEGER NOT NULL DEFAULT 0,
    preimage        TEXT,                 -- NULL for pending or failed
    status          TEXT    NOT NULL,     -- see comment above
    memo            TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    settled_at      INTEGER               -- NULL until settled (or terminal-failed)
);

CREATE INDEX payments_user_id_idx     ON payments (user_id);
CREATE INDEX payments_payment_hash_idx ON payments (payment_hash);

-- Partial unique index: a single user cannot have two successful
-- payments to the same invoice. (Failed attempts don't count.)
CREATE UNIQUE INDEX payments_user_hash_settled_idx
    ON payments (user_id, payment_hash)
    WHERE status IN ('internal', 'external_settled');
