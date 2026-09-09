-- "ur spaces" is a curated subset of the servers an account belongs to, not
-- every membership: NULL means the server isn't in it, a value is its
-- position in the curated order. Lives on `membership` rather than a new
-- table because it only ever makes sense for a row that already exists there.
ALTER TABLE membership ADD COLUMN spaces_position INTEGER NULL;
