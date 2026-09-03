-- 0002_server_invite_code.sql — adds server.invite_code (M0 slice 4)
-- Forward-only: 0001_init.sql is frozen, this is a new numbered migration.
-- Safe as a plain NOT NULL UNIQUE add with no default: this runs on a
-- fresh/empty `server` table in every environment that exists right now, so
-- there are no pre-existing rows to violate the constraint.

BEGIN;

ALTER TABLE server ADD COLUMN invite_code TEXT NOT NULL UNIQUE;

COMMIT;
