-- Track actual CLN receipts separately from legacy internal ledger transfers.
-- Backfill external credits so an upgrade never replays money already credited.
CREATE TABLE invoice_receipts (
    payment_hash TEXT NOT NULL PRIMARY KEY REFERENCES invoices(payment_hash)
);

INSERT INTO invoice_receipts (payment_hash)
SELECT DISTINCT invoices.payment_hash FROM invoices
JOIN ledger ON ledger.ref_hash = invoices.payment_hash
WHERE ledger.kind = 'invoice_settled';
