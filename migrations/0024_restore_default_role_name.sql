-- Restore the canonical name for every implicit default role without changing
-- its permissions or other role attributes.
UPDATE server_role
SET name = 'everyone'
WHERE is_default
  AND name IS DISTINCT FROM 'everyone';
