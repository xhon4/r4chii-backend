-- `custom_emoji` was the one profile column with no bound at all, so an
-- application path that skipped validation could store arbitrary text in it.
--
-- The bound is in bytes and deliberately generous. The real rule is one
-- grapheme cluster, which the application enforces; a single sequence can run
-- to about 41 bytes (a four-person family carrying skin tone modifiers), and
-- expressing "one grapheme" in a CHECK is not something Postgres offers. This
-- is the floor under that rule, not the rule.
ALTER TABLE account
    ADD CONSTRAINT account_custom_emoji_bytes CHECK (
        custom_emoji IS NULL OR octet_length(custom_emoji) <= 64
    );
