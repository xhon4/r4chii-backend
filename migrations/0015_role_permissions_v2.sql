-- 0015_role_permissions_v2.sql — seven new server-wide permission
-- bits (MENTION_EVERYONE, MENTION_ROLES, MANAGE_NICKNAMES, TIMEOUT_MEMBERS,
-- MANAGE_MESSAGES, PIN_MESSAGES, MANAGE_INVITES) and the three columns their
-- actions need. Every default below is the correct value for every existing
-- row (additive, no backfill logic needed):
--
--   membership.timeout_until — NULL means "not timed out", which is what
--   every existing membership already is.
--
--   message.pinned_at — NULL means "not pinned", same reasoning.
--
--   server_role.mentionable — false is the safer default (a role isn't
--   `@`-mentionable by everyone until an admin opts it in), and matches how
--   every other permission-shaped column in this schema defaults closed.

ALTER TABLE membership ADD COLUMN timeout_until TIMESTAMPTZ;
-- Mirrors `server_ban.reason` (M2) — same optional, caller-supplied context,
-- not persisted anywhere it would need to be if left NULL.
ALTER TABLE membership ADD COLUMN timeout_reason TEXT;

ALTER TABLE message ADD COLUMN pinned_at TIMESTAMPTZ;

ALTER TABLE server_role ADD COLUMN mentionable BOOLEAN NOT NULL DEFAULT false;
