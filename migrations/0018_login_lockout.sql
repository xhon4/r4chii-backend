-- 0018_login_lockout.sql — per-account lockout after repeated failed logins.
--
-- No BEGIN/COMMIT: sqlx wraps each migration in its own transaction.
--
-- IP-based rate limiting in the api crate does not stop a distributed
-- credential-stuffing attempt against one specific account. This adds the
-- complementary per-account counter: five consecutive failed logins lock the
-- account for fifteen minutes, and any successful login resets the counter.

ALTER TABLE account
    ADD COLUMN failed_login_attempts INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN locked_until           TIMESTAMPTZ;

ALTER TABLE account
    ADD CONSTRAINT account_failed_login_attempts_non_negative CHECK (
        failed_login_attempts >= 0
    );
