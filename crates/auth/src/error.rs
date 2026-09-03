use thiserror::Error;

/// Domain errors for the `auth` crate. No HTTP knowledge lives here — the
/// `api` crate maps each variant to a status code and error envelope.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("email is already registered")]
    EmailTaken,

    #[error("username is already taken")]
    UsernameTaken,

    /// Deliberately generic: login never reveals whether the email or the
    /// password was wrong.
    #[error("invalid credentials")]
    InvalidCredentials,

    /// No silent eviction — the caller must revoke an existing session
    /// before a new login can succeed.
    #[error("maximum number of concurrent sessions reached")]
    SessionLimitReached,

    /// Also returned when a session exists but belongs to a different
    /// account, so a revoke/list caller can never learn whether an id
    /// exists under someone else.
    #[error("session not found")]
    SessionNotFound,

    /// The account id doesn't exist. In practice this only happens for
    /// `get_account`'s path-param lookup of *another* account — the caller's
    /// own account id always comes from a verified `AuthContext`, never
    /// client input, so it "shouldn't" be missing there.
    #[error("account not found")]
    AccountNotFound,

    /// A session token is missing, unknown, revoked, or expired (absolute
    /// or idle). Kept distinct from `InvalidCredentials`, which is only
    /// for the login endpoint.
    #[error("unauthenticated")]
    Unauthenticated,

    /// No live pending registration for that address, or the code was wrong,
    /// expired, or already burned through its attempts. Deliberately one
    /// variant for all of those: distinguishing them would tell an attacker
    /// which addresses are mid-registration and whether a guess was close.
    #[error("invalid or expired verification code")]
    InvalidVerificationCode,

    /// The username was free when the registration started and taken by the
    /// time it was verified. Distinct from `UsernameTaken` so the client can
    /// say what actually happened — the caller proved their address correctly
    /// and still needs to pick another name, which is not the same situation
    /// as choosing a taken name up front.
    #[error("username was taken while the registration was pending")]
    UsernameTakenDuringVerification,

    #[error("validation failed: {0}")]
    Validation(String),

    /// The verification mail could not be handed to the relay. Surfaced
    /// rather than swallowed: a registration whose code never left the
    /// building is a dead end the user cannot diagnose or escape.
    #[error("could not send the verification email")]
    MailDelivery(#[source] mailer::MailError),

    #[error("database error")]
    Database(#[source] sqlx::Error),
}

impl From<sqlx::Error> for AuthError {
    fn from(err: sqlx::Error) -> Self {
        AuthError::Database(err)
    }
}
