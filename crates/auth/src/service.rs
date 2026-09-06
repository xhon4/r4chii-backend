use std::sync::Arc;

use app_core::{new_id, AuthContext, Uuid};
use chrono::{DateTime, Utc};
use db::PgPool;
use mailer::{Mailer, OutgoingMail};

use crate::crypto::{
    dummy_password_hash, generate_session_token, hash_password, hash_token, verify_password,
};
use crate::error::AuthError;
use crate::types::{
    AccountSummary, LoginInput, RegisterInput, SessionSummary, UpdateAccountInput,
    VerifyRegistrationInput,
};
use crate::validation::{
    normalize_email, validate_accent_color, validate_avatar_url, validate_banner_url,
    validate_bio, validate_display_name, validate_email, validate_password, validate_pronouns,
    validate_username,
};
use crate::verification::{
    code_matches, digest_code, generate_code, CODE_TTL_MINUTES, MAX_ATTEMPTS,
    RESEND_COOLDOWN_SECONDS,
};

/// Flat cap for every account in M0 — there is no tier column and no
/// monetization yet, so the "4 free / 10 premium" split does not
/// apply until a paid tier exists (M2+).
const MAX_CONCURRENT_SESSIONS: i64 = 4;

/// Idle sliding window in days, measured from `last_used_at`. `verify_session`
/// bumps `last_used_at` on every successful check, which is what makes this
/// a *sliding* window rather than a fixed expiry from session creation.
const IDLE_TIMEOUT_DAYS: i64 = 14;

/// Absolute hard cap in days, regardless of activity.
const ABSOLUTE_LIFETIME_DAYS: i64 = 90;

/// The single definition of "this session still counts": not revoked, still
/// inside its absolute lifetime, and touched within the idle window. The
/// login quota, `verify_session`, and `list_sessions` all filter through this
/// one fragment so none of them can drift from what the others consider live
/// — a session past the idle window is already unusable to `verify_session`,
/// so it must not occupy a login slot or appear as active in a listing.
fn session_eligibility_sql() -> String {
    format!(
        "revoked_at IS NULL \
         AND absolute_expires_at > now() \
         AND last_used_at + interval '{IDLE_TIMEOUT_DAYS} days' > now()"
    )
}

/// Accounts, auth identities, and sessions — the whole surface of this
/// crate's session model. Wraps a `db::PgPool` directly (auth owns
/// its own queries against `account`/`auth_identity`/`session`, a
/// crate-boundary rule) so `api` can depend on `auth` alone, never on `db`.
#[derive(Clone)]
pub struct AuthService {
    pool: PgPool,
    mailer: Arc<dyn Mailer>,
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: Uuid,
    username: String,
    email: String,
    display_name: String,
    avatar_url: Option<String>,
    bio: Option<String>,
    banner_url: Option<String>,
    accent_color: Option<String>,
    pronouns: Option<String>,
    created_at: DateTime<Utc>,
    email_verified_at: Option<DateTime<Utc>>,
}

/// The column list every account query selects. One definition, so a new
/// profile field cannot be added to one query and forgotten in another —
/// which `FromRow` would only catch at runtime.
///
/// A macro rather than a `const` on purpose: `concat!` needs literals, and
/// building the SQL with `format!` instead would hand sqlx a runtime string,
/// which it refuses without an explicit injection audit. This way every query
/// below is still a single compile-time literal.
macro_rules! account_columns {
    () => {
        "id, username, email, display_name, avatar_url, bio, banner_url, accent_color, \
         pronouns, created_at, email_verified_at"
    };
}

impl From<AccountRow> for AccountSummary {
    fn from(row: AccountRow) -> Self {
        Self {
            id: row.id,
            username: row.username,
            email: row.email,
            display_name: row.display_name,
            avatar_url: row.avatar_url,
            bio: row.bio,
            banner_url: row.banner_url,
            accent_color: row.accent_color,
            pronouns: row.pronouns,
            created_at: row.created_at,
            email_verified_at: row.email_verified_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct PendingRegistrationRow {
    id: Uuid,
    email: String,
    username: String,
    display_name: String,
    password_hash: String,
    code_digest: String,
    attempts: i32,
}

#[derive(sqlx::FromRow)]
struct AccountIdRow {
    id: Uuid,
}

#[derive(sqlx::FromRow)]
struct PasswordHashRow {
    password_hash: Option<String>,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: Uuid,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
    absolute_expires_at: DateTime<Utc>,
}

impl From<SessionRow> for SessionSummary {
    fn from(row: SessionRow) -> Self {
        Self {
            id: row.id,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            expires_at: row.absolute_expires_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct SessionIdentityRow {
    id: Uuid,
    account_id: Uuid,
}

impl AuthService {
    pub fn new(pool: PgPool, mailer: Arc<dyn Mailer>) -> Self {
        Self { pool, mailer }
    }

    /// Starts a registration. Creates no account yet: the row that
    /// lands is a `pending_registration`, and the account only comes into
    /// existence when `verify_registration` promotes it.
    ///
    /// Succeeds even when the email or username is already taken, and sends no
    /// mail in that case. Reporting the clash here would turn registration
    /// into an oracle for "is this address registered", which is precisely
    /// what a login endpoint is careful never to be. The caller who genuinely
    /// owns the address finds out at the next step; the caller who is probing
    /// learns nothing.
    pub async fn register(&self, input: RegisterInput) -> Result<(), AuthError> {
        validate_email(&input.email)?;
        validate_username(&input.username)?;
        validate_password(&input.password)?;
        validate_display_name(&input.display_name)?;

        // Canonicalized once, here, and used for every lookup, guard, and
        // stored value below — never `input.email` again in this function.
        let email = normalize_email(&input.email);

        // Hash before the existence check, not after. Skipping the argon2 work
        // on the "already registered" path would make that response measurably
        // faster and hand back the oracle the identical response text just
        // took away.
        let password_hash = hash_password(&input.password)?;

        // Username and email are treated differently on purpose.
        //
        // A username clash is reported plainly: usernames are public — they
        // show up in every member list — so saying "that one is taken" reveals
        // nothing an attacker could not read off a channel, and staying silent
        // would leave someone waiting for a mail that is never coming.
        //
        // An email clash is silent, because *that* is the private fact. Saying
        // "already registered" would turn this endpoint into the enumeration
        // oracle that login is careful never to be.
        let username_taken = sqlx::query_as::<_, AccountIdRow>(
            "SELECT id FROM account WHERE username = $1 \
             UNION ALL \
             SELECT id FROM pending_registration WHERE username = $1 AND expires_at > now()",
        )
        .bind(&input.username)
        .fetch_optional(&self.pool)
        .await?;

        if username_taken.is_some() {
            return Err(AuthError::UsernameTaken);
        }

        let email_taken = sqlx::query_as::<_, AccountIdRow>("SELECT id FROM account WHERE email = $1")
            .bind(&email)
            .fetch_optional(&self.pool)
            .await?;

        if email_taken.is_some() {
            return Ok(());
        }

        let code = generate_code();
        let code_digest = digest_code(&code);
        let pending_id = new_id();

        let mut tx = self.pool.begin().await?;

        // A live pending row already claims this address. Replacing it here
        // would let a second caller overwrite a stranger's chosen username
        // and password before that stranger ever proves they own the
        // mailbox — the registration-flow equivalent of account takeover.
        // Leave it untouched and answer exactly like the already-registered
        // branch above: uniform, silent, no oracle.
        let live_registration = sqlx::query_as::<_, AccountIdRow>(
            "SELECT id FROM pending_registration \
             WHERE email = $1 AND expires_at > now() FOR UPDATE",
        )
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;

        if live_registration.is_some() {
            tx.commit().await?;
            return Ok(());
        }

        let recently_mailed = self.mailed_within_cooldown(&mut tx, &email).await?;

        // Only expired rows are ever removed here — a live row for this
        // email already returned above. Scoped to the caller's own address,
        // plus any *expired* row holding the username: deleting a live row
        // by username would let anyone wipe a stranger's in-flight
        // registration just by claiming the name they picked, and an expired
        // row holds no claim worth keeping — the availability check above
        // already ignores it, so leaving it in place would make that check a
        // liar and fail this INSERT against the table's UNIQUE constraint,
        // stranding the name until someone purges by hand.
        sqlx::query(
            "DELETE FROM pending_registration \
             WHERE expires_at <= now() AND (email = $1 OR username = $2)",
        )
        .bind(&email)
        .bind(&input.username)
        .execute(&mut *tx)
        .await?;

        let insert_sql = format!(
            "INSERT INTO pending_registration \
             (id, email, username, display_name, password_hash, code_digest, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now() + interval '{CODE_TTL_MINUTES} minutes')"
        );

        // Safe to assert: built only from a fixed Rust integer constant,
        // never from request input.
        sqlx::query(sqlx::AssertSqlSafe(insert_sql))
            .bind(pending_id)
            .bind(&email)
            .bind(&input.username)
            .bind(&input.display_name)
            .bind(&password_hash)
            .bind(&code_digest)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        // The row (and its fresh code) is committed either way; only the mail
        // is withheld inside the cooldown. Doing it the other way round would
        // leave the caller holding a code that no longer works.
        if !recently_mailed {
            self.send_code(&email, &code).await?;
        }

        Ok(())
    }

    /// Issues a fresh code for a live pending registration, invalidating the
    /// previous one.
    ///
    /// Silent when there is no such registration, for the same reason
    /// `register` is silent about a taken address.
    pub async fn resend_verification_code(&self, email: &str) -> Result<(), AuthError> {
        let email = normalize_email(email);
        let mut tx = self.pool.begin().await?;

        if self.mailed_within_cooldown(&mut tx, &email).await? {
            // Leave the existing code alone. Rotating it here would let an
            // attacker invalidate a victim's in-flight code at will, simply by
            // hammering resend faster than the victim can type.
            tx.commit().await?;
            return Ok(());
        }

        let code = generate_code();
        let code_digest = digest_code(&code);

        // Resetting `attempts` is deliberate. The attempt counter guards one
        // code against guessing; a genuinely new secret has not been guessed
        // at yet, and carrying the count over would let someone lock a
        // stranger's registration out of its own retries by burning attempts
        // against a code they never received.
        let update_sql = format!(
            "UPDATE pending_registration \
             SET code_digest = $1, \
                 attempts = 0, \
                 expires_at = now() + interval '{CODE_TTL_MINUTES} minutes' \
             WHERE email = $2"
        );

        // Safe to assert: fixed Rust constant only.
        let result = sqlx::query(sqlx::AssertSqlSafe(update_sql))
            .bind(&code_digest)
            .bind(&email)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        if result.rows_affected() == 0 {
            return Ok(());
        }

        self.send_code(&email, &code).await?;

        Ok(())
    }

    /// Whether a code went out to this address inside the cooldown window.
    ///
    /// Derived from `expires_at` rather than a separate column: issuing a code
    /// always sets `expires_at = now() + CODE_TTL_MINUTES`, so a row whose
    /// expiry is still further out than `TTL - cooldown` was mailed within the
    /// cooldown. One source of truth, and it survives a restart the way an
    /// in-memory limiter would not.
    async fn mailed_within_cooldown(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        email: &str,
    ) -> Result<bool, AuthError> {
        let sql = format!(
            "SELECT EXISTS ( \
                 SELECT 1 FROM pending_registration \
                 WHERE email = $1 \
                   AND expires_at > now() + interval '{CODE_TTL_MINUTES} minutes' \
                                 - interval '{RESEND_COOLDOWN_SECONDS} seconds' \
             )"
        );

        // Safe to assert: fixed Rust constants only.
        let (recent,): (bool,) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(email)
            .fetch_one(&mut **tx)
            .await?;

        Ok(recent)
    }

    /// Proves an address and creates the account it was registered for.
    ///
    /// Everything happens in one transaction: the row is re-read `FOR UPDATE`,
    /// the code is checked, uniqueness is re-checked against `account`, and
    /// the account, its password identity, and the deletion of the pending row
    /// all commit together. A failure anywhere leaves the registration exactly
    /// as it was.
    pub async fn verify_registration(
        &self,
        input: VerifyRegistrationInput,
    ) -> Result<AccountSummary, AuthError> {
        let mut tx = self.pool.begin().await?;

        // FOR UPDATE so two concurrent verifications of the same registration
        // serialize: without it both could read the same attempt count, or
        // both pass the code check and race to insert the account.
        let email = normalize_email(&input.email);
        let pending = sqlx::query_as::<_, PendingRegistrationRow>(
            "SELECT id, email, username, display_name, password_hash, code_digest, attempts \
             FROM pending_registration \
             WHERE email = $1 AND expires_at > now() \
             FOR UPDATE",
        )
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;

        // No live registration, or it expired. Same error as a wrong code, so
        // a guesser cannot tell "nobody is registering this address" from
        // "wrong code".
        let Some(pending) = pending else {
            return Err(AuthError::InvalidVerificationCode);
        };

        if !code_matches(&input.code, &pending.code_digest) {
            let attempts = pending.attempts + 1;

            if attempts >= MAX_ATTEMPTS {
                // Burn the registration outright rather than just blocking
                // further guesses. Leaving a dead row behind would keep the
                // username reserved for a registration that can never complete.
                sqlx::query("DELETE FROM pending_registration WHERE id = $1")
                    .bind(pending.id)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("UPDATE pending_registration SET attempts = $1 WHERE id = $2")
                    .bind(attempts)
                    .bind(pending.id)
                    .execute(&mut *tx)
                    .await?;
            }

            tx.commit().await?;
            return Err(AuthError::InvalidVerificationCode);
        }

        let account = self
            .promote_pending(&mut tx, &pending, PromotionReuse::Registration)
            .await?;

        sqlx::query("DELETE FROM pending_registration WHERE id = $1")
            .bind(pending.id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(account.into())
    }

    /// Deletes expired pending registrations. Returns how many went.
    ///
    /// They hold argon2 hashes for accounts that will never exist, and each
    /// one keeps a username reserved. Neither is worth keeping.
    pub async fn purge_expired_registrations(&self) -> Result<u64, AuthError> {
        let result = sqlx::query("DELETE FROM pending_registration WHERE expires_at <= now()")
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected())
    }

    /// Creates an account directly, skipping the email round-trip.
    ///
    /// Exists for other crates' integration tests, which need accounts to
    /// exist but have nothing to say about verification. Feature-gated so the
    /// server binary cannot reach it — a bypass of the registration flow is
    /// exactly the kind of thing that should be impossible to call by accident
    /// in production.
    #[cfg(feature = "test-support")]
    pub async fn create_verified_account(
        &self,
        input: RegisterInput,
    ) -> Result<AccountSummary, AuthError> {
        validate_email(&input.email)?;
        validate_username(&input.username)?;
        validate_password(&input.password)?;
        validate_display_name(&input.display_name)?;

        let password_hash = hash_password(&input.password)?;

        let pending = PendingRegistrationRow {
            id: new_id(),
            email: normalize_email(&input.email),
            username: input.username,
            display_name: input.display_name,
            password_hash,
            code_digest: String::new(),
            attempts: 0,
        };

        let mut tx = self.pool.begin().await?;
        let account = self
            .promote_pending(&mut tx, &pending, PromotionReuse::Direct)
            .await?;
        tx.commit().await?;

        Ok(account.into())
    }

    /// Inserts the `account` and its password identity from a pending row.
    /// Shared by verification and the test-support direct path so the two can
    /// never drift into producing differently-shaped accounts.
    async fn promote_pending(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        pending: &PendingRegistrationRow,
        reuse: PromotionReuse,
    ) -> Result<AccountRow, AuthError> {
        let account_id = new_id();
        let identity_id = new_id();

        // `email_verified_at` is set here and nowhere else: an
        // account cannot exist without a proven address, so NULL in that
        // column means "predates this migration" and nothing else.
        let account = sqlx::query_as::<_, AccountRow>(concat!(
            "INSERT INTO account (id, username, email, display_name, email_verified_at) \
             VALUES ($1, $2, $3, $4, now()) RETURNING ",
            account_columns!()
        ))
        .bind(account_id)
        .bind(&pending.username)
        .bind(&pending.email)
        .bind(&pending.display_name)
        .fetch_one(&mut **tx)
        .await
        .map_err(|err| map_promotion_conflict(err, reuse))?;

        sqlx::query(
            "INSERT INTO auth_identity (id, account_id, provider, provider_account_id, password_hash) \
             VALUES ($1, $2, 'password', NULL, $3)",
        )
        .bind(identity_id)
        .bind(account.id)
        .bind(&pending.password_hash)
        .execute(&mut **tx)
        .await?;

        Ok(account)
    }

    async fn send_code(&self, email: &str, code: &str) -> Result<(), AuthError> {
        let mail = OutgoingMail {
            to: email.to_string(),
            subject: "Tu código de verificación".to_string(),
            body: format!(
                "Tu código de verificación es {code}\n\n\
                 Vence en {CODE_TTL_MINUTES} minutos y sirve una sola vez.\n\
                 Si no fuiste vos, ignorá este mensaje: sin el código no se crea ninguna cuenta.\n"
            ),
        };

        self.mailer
            .send(mail)
            .await
            .map_err(AuthError::MailDelivery)
    }

    pub async fn get_account(&self, account_id: Uuid) -> Result<AccountSummary, AuthError> {
        let account = sqlx::query_as::<_, AccountRow>(concat!(
            "SELECT ",
            account_columns!(),
            " FROM account WHERE id = $1"
        ))
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AuthError::AccountNotFound)?;

        Ok(account.into())
    }

    pub async fn update_account(
        &self,
        account_id: Uuid,
        input: UpdateAccountInput,
    ) -> Result<AccountSummary, AuthError> {
        if let Some(username) = &input.username {
            validate_username(username)?;
        }
        if let Some(display_name) = &input.display_name {
            validate_display_name(display_name)?;
        }
        // For the nullable fields, only a supplied *value* is validated.
        // `Some(None)` is a request to clear, and there is nothing to check
        // about the absence of a bio.
        if let Some(Some(avatar_url)) = &input.avatar_url {
            validate_avatar_url(avatar_url)?;
        }
        if let Some(Some(bio)) = &input.bio {
            validate_bio(bio)?;
        }
        if let Some(Some(banner_url)) = &input.banner_url {
            validate_banner_url(banner_url)?;
        }
        if let Some(Some(accent_color)) = &input.accent_color {
            validate_accent_color(accent_color)?;
        }
        if let Some(Some(pronouns)) = &input.pronouns {
            validate_pronouns(pronouns)?;
        }

        // Nothing to change: skip the UPDATE entirely (and its `updated_at`
        // bump) and just return the current row.
        if input.username.is_none()
            && input.display_name.is_none()
            && input.avatar_url.is_none()
            && input.bio.is_none()
            && input.banner_url.is_none()
            && input.accent_color.is_none()
            && input.pronouns.is_none()
        {
            return self.get_account(account_id).await;
        }

        // Still a single fixed statement, so sea-query stays
        // reserved for genuinely dynamic queries. The nullable columns use
        // `CASE WHEN <present> THEN <value> ELSE column END` rather than
        // COALESCE, because COALESCE cannot express "set this to NULL":
        // `COALESCE(NULL, column)` is the column, so a clear would silently
        // become a no-op and a user could never remove their own bio.
        // `username`/`display_name` are NOT NULL, so COALESCE still says
        // everything there is to say about them.
        let account = sqlx::query_as::<_, AccountRow>(concat!(
            "UPDATE account \
             SET username = COALESCE($1, username), \
                 display_name = COALESCE($2, display_name), \
                 avatar_url = CASE WHEN $3 THEN $4 ELSE avatar_url END, \
                 bio = CASE WHEN $5 THEN $6 ELSE bio END, \
                 banner_url = CASE WHEN $7 THEN $8 ELSE banner_url END, \
                 accent_color = CASE WHEN $9 THEN $10 ELSE accent_color END, \
                 pronouns = CASE WHEN $11 THEN $12 ELSE pronouns END, \
                 updated_at = now() \
             WHERE id = $13 RETURNING ",
            account_columns!()
        ))
        .bind(&input.username)
        .bind(&input.display_name)
        .bind(input.avatar_url.is_some())
        .bind(input.avatar_url.clone().flatten())
        .bind(input.bio.is_some())
        .bind(input.bio.clone().flatten())
        .bind(input.banner_url.is_some())
        .bind(input.banner_url.clone().flatten())
        .bind(input.accent_color.is_some())
        .bind(input.accent_color.clone().flatten())
        .bind(input.pronouns.is_some())
        .bind(input.pronouns.clone().flatten())
        .bind(account_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_account_conflict)?
        // `account_id` only ever comes from a verified `AuthContext`, never
        // client input, so zero rows here "shouldn't" happen — but return
        // `AccountNotFound` rather than panicking if it somehow does.
        .ok_or(AuthError::AccountNotFound)?;

        Ok(account.into())
    }

    pub async fn login(&self, input: LoginInput) -> Result<(SessionSummary, String), AuthError> {
        // Never distinguish "no such account" from "wrong password" — not
        // just in the returned error, but in timing too. If we skipped the
        // argon2 verify whenever the account or password identity didn't
        // exist, a nonexistent-email request would return measurably
        // faster than a wrong-password one, letting an attacker enumerate
        // registered emails by response latency alone. So a hash verify
        // — real or, on the missing-account/missing-password paths, a
        // fixed dummy one — always runs before we decide the outcome.
        let email = normalize_email(&input.email);
        let account = sqlx::query_as::<_, AccountIdRow>("SELECT id FROM account WHERE email = $1")
            .bind(&email)
            .fetch_optional(&self.pool)
            .await?;

        let stored_hash = match &account {
            Some(account) => {
                sqlx::query_as::<_, PasswordHashRow>(
                    "SELECT password_hash FROM auth_identity WHERE account_id = $1 AND provider = 'password'",
                )
                .bind(account.id)
                .fetch_optional(&self.pool)
                .await?
                .and_then(|identity| identity.password_hash)
            }
            None => None,
        };
        let stored_hash = stored_hash.unwrap_or_else(|| dummy_password_hash().to_string());

        let password_matches = verify_password(&input.password, &stored_hash);

        let Some(account) = account.filter(|_| password_matches) else {
            return Err(AuthError::InvalidCredentials);
        };

        let mut tx = self.pool.begin().await?;

        // Lock this account's row for the rest of the transaction so two
        // concurrent logins for the same account serialize here instead of
        // both reading the same active-session count and both inserting —
        // without this the count-then-insert below is a TOCTOU race that
        // can push an account above MAX_CONCURRENT_SESSIONS.
        sqlx::query("SELECT id FROM account WHERE id = $1 FOR UPDATE")
            .bind(account.id)
            .fetch_one(&mut *tx)
            .await?;

        let count_sql = format!(
            "SELECT COUNT(*) FROM session WHERE account_id = $1 AND {}",
            session_eligibility_sql()
        );

        // Safe to assert: the interpolated fragment is built only from fixed
        // Rust integer constants, never from request input.
        let (active_sessions,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(count_sql))
            .bind(account.id)
            .fetch_one(&mut *tx)
            .await?;

        if active_sessions >= MAX_CONCURRENT_SESSIONS {
            // No silent eviction — the caller must revoke an existing
            // session first. Dropping `tx`
            // here rolls back — we haven't written anything yet.
            return Err(AuthError::SessionLimitReached);
        }

        let raw_token = generate_session_token();
        let token_hash = hash_token(&raw_token);
        let session_id = new_id();

        let insert_sql = format!(
            "INSERT INTO session (id, account_id, token_hash, absolute_expires_at) \
             VALUES ($1, $2, $3, now() + interval '{ABSOLUTE_LIFETIME_DAYS} days') \
             RETURNING id, created_at, last_used_at, absolute_expires_at"
        );

        // Safe to assert: `insert_sql` is built only from a fixed Rust
        // integer constant (ABSOLUTE_LIFETIME_DAYS), never from request
        // input — sqlx's SqlSafeStr guard exists for exactly the opposite
        // case (interpolating untrusted data into SQL).
        let session_row = sqlx::query_as::<_, SessionRow>(sqlx::AssertSqlSafe(insert_sql))
            .bind(session_id)
            .bind(account.id)
            .bind(&token_hash)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok((session_row.into(), raw_token))
    }

    pub async fn verify_session(&self, raw_token: &str) -> Result<AuthContext, AuthError> {
        let token_hash = hash_token(raw_token);

        let select_sql = format!(
            "SELECT id, account_id FROM session WHERE token_hash = $1 AND {}",
            session_eligibility_sql()
        );

        // Safe to assert: the interpolated fragment is built only from fixed
        // Rust integer constants, never from request input.
        let session = sqlx::query_as::<_, SessionIdentityRow>(sqlx::AssertSqlSafe(select_sql))
            .bind(&token_hash)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AuthError::Unauthenticated)?;

        // Touch last_used_at so the idle window slides forward from "now"
        // on every successful check, instead of expiring on a fixed
        // schedule anchored to session creation.
        sqlx::query("UPDATE session SET last_used_at = now() WHERE id = $1")
            .bind(session.id)
            .execute(&self.pool)
            .await?;

        Ok(AuthContext {
            account_id: session.account_id,
            session_id: session.id,
        })
    }

    pub async fn list_sessions(&self, account_id: Uuid) -> Result<Vec<SessionSummary>, AuthError> {
        let list_sql = format!(
            "SELECT id, created_at, last_used_at, absolute_expires_at FROM session \
             WHERE account_id = $1 AND {} \
             ORDER BY created_at DESC",
            session_eligibility_sql()
        );

        // Safe to assert: the interpolated fragment is built only from fixed
        // Rust integer constants, never from request input.
        let rows = sqlx::query_as::<_, SessionRow>(sqlx::AssertSqlSafe(list_sql))
            .bind(account_id)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn revoke_session(&self, account_id: Uuid, session_id: Uuid) -> Result<(), AuthError> {
        // Scoped to (id AND account_id) in one statement so a revoke can
        // never touch another account's row, and a session that belongs to
        // someone else looks identical to one that doesn't exist.
        let result = sqlx::query(
            "UPDATE session SET revoked_at = now() \
             WHERE id = $1 AND account_id = $2 AND revoked_at IS NULL",
        )
        .bind(session_id)
        .bind(account_id)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            return Err(AuthError::SessionNotFound);
        }

        Ok(())
    }

    pub async fn revoke_all_sessions(&self, account_id: Uuid) -> Result<(), AuthError> {
        sqlx::query("UPDATE session SET revoked_at = now() WHERE account_id = $1 AND revoked_at IS NULL")
            .bind(account_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}

/// `account.username` and `account.email` are both `UNIQUE`, so a violation
/// during `register` or `update_account` could be either — inspect the
/// actual constraint name Postgres reports rather than guessing which
/// column collided. (`update_account` only ever writes `username`, so in
/// practice only the username-key branch fires there, but reusing this
/// keeps the mapping in exactly one place.)
/// Which path is promoting a pending row, so a unique violation can be
/// reported in the caller's terms.
#[derive(Debug, Clone, Copy)]
enum PromotionReuse {
    /// Reached through `verify_registration`: the caller already proved their
    /// address, so a username clash is a lost race, not a bad choice.
    Registration,
    /// Reached through the test-support direct path, where a clash means the
    /// caller simply picked a taken name.
    #[cfg_attr(not(feature = "test-support"), allow(dead_code))]
    Direct,
}

/// Maps a unique violation raised while promoting a pending registration.
///
/// The username branch is the interesting one. Under `Registration` the caller
/// verified correctly and lost a race to a name that was free when they
/// started, which is a different situation from choosing a taken name — and
/// telling them apart is the whole reason `UsernameTakenDuringVerification`
/// exists.
fn map_promotion_conflict(err: sqlx::Error, reuse: PromotionReuse) -> AuthError {
    if let sqlx::Error::Database(db_err) = &err {
        if db_err.is_unique_violation() {
            match (db_err.constraint(), reuse) {
                (Some("account_username_key"), PromotionReuse::Registration) => {
                    return AuthError::UsernameTakenDuringVerification
                }
                (Some("account_username_key"), PromotionReuse::Direct) => {
                    return AuthError::UsernameTaken
                }
                (Some("account_email_key"), _) => return AuthError::EmailTaken,
                _ => {}
            }
        }
    }
    AuthError::Database(err)
}

fn map_account_conflict(err: sqlx::Error) -> AuthError {
    if let sqlx::Error::Database(db_err) = &err {
        if db_err.is_unique_violation() {
            match db_err.constraint() {
                Some("account_username_key") => return AuthError::UsernameTaken,
                Some("account_email_key") => return AuthError::EmailTaken,
                _ => {}
            }
        }
    }
    AuthError::Database(err)
}
