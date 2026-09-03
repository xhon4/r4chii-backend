-- 0006_server_roles.sql — M2: server roles, bitwise permissions, bans.
--
-- Forward-only, additive: no existing table (server, membership, channel) is
-- altered. `membership.role` (owner|member) is untouched and keeps meaning
-- exactly what it already means — the owner-bypass check reads it the same
-- way `ServerSummary::from` already does.
--
-- No BEGIN/COMMIT here on purpose — sqlx already wraps each migration in its
-- own transaction, and nesting them produces spurious warnings.

CREATE TABLE server_role (
    id           UUID PRIMARY KEY,
    server_id    UUID NOT NULL REFERENCES server(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    color        TEXT,
    permissions  BIGINT NOT NULL DEFAULT 0,
    position     INTEGER NOT NULL,
    is_default   BOOLEAN NOT NULL DEFAULT false,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_server_role_server ON server_role (server_id);
-- Exactly one default ("@everyone"-equivalent) role per server, enforced by
-- the DB rather than only app logic.
CREATE UNIQUE INDEX idx_server_role_one_default
    ON server_role (server_id) WHERE is_default;

CREATE TABLE membership_role (
    membership_id  UUID NOT NULL REFERENCES membership(id) ON DELETE CASCADE,
    role_id        UUID NOT NULL REFERENCES server_role(id) ON DELETE CASCADE,
    PRIMARY KEY (membership_id, role_id)
);

CREATE TABLE server_ban (
    id          UUID PRIMARY KEY,
    server_id   UUID NOT NULL REFERENCES server(id) ON DELETE CASCADE,
    account_id  UUID NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    banned_by   UUID NOT NULL REFERENCES account(id) ON DELETE RESTRICT,
    reason      TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (server_id, account_id)
);
CREATE INDEX idx_server_ban_server ON server_ban (server_id);

-- Backfill: every server created before this migration gets its default role
-- now, so `member_context` never has to treat "no default role exists" as a
-- case new code has to handle. `md5(...)::uuid` generates the one-off id
-- here rather than `gen_random_uuid()` — that's PG13+ only (built in, no
-- extension), and this project's own test harness runs postgres:11-alpine
-- (`testcontainers_modules::postgres::Postgres::default()`), where it does
-- not exist. `md5`/casting a 32-hex-digit string to `uuid` are both plain
-- core Postgres, no extension, and work on every version this project
-- targets. Not a row the app's own UUIDv7 id-ordering logic ever sorts by,
-- so its shape doesn't need to match.
INSERT INTO server_role (id, server_id, name, permissions, position, is_default)
SELECT md5(random()::text || clock_timestamp()::text)::uuid, id, 'everyone', 0, 0, true
FROM server;
