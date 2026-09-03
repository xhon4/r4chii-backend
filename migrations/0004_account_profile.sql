-- 0004_account_profile.sql — profile fields behind the mini popout and the
-- full profile view.
--
-- No BEGIN/COMMIT: sqlx already wraps each migration in its own transaction.
--
-- Every column is nullable, so existing accounts stay valid without a
-- backfill. `account.created_at` already exists and is the "member since"
-- for the global profile; `membership.joined_at` is the per-server one, so
-- server identity needs no schema change here.
--
-- The CHECK constraints below duplicate validation that also lives in the
-- service layer. That is deliberate: the service owns the readable error
-- message, and the database owns the guarantee. A bug in one is caught by
-- the other, and neither is load-bearing alone.

ALTER TABLE account
    ADD COLUMN bio TEXT,
    ADD COLUMN banner_url TEXT,
    ADD COLUMN accent_color TEXT,
    ADD COLUMN pronouns TEXT;

-- char_length, not octet_length: 190 characters, so an accented or non-Latin
-- bio is not silently shorter than an ASCII one.
ALTER TABLE account
    ADD CONSTRAINT account_bio_length CHECK (bio IS NULL OR char_length(bio) <= 190);

ALTER TABLE account
    ADD CONSTRAINT account_banner_url_length
        CHECK (banner_url IS NULL OR char_length(banner_url) <= 2048);

-- Exactly #RRGGBB. Stored as written; comparisons that care about case are
-- the reader's problem, since the client only ever renders it.
ALTER TABLE account
    ADD CONSTRAINT account_accent_color_format
        CHECK (accent_color IS NULL OR accent_color ~ '^#[0-9A-Fa-f]{6}$');

-- Exactly one slash, 1-5 letters either side. [[:alpha:]] is Unicode-aware in
-- a UTF-8 database, which is the point: an ASCII-only class would reject
-- "él/ella", and this app is written in Spanish first.
ALTER TABLE account
    ADD CONSTRAINT account_pronouns_format
        CHECK (pronouns IS NULL OR pronouns ~ '^[[:alpha:]]{1,5}/[[:alpha:]]{1,5}$');
