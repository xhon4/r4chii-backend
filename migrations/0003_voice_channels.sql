-- 0003_voice_channels.sql — allows `channel.kind = 'voice'`.
--
-- Forward-only: 0001_init.sql is frozen, so both of its CHECK constraints on
-- `channel` are dropped and recreated here rather than edited in place. The
-- names below are Postgres's auto-generated ones from 0001 (confirmed with
-- `\d channel`), not guesses: the column-level `kind IN (...)` check became
-- `channel_kind_check`, and the table-level kind/server_id pairing check
-- became `channel_check`.
--
-- A `voice` channel lives in a server exactly like a `text` channel does, so
-- it joins the non-NULL-server_id side of the pairing constraint. This adds
-- the shape only: media transport (LiveKit/WebRTC) is still unbuilt and
-- still gated on the outcome of the latency spike.
--
-- No BEGIN/COMMIT here on purpose — sqlx already wraps each migration in its
-- own transaction, and nesting them produces spurious warnings.

ALTER TABLE channel DROP CONSTRAINT channel_kind_check;
ALTER TABLE channel ADD CONSTRAINT channel_kind_check
    CHECK (kind IN ('text','voice','dm','group_dm'));

ALTER TABLE channel DROP CONSTRAINT channel_check;
ALTER TABLE channel ADD CONSTRAINT channel_check
    CHECK (
        (kind IN ('text','voice')  AND server_id IS NOT NULL) OR
        (kind IN ('dm','group_dm') AND server_id IS NULL)
    );
