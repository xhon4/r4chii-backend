-- 0012_threads.sql — a thread is a `channel` row (`kind = 'thread'`),
-- the same way 0003_voice_channels.sql added `voice`. `message.channel_id`
-- keeps pointing at exactly one thing (a load-bearing invariant) — a
-- thread's messages are `channel_id`-scoped like any other channel's, so
-- no message/realtime code changes.
--
-- Same approach as 0003_voice_channels.sql for the two CHECK constraints:
-- drop and recreate under their auto-generated names rather than edit in
-- place (0001_init.sql is frozen).

ALTER TABLE channel ADD COLUMN parent_channel_id UUID REFERENCES channel(id) ON DELETE CASCADE;
-- ON DELETE SET NULL, not CASCADE: deleting the message a thread was spawned
-- from must not delete the thread's own archive of replies underneath it.
ALTER TABLE channel ADD COLUMN root_message_id   UUID REFERENCES message(id) ON DELETE SET NULL;
-- Threads carry a `title` (shown in search/the public read path); plain
-- text/voice channels keep using the existing `name` column, untouched.
ALTER TABLE channel ADD COLUMN title             TEXT;
-- URL-safe, unique per parent (not globally) — see the partial unique index
-- below. A bare channel id always resolves regardless of slug drift.
ALTER TABLE channel ADD COLUMN slug              TEXT;

ALTER TABLE channel DROP CONSTRAINT channel_kind_check;
ALTER TABLE channel ADD CONSTRAINT channel_kind_check
    CHECK (kind IN ('text','voice','thread','dm','group_dm'));

ALTER TABLE channel DROP CONSTRAINT channel_check;
ALTER TABLE channel ADD CONSTRAINT channel_check
    CHECK (
        (kind IN ('text','voice')      AND server_id IS NOT NULL AND parent_channel_id IS NULL) OR
        (kind = 'thread'               AND server_id IS NOT NULL AND parent_channel_id IS NOT NULL) OR
        (kind IN ('dm','group_dm')     AND server_id IS NULL     AND parent_channel_id IS NULL)
    );

CREATE INDEX idx_channel_parent ON channel (parent_channel_id);
CREATE UNIQUE INDEX idx_channel_thread_slug
    ON channel (parent_channel_id, slug) WHERE kind = 'thread';
