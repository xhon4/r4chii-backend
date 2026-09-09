-- Channel permissions currently define only VIEW_CHANNEL (bit 0). Retain that
-- bit and preserve every existing grant row, including rows that become zero.
UPDATE channel_role_permission
SET permissions = permissions & 1
WHERE permissions <> (permissions & 1);
