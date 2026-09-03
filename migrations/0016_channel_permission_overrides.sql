-- 0016_channel_permission_overrides.sql — channel visibility per
-- role. Deliberately NOT Discord's allow/deny/inherit model, and not even
-- Nerimity's own 3-bit CHANNEL_PERMISSIONS — a plain grant list, OR'd across
-- a member's held roles, no deny semantics at all. A plain grant list is
-- enough for the actual request ("a #staff channel only Staff can see") —
-- a single-purpose channel gate has no need for deny semantics or
-- inheritance.
--
-- `restricted = false` (every existing channel, and every new one unless
-- explicitly flipped) is unaffected by this migration — the containment
-- check this adds is skipped entirely for an unrestricted channel, so
-- nothing about an ordinary server's behavior changes today.

ALTER TABLE channel ADD COLUMN restricted BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE channel_role_permission (
    channel_id  UUID NOT NULL REFERENCES channel(id) ON DELETE CASCADE,
    role_id     UUID NOT NULL REFERENCES server_role(id) ON DELETE CASCADE,
    -- crates/domain/src/channel_permissions.rs's own bitmask, a separate
    -- namespace from crates/domain/src/permissions.rs's server-wide one —
    -- see the ADR for why conflating the two bit spaces would be a mistake.
    permissions BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (channel_id, role_id)
);
CREATE INDEX idx_channel_role_permission_channel ON channel_role_permission (channel_id);
