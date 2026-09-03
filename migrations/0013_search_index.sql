-- 0013_search_index.sql — Postgres native full-text search.
--
-- Plain (non-generated) `tsvector` column, maintained by the application at
-- write time (`db::message::insert`/`update_content` now include
-- `to_tsvector('english', $content)` directly in their INSERT/UPDATE), NOT
-- `GENERATED ALWAYS AS (...) STORED` — that syntax is PG12+ only, and
-- `0006_server_roles.sql` already established that this project's test
-- harness and (per that same comment) deployment target run
-- `postgres:11-alpine`. Same constraint, same workaround shape as that
-- migration's own `gen_random_uuid()` avoidance.
--
-- Backfilled for any row that predates this migration (this project has a
-- deployed instance with real messages, not just fixtures) so the GIN index
-- is complete from the moment it exists, not lazily populated on next edit.

ALTER TABLE message ADD COLUMN search_vector tsvector;
UPDATE message SET search_vector = to_tsvector('english', coalesce(content, ''));
CREATE INDEX idx_message_search ON message USING GIN (search_vector);
