ALTER TABLE channel ADD COLUMN deleted_at TIMESTAMPTZ;
ALTER TABLE channel ADD COLUMN public_tombstone_eligible BOOLEAN;
