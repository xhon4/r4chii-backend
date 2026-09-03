-- 0010_visibility_model.sql — channel-level visibility override.
--
-- `channel.visibility` is nullable and shares the exact same three values as
-- `server.visibility` (0001_init.sql). NULL means "inherit the server's
-- visibility unchanged"; a non-NULL value narrows it. Every existing channel
-- gets NULL by construction (a new nullable column with no default), so no
-- server's effective behavior changes — this migration is schema plumbing
-- only. The rule that an override may not be BROADER than the server's own
-- visibility is enforced in the service layer (crates/domain), not here —
-- it needs to read the parent server row, which a table-level CHECK cannot
-- do.

ALTER TABLE channel ADD COLUMN visibility TEXT
    CHECK (visibility IN ('public','unlisted','private'));
