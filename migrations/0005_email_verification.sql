-- 0005_email_verification.sql — email verification.
--
-- No BEGIN/COMMIT: sqlx already wraps each migration in its own transaction.
--
-- Two changes, and the second is the interesting one.
--
-- `account.email_verified_at` is left NULL for every existing row on purpose.
-- New accounts are born with it set — after this migration an account cannot
-- come into existence without a proven address — so NULL means exactly one
-- thing: "predates verification". Those accounts are NOT blocked; they are
-- prompted from settings. A retroactive hard gate would lock out every
-- current user on the first deploy of a subsystem that has never run.
--
-- `pending_registration` holds a registration that has not proven its address
-- yet. There is deliberately no foreign key to `account`, because the whole
-- point of this design is that no account exists until verification promotes
-- the row. It carries an argon2 hash and so deserves the same care as `account`;
-- expired rows are purged rather than accumulated.

ALTER TABLE account
    ADD COLUMN email_verified_at TIMESTAMPTZ;

CREATE TABLE pending_registration (
    id             UUID PRIMARY KEY,
    email          TEXT NOT NULL UNIQUE,
    username       TEXT NOT NULL UNIQUE,
    display_name   TEXT NOT NULL,
    password_hash  TEXT NOT NULL,                -- argon2, same as auth_identity
    code_digest    TEXT NOT NULL,                -- SHA-256 hex of the 8-digit code
    attempts       INTEGER NOT NULL DEFAULT 0,
    expires_at     TIMESTAMPTZ NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The purge sweeps by expiry, and issuing a code re-reads the row by email.
CREATE INDEX idx_pending_registration_expires ON pending_registration (expires_at);

-- The UNIQUE constraints above are what soft-reserve the name and address for
-- the life of the row. They hold only while the pending row lives,
-- which is the fifteen-minute window plus whatever time passes before the
-- purge runs — long enough to stop two concurrent registrations colliding,
-- short enough that squatting the namespace is not practical.
ALTER TABLE pending_registration
    ADD CONSTRAINT pending_registration_attempts_non_negative CHECK (attempts >= 0);
