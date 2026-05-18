-- migrations/0006_payments_unique_active.sql
--
-- Slice 5b — atomic concurrency guard for /payinvoice.
--
-- The narrow partial unique index from migration 0003 covered only
-- terminal-successful states (`internal`, `external_settled`). That
-- prevented two terminal-settled rows for the same invoice but had
-- a subtle hole: a SECOND attempt to pay an invoice the user had
-- ALREADY paid would slip past the index at INSERT (status starts
-- as `external_pending`, outside the old WHERE filter), proceed to
-- call CLN `pay` (which is idempotent and returns the same preimage),
-- and finally trip the old index inside `settle_external_pay` when
-- the row tried to move pending → settled. The user would see HTTP
-- 500 instead of the intended code 7 "already attempted".
--
-- Fix: replace the index with a unified one that ALSO covers
-- `external_pending`. Now `reserve_external_pay`'s INSERT fails
-- up-front whenever ANY active payment exists for (user, hash) —
-- terminal or in-flight — and the handler returns code 7 cleanly.
--
-- `external_failed` is deliberately excluded so a user can retry the
-- same invoice after a terminal failure.

DROP INDEX payments_user_hash_settled_idx;

CREATE UNIQUE INDEX payments_user_hash_active_idx
    ON payments (user_id, payment_hash)
    WHERE status IN ('internal', 'external_settled', 'external_pending');
