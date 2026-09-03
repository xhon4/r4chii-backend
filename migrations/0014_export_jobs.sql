-- 0014_export_jobs.sql — full export as an async job.
--
-- `download_url` is populated once, by the worker, at job completion —
-- alongside `storage_key` (the permanent object reference) it caches the
-- presigned URL computed at that moment rather than re-presigning on every
-- GET. This is a deliberate simplification versus a "generate a fresh
-- presigned URL per request" design: it means a request more than
-- ~24h after completion sees a dead link instead of a fresh one, but it
-- keeps `crates/api`'s `AppState` free of a `StorageService` dependency —
-- every request handler in this codebase today only ever needs
-- `db`/`domain`/`realtime`, and adding storage there would touch every
-- existing test's app-construction helper for one feature's sake. No
-- inbound port or process is opened by this choice; it's a data-shape
-- decision only.

CREATE TABLE export_job (
    id            UUID PRIMARY KEY,
    server_id     UUID NOT NULL REFERENCES server(id) ON DELETE CASCADE,
    requested_by  UUID NOT NULL REFERENCES account(id) ON DELETE RESTRICT,
    status        TEXT NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending','running','done','failed')),
    storage_key   TEXT,
    download_url  TEXT,
    error         TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at  TIMESTAMPTZ
);
CREATE INDEX idx_export_job_server ON export_job (server_id, created_at DESC);
