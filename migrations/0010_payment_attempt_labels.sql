-- Existing pending payments retain NULL because older CLN calls had no label.
ALTER TABLE payments ADD COLUMN cln_label TEXT;
