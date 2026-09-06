use chrono::{DateTime, Utc};

use app_core::Uuid;

/// Input to `AuthService::register`. Raw, unvalidated user input — validation
/// happens inside `register` itself so the rule is unit-testable and lives
/// in exactly one place.
#[derive(Debug, Clone)]
pub struct RegisterInput {
    pub email: String,
    pub username: String,
    pub password: String,
    pub display_name: String,
}

/// Input to `AuthService::login`.
#[derive(Debug, Clone)]
pub struct LoginInput {
    pub email: String,
    pub password: String,
}

/// Input to `AuthService::verify_registration` — the address being proven and
/// the code that proves it.
#[derive(Debug, Clone)]
pub struct VerifyRegistrationInput {
    pub email: String,
    pub code: String,
}

/// Public-safe view of an `account` row. Never carries a password hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub bio: Option<String>,
    pub banner_url: Option<String>,
    /// `#RRGGBB`, validated on write (migration 0004 constrains it too).
    pub accent_color: Option<String>,
    /// `she/her` shape: one slash, 1-5 letters a side.
    pub pronouns: Option<String>,
    /// Doubles as the global "member since", surfaced on `AccountResponse`.
    pub created_at: DateTime<Utc>,
    /// When the address was proven. `None` only for accounts predating
    /// required email verification — after that, an account cannot exist without a proven address,
    /// so `None` means "grandfathered", not "suspicious". Those accounts are
    /// prompted from settings, never blocked.
    pub email_verified_at: Option<DateTime<Utc>>,
}

/// Input to `AuthService::update_account`.
///
/// `username` and `display_name` are `NOT NULL` columns, so `Option` says all
/// there is to say: `None` leaves them alone.
///
/// The nullable profile fields need one more state. `Option<Option<String>>`
/// distinguishes three cases the API actually has:
///
/// - `None` — the field was absent from the request; leave the column alone.
/// - `Some(None)` — the field was sent as `null`; **clear** the column.
/// - `Some(Some(v))` — set it to `v`.
///
/// A plain `Option` cannot express "clear it", and the `COALESCE` update this
/// service used to run could not either: `COALESCE(NULL, column)` is the
/// column, so binding NULL means "no change". Without this, a user could set
/// a bio and never remove it.
#[derive(Debug, Clone, Default)]
pub struct UpdateAccountInput {
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<Option<String>>,
    pub bio: Option<Option<String>>,
    pub banner_url: Option<Option<String>>,
    pub accent_color: Option<Option<String>>,
    pub pronouns: Option<Option<String>>,
}

/// Public-safe view of a `session` row. Never carries `token_hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}
