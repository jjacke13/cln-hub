-- migrations/0002_invoices_ledger.sql
--
-- Slice 4 schema: per-invoice records and an append-only ledger.
--
-- invoices: one row per BOLT11 we issued via /addinvoice. We keep our
--           own `label` (the unique tag we passed to CLN's `invoice`
--           RPC) so we can match the inbound `invoice_payment`
--           notification back to the owning user.
--
-- ledger:   append-only credit/debit log. The user's balance is
--           computed as `SUM(amount_msat) WHERE user_id = ?`. Credits
--           are positive; debits (slice 5: outbound payments) will be
--           negative. Atomic balance changes are wrapped in a sqlx
--           transaction that updates `invoices.settled_*` and inserts
--           the ledger row in one shot — a crash between the two
--           cannot leave us out of sync.
--
-- Idempotency: the credit-on-settle path conditions its UPDATE on
--   `settled_at IS NULL`. If `invoice_payment` fires twice for the
--   same invoice, the second one updates zero rows and the credit
--   is skipped. No risk of double-credit.

CREATE TABLE invoices (
    payment_hash    TEXT    NOT NULL PRIMARY KEY,  -- 32-byte hash, hex-encoded
    label           TEXT    NOT NULL UNIQUE,       -- our internal tag, e.g. "cln-hub:abcdef01"
    user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    amount_msat     INTEGER NOT NULL,              -- requested amount (msat)
    memo            TEXT    NOT NULL DEFAULT '',
    bolt11          TEXT    NOT NULL,              -- BOLT11 payment_request
    expires_at      INTEGER NOT NULL,              -- unix epoch seconds
    created_at      INTEGER NOT NULL,
    settled_at      INTEGER,                       -- NULL => unpaid
    settled_msat    INTEGER                        -- NULL => unpaid; actual amount received
);

CREATE INDEX invoices_user_id_idx ON invoices (user_id);
CREATE INDEX invoices_label_idx   ON invoices (label);

CREATE TABLE ledger (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind            TEXT    NOT NULL,              -- 'invoice_settled', 'payment_sent', 'internal_in', 'internal_out'
    amount_msat     INTEGER NOT NULL,              -- positive = credit, negative = debit
    ref_hash        TEXT,                          -- payment_hash this entry refers to (NULL for adjustments)
    description     TEXT,
    created_at      INTEGER NOT NULL
);

CREATE INDEX ledger_user_id_idx  ON ledger (user_id);
CREATE INDEX ledger_ref_hash_idx ON ledger (ref_hash);
