-- Separates the username people type from the username that identifies an
-- account. `username` keeps whatever casing its owner chose and is what every
-- surface renders; `username_normalized` is what must be unique, so `Ada` and
-- `ada` can no longer be two accounts.
--
-- Postgres owns the normalization. The column is generated, so no application
-- code writes it and no second implementation can drift from this one; a
-- lookup normalizes its input with the same two functions and compares.
--
-- `normalize(..., NFKC)` folds compatibility forms, so `ＡＤＡ` and `ADA`
-- resolve to the same account. It is a no-op for the ASCII-only usernames
-- `validate_username` accepts today, and is here for the case that rule ever
-- widens.
--
-- This does NOT address visually confusable characters across scripts
-- (Cyrillic `о` against Latin `o`). That is a different problem with a
-- different fix, and folding it in here would hide it.
ALTER TABLE account
    ADD COLUMN username_normalized TEXT
        GENERATED ALWAYS AS (lower(normalize(username, NFKC))) STORED;

-- Fails loudly if two existing rows normalize to the same value; they must be
-- reconciled before this migration can apply.
CREATE UNIQUE INDEX account_username_normalized_key
    ON account (username_normalized);

-- Uniqueness now lives on the normalized column. Leaving the case-sensitive
-- constraint in place would keep a second name for the same rule and let a
-- conflict report whichever of the two it happened to hit.
ALTER TABLE account
    DROP CONSTRAINT account_username_key;
