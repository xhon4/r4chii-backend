//! Business-rule validation for registration input. Lives here (not the
//! `api` crate) because it is a business rule, not request parsing — the
//! api crate's job is limited to JSON structural parsing and mapping
//! `AuthError::Validation` to a 400 response.

use chrono::{DateTime, TimeDelta, Utc};

use crate::error::AuthError;

/// A password long enough to survive a naive length-based DoS against the
/// (deliberately slow) argon2 hasher, well above what any real password
/// needs.
const MAX_PASSWORD_LEN: usize = 256;

/// Basic shape check only — no regex crate, no attempt at full RFC 5322.
/// Real deliverability is proven by actually sending mail, which is what the
/// verification code does: this only rejects input that could
/// not possibly be an address, and the code round-trip settles the rest.
pub(crate) fn validate_email(email: &str) -> Result<(), AuthError> {
    let mut parts = email.splitn(2, '@');
    let (Some(local), Some(domain)) = (parts.next(), parts.next()) else {
        return Err(AuthError::Validation(
            "email must contain exactly one '@'".to_string(),
        ));
    };

    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return Err(AuthError::Validation(
            "email must have a non-empty local part and a domain with a dot".to_string(),
        ));
    }

    Ok(())
}

/// Canonicalizes an address for every lookup, comparison, and stored value.
///
/// Email is effectively case-insensitive in practice: `Bob@x.com` and
/// `bob@x.com` are the same mailbox to every real provider. Without this,
/// the same address can claim two accounts, dodge the "already registered"
/// check, and dodge the resend cooldown, all by changing case. Trimming
/// surrounding whitespace too, since a pasted address carrying it is not a
/// different address.
pub(crate) fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

pub(crate) fn validate_username(username: &str) -> Result<(), AuthError> {
    let len = username.chars().count();
    if !(3..=32).contains(&len) {
        return Err(AuthError::Validation(
            "username must be between 3 and 32 characters".to_string(),
        ));
    }

    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(AuthError::Validation(
            "username may only contain letters, digits, and underscores".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_password(password: &str) -> Result<(), AuthError> {
    if password.len() < 8 {
        return Err(AuthError::Validation(
            "password must be at least 8 characters".to_string(),
        ));
    }

    if password.len() > MAX_PASSWORD_LEN {
        return Err(AuthError::Validation(format!(
            "password must be at most {MAX_PASSWORD_LEN} characters"
        )));
    }

    Ok(())
}

/// A name meant to render inline next to an avatar in a member list: long
/// enough for a real name, short enough that layout and storage stay
/// predictable regardless of what a caller sends.
const MAX_DISPLAY_NAME_LEN: usize = 32;

pub(crate) fn validate_display_name(display_name: &str) -> Result<(), AuthError> {
    if display_name.trim().is_empty() {
        return Err(AuthError::Validation(
            "display name must not be empty".to_string(),
        ));
    }

    if display_name.chars().count() > MAX_DISPLAY_NAME_LEN {
        return Err(AuthError::Validation(format!(
            "display name must be at most {MAX_DISPLAY_NAME_LEN} characters"
        )));
    }

    Ok(())
}

/// M0 stores `avatar_url` as a plain string with no real upload flow yet —
/// this is a sanity bound, not URL
/// validation. Only called when the caller actually supplied a value; `None`
/// (no change requested) skips validation entirely.
const MAX_AVATAR_URL_LEN: usize = 2048;

pub(crate) fn validate_avatar_url(avatar_url: &str) -> Result<(), AuthError> {
    if avatar_url.trim().is_empty() {
        return Err(AuthError::Validation(
            "avatar url must not be empty".to_string(),
        ));
    }

    if avatar_url.chars().count() > MAX_AVATAR_URL_LEN {
        return Err(AuthError::Validation(format!(
            "avatar url must be at most {MAX_AVATAR_URL_LEN} characters"
        )));
    }

    let parsed = url::Url::parse(avatar_url)
        .map_err(|_| AuthError::Validation("avatar url must be a valid url".to_string()))?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(AuthError::Validation(
            "avatar url must use http or https".to_string(),
        ));
    }

    Ok(())
}

/// Matches migration 0004's `account_bio_length`. Characters, not bytes, so
/// an accented bio is not silently shorter than an ASCII one.
const MAX_BIO_LEN: usize = 190;

/// Letters either side of the slash. Deliberately not `is_ascii_alphabetic`:
/// "él/ella" is the ordinary case for this app's users, and an ASCII-only
/// class would reject it.
const MAX_PRONOUN_SIDE_LEN: usize = 5;

pub(crate) fn validate_bio(bio: &str) -> Result<(), AuthError> {
    if bio.chars().count() > MAX_BIO_LEN {
        return Err(AuthError::Validation(format!(
            "bio must be at most {MAX_BIO_LEN} characters"
        )));
    }

    Ok(())
}

pub(crate) fn validate_banner_url(banner_url: &str) -> Result<(), AuthError> {
    // Same shape and rules as an avatar url; only the field name in the
    // error needs to change to actually name what was rejected.
    validate_avatar_url(banner_url).map_err(|err| match err {
        AuthError::Validation(message) => {
            AuthError::Validation(message.replace("avatar", "banner"))
        }
        other => other,
    })
}

/// Exactly `#RRGGBB`. Three-digit shorthand and named colours are rejected on
/// purpose: one stored shape means the client never has to normalise before
/// rendering, and the database CHECK can be exact.
pub(crate) fn validate_accent_color(accent_color: &str) -> Result<(), AuthError> {
    let invalid = || {
        AuthError::Validation("accent color must be a hex value like #ffa800".to_string())
    };

    let Some(digits) = accent_color.strip_prefix('#') else {
        return Err(invalid());
    };

    if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid());
    }

    Ok(())
}

/// Exactly one slash, 1-5 letters on each side: "she/her", "él/ella".
pub(crate) fn validate_pronouns(pronouns: &str) -> Result<(), AuthError> {
    let invalid = || {
        AuthError::Validation(format!(
            "pronouns must look like she/her, with at most {MAX_PRONOUN_SIDE_LEN} letters per side"
        ))
    };

    let mut parts = pronouns.split('/');
    let (Some(left), Some(right), None) = (parts.next(), parts.next(), parts.next()) else {
        // Three parts means a second slash ("she/her/hers"); fewer means none.
        return Err(invalid());
    };

    for side in [left, right] {
        let len = side.chars().count();
        if !(1..=MAX_PRONOUN_SIDE_LEN).contains(&len) || !side.chars().all(char::is_alphabetic) {
            return Err(invalid());
        }
    }

    Ok(())
}

/// The `presence_status` enum's members. An unlisted value is rejected here
/// rather than at the cast, which would surface as a server error.
const PRESENCE_STATUSES: [&str; 4] = ["online", "idle", "dnd", "invisible"];

/// The `visibility` enum's members, rejected here for the same reason.
const VISIBILITIES: [&str; 3] = ["public", "friends", "private"];

/// Matches the `account_custom_status_len` constraint. Characters, not bytes.
const MAX_CUSTOM_STATUS_LEN: usize = 128;

/// No database constraint bounds the emoji column. This is a defensive cap,
/// wide enough for a composed sequence joined by zero-width joiners.
const MAX_CUSTOM_EMOJI_LEN: usize = 16;

/// How far ahead a manual status may be scheduled to clear itself.
const MAX_CUSTOM_STATUS_TTL_HOURS: i64 = 24;

const MAX_PROFILE_LINKS: usize = 5;

/// Matches `account_profile_link`'s `label_len` and `url_len` constraints.
const MAX_LINK_LABEL_LEN: usize = 32;
const MAX_LINK_URL_LEN: usize = 256;

pub(crate) fn validate_status(status: &str) -> Result<(), AuthError> {
    if !PRESENCE_STATUSES.contains(&status) {
        return Err(AuthError::Validation(format!(
            "status must be one of: {}",
            PRESENCE_STATUSES.join(", ")
        )));
    }

    Ok(())
}

/// `axis` names the field in the error so a caller sees which of the three
/// visibility settings was rejected.
pub(crate) fn validate_visibility(axis: &str, value: &str) -> Result<(), AuthError> {
    if !VISIBILITIES.contains(&value) {
        return Err(AuthError::Validation(format!(
            "{axis} visibility must be one of: {}",
            VISIBILITIES.join(", ")
        )));
    }

    Ok(())
}

/// Trims the text and rejects control characters, which includes newlines.
/// `Ok(None)` means the text was empty or whitespace only, which the caller
/// treats as a request to clear the status.
pub(crate) fn sanitize_custom_status_text(text: &str) -> Result<Option<String>, AuthError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if trimmed.chars().count() > MAX_CUSTOM_STATUS_LEN {
        return Err(AuthError::Validation(format!(
            "custom status must be at most {MAX_CUSTOM_STATUS_LEN} characters"
        )));
    }

    if trimmed.chars().any(char::is_control) {
        return Err(AuthError::Validation(
            "custom status must not contain line breaks or control characters".to_string(),
        ));
    }

    Ok(Some(trimmed.to_string()))
}

/// Trims the emoji and applies the defensive length cap. `Ok(None)` means
/// nothing was supplied to store.
pub(crate) fn sanitize_custom_status_emoji(emoji: &str) -> Result<Option<String>, AuthError> {
    let trimmed = emoji.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if trimmed.chars().count() > MAX_CUSTOM_EMOJI_LEN {
        return Err(AuthError::Validation(format!(
            "custom status emoji must be at most {MAX_CUSTOM_EMOJI_LEN} characters, \
             a defensive bound rather than a contract limit"
        )));
    }

    if trimmed.chars().any(char::is_control) {
        return Err(AuthError::Validation(
            "custom status emoji must not contain control characters".to_string(),
        ));
    }

    Ok(Some(trimmed.to_string()))
}

pub(crate) fn validate_custom_status_expiry(
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), AuthError> {
    if expires_at <= now {
        return Err(AuthError::Validation(
            "custom status expiry must be in the future".to_string(),
        ));
    }

    if expires_at > now + TimeDelta::hours(MAX_CUSTOM_STATUS_TTL_HOURS) {
        return Err(AuthError::Validation(format!(
            "custom status expiry must be at most {MAX_CUSTOM_STATUS_TTL_HOURS} hours from now"
        )));
    }

    Ok(())
}

pub(crate) fn validate_profile_link_count(count: usize) -> Result<(), AuthError> {
    if count > MAX_PROFILE_LINKS {
        return Err(AuthError::Validation(format!(
            "at most {MAX_PROFILE_LINKS} profile links are allowed"
        )));
    }

    Ok(())
}

pub(crate) fn sanitize_profile_link_label(label: &str) -> Result<String, AuthError> {
    let trimmed = label.trim();
    let len = trimmed.chars().count();
    if !(1..=MAX_LINK_LABEL_LEN).contains(&len) {
        return Err(AuthError::Validation(format!(
            "profile link label must be between 1 and {MAX_LINK_LABEL_LEN} characters"
        )));
    }

    if trimmed.chars().any(char::is_control) {
        return Err(AuthError::Validation(
            "profile link label must not contain line breaks or control characters".to_string(),
        ));
    }

    Ok(trimmed.to_string())
}

/// Applies the column's own length bound, then defers to the url rules the
/// avatar and banner fields already use so all three accept the same schemes.
pub(crate) fn sanitize_profile_link_url(url: &str) -> Result<String, AuthError> {
    let trimmed = url.trim();
    if trimmed.chars().count() > MAX_LINK_URL_LEN {
        return Err(AuthError::Validation(format!(
            "profile link url must be at most {MAX_LINK_URL_LEN} characters"
        )));
    }

    validate_avatar_url(trimmed).map_err(|err| match err {
        AuthError::Validation(message) => {
            AuthError::Validation(message.replace("avatar url", "profile link url"))
        }
        other => other,
    })?;

    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_email_accepts_a_plausible_address() {
        assert!(validate_email("person@example.com").is_ok());
    }

    #[test]
    fn validate_email_rejects_missing_at_sign() {
        assert!(validate_email("person.example.com").is_err());
    }

    #[test]
    fn validate_email_rejects_empty_local_or_domain_part() {
        assert!(validate_email("@example.com").is_err());
        assert!(validate_email("person@").is_err());
    }

    #[test]
    fn validate_email_rejects_domain_without_a_dot() {
        assert!(validate_email("person@localhost").is_err());
    }

    #[test]
    fn validate_username_accepts_alnum_and_underscore_in_range() {
        assert!(validate_username("r4chii_user").is_ok());
    }

    #[test]
    fn validate_username_rejects_too_short() {
        assert!(validate_username("ab").is_err());
    }

    #[test]
    fn validate_username_rejects_too_long() {
        let too_long = "a".repeat(33);
        assert!(validate_username(&too_long).is_err());
    }

    #[test]
    fn validate_username_rejects_disallowed_characters() {
        assert!(validate_username("has space").is_err());
        assert!(validate_username("has-dash").is_err());
        assert!(validate_username("has@sign").is_err());
    }

    #[test]
    fn validate_password_accepts_eight_or_more_characters() {
        assert!(validate_password("12345678").is_ok());
    }

    #[test]
    fn validate_password_rejects_fewer_than_eight_characters() {
        assert!(validate_password("1234567").is_err());
    }

    #[test]
    fn validate_password_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_PASSWORD_LEN + 1);
        assert!(validate_password(&too_long).is_err());
    }

    #[test]
    fn validate_display_name_accepts_non_empty_text() {
        assert!(validate_display_name("Ignacio").is_ok());
    }

    #[test]
    fn validate_display_name_rejects_empty_or_whitespace_only() {
        assert!(validate_display_name("").is_err());
        assert!(validate_display_name("   ").is_err());
    }

    #[test]
    fn validate_display_name_accepts_up_to_the_max_length() {
        assert!(validate_display_name(&"a".repeat(32)).is_ok());
    }

    #[test]
    fn validate_display_name_rejects_over_the_max_length() {
        assert!(validate_display_name(&"a".repeat(33)).is_err());
    }

    #[test]
    fn validate_avatar_url_accepts_a_plausible_url() {
        assert!(validate_avatar_url("https://example.com/avatar.png").is_ok());
    }

    #[test]
    fn validate_avatar_url_rejects_empty_or_whitespace_only() {
        assert!(validate_avatar_url("").is_err());
        assert!(validate_avatar_url("   ").is_err());
    }

    #[test]
    fn validate_avatar_url_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_AVATAR_URL_LEN + 1);
        assert!(validate_avatar_url(&too_long).is_err());
    }

    #[test]
    fn validate_avatar_url_accepts_exactly_the_max_length() {
        let prefix = "https://example.com/";
        let max_len = format!("{prefix}{}", "a".repeat(MAX_AVATAR_URL_LEN - prefix.len()));
        assert_eq!(max_len.chars().count(), MAX_AVATAR_URL_LEN);
        assert!(validate_avatar_url(&max_len).is_ok());
    }

    #[test]
    fn validate_avatar_url_rejects_a_string_that_is_not_a_url() {
        assert!(validate_avatar_url("not a url at all").is_err());
    }

    #[test]
    fn validate_avatar_url_rejects_a_non_http_scheme() {
        assert!(validate_avatar_url("javascript:alert(1)").is_err());
        assert!(validate_avatar_url("ftp://example.com/avatar.png").is_err());
    }

    #[test]
    fn validate_banner_url_rejects_a_non_http_scheme_with_its_own_field_name() {
        let error = validate_banner_url("javascript:alert(1)").expect_err("must be rejected");
        let AuthError::Validation(message) = error else {
            panic!("expected a validation error");
        };
        assert!(
            message.contains("banner"),
            "the error should name the field it is about, got: {message}"
        );
    }

    #[test]
    fn validate_bio_accepts_up_to_the_limit_and_rejects_past_it() {
        assert!(validate_bio("").is_ok());
        assert!(validate_bio(&"a".repeat(MAX_BIO_LEN)).is_ok());
        assert!(validate_bio(&"a".repeat(MAX_BIO_LEN + 1)).is_err());
    }

    /// The limit counts characters. Measured in bytes, 190 accented
    /// characters would be rejected while 190 ASCII ones passed.
    #[test]
    fn validate_bio_counts_characters_not_bytes() {
        assert!(validate_bio(&"á".repeat(MAX_BIO_LEN)).is_ok());
    }

    #[test]
    fn validate_accent_color_accepts_six_hex_digits_in_either_case() {
        assert!(validate_accent_color("#ffa800").is_ok());
        assert!(validate_accent_color("#FFA800").is_ok());
    }

    #[test]
    fn validate_accent_color_rejects_anything_but_that_exact_shape() {
        assert!(validate_accent_color("ffa800").is_err(), "missing #");
        assert!(validate_accent_color("#fa0").is_err(), "shorthand");
        assert!(validate_accent_color("#ffa8000").is_err(), "too long");
        assert!(validate_accent_color("#gggggg").is_err(), "not hex");
        assert!(validate_accent_color("orange").is_err(), "named colour");
        assert!(validate_accent_color("").is_err());
    }

    #[test]
    fn validate_pronouns_accepts_the_expected_shape() {
        assert!(validate_pronouns("she/her").is_ok());
        assert!(validate_pronouns("they/them").is_ok());
        assert!(validate_pronouns("he/him").is_ok());
        assert!(validate_pronouns("a/b").is_ok(), "one letter per side is fine");
    }

    /// What the five-letter, single-slash rule costs, written down so the
    /// trade-off is visible rather than discovered by a user who cannot enter
    /// their own pronouns. These are real forms people use; the constraint is
    /// a deliberate product decision, not an oversight.
    #[test]
    fn validate_pronouns_rejects_some_legitimate_real_world_forms() {
        assert!(validate_pronouns("he/him/his").is_err(), "three-part form");
        assert!(validate_pronouns("their/theirs").is_err(), "6 letters right");
        assert!(validate_pronouns("nosotres/nosotres").is_err(), "8 letters");
    }

    /// The case this app actually has to serve. An ASCII-only letter class
    /// would reject both of these.
    #[test]
    fn validate_pronouns_accepts_accented_and_non_ascii_letters() {
        assert!(validate_pronouns("él/ella").is_ok());
        assert!(validate_pronouns("elle/elle").is_ok());
    }

    #[test]
    fn validate_pronouns_rejects_the_wrong_shape() {
        assert!(validate_pronouns("she").is_err(), "no slash");
        assert!(validate_pronouns("she/her/hers").is_err(), "two slashes");
        assert!(validate_pronouns("/her").is_err(), "empty left side");
        assert!(validate_pronouns("she/").is_err(), "empty right side");
        assert!(validate_pronouns("their/theirs").is_err(), "over 5 a side");
        assert!(validate_pronouns("12/34").is_err(), "digits are not letters");
        assert!(validate_pronouns("s e/h e").is_err(), "spaces are not letters");
        assert!(validate_pronouns("").is_err());
    }

    #[test]
    fn validate_status_accepts_every_enum_member() {
        for status in PRESENCE_STATUSES {
            assert!(validate_status(status).is_ok(), "{status} is a real status");
        }
    }

    #[test]
    fn validate_status_rejects_anything_else() {
        assert!(validate_status("offline").is_err(), "a derived value");
        assert!(validate_status("Online").is_err(), "wrong case");
        assert!(validate_status("away").is_err());
        assert!(validate_status("").is_err());
    }

    #[test]
    fn validate_visibility_accepts_every_enum_member() {
        for value in VISIBILITIES {
            assert!(validate_visibility("bio", value).is_ok());
        }
    }

    #[test]
    fn validate_visibility_rejects_anything_else_and_names_the_axis() {
        let error = validate_visibility("communities", "everyone").expect_err("must be rejected");
        let AuthError::Validation(message) = error else {
            panic!("expected a validation error");
        };
        assert!(
            message.contains("communities"),
            "the error should name the axis it is about, got: {message}"
        );
    }

    #[test]
    fn sanitize_custom_status_text_trims_and_keeps_the_text() {
        let sanitized = sanitize_custom_status_text("  en una reunión  ").expect("valid");
        assert_eq!(sanitized.as_deref(), Some("en una reunión"));
    }

    #[test]
    fn sanitize_custom_status_text_reads_an_empty_value_as_a_clear() {
        assert_eq!(sanitize_custom_status_text("").expect("valid"), None);
        assert_eq!(sanitize_custom_status_text("   ").expect("valid"), None);
    }

    #[test]
    fn sanitize_custom_status_text_rejects_over_the_max_length() {
        assert!(sanitize_custom_status_text(&"a".repeat(MAX_CUSTOM_STATUS_LEN)).is_ok());
        assert!(sanitize_custom_status_text(&"a".repeat(MAX_CUSTOM_STATUS_LEN + 1)).is_err());
    }

    /// The column's constraint counts characters, so the check that guards it
    /// has to as well.
    #[test]
    fn sanitize_custom_status_text_counts_characters_not_bytes() {
        assert!(sanitize_custom_status_text(&"á".repeat(MAX_CUSTOM_STATUS_LEN)).is_ok());
    }

    #[test]
    fn sanitize_custom_status_text_rejects_line_breaks_and_control_characters() {
        assert!(sanitize_custom_status_text("two\nlines").is_err());
        assert!(sanitize_custom_status_text("a\tb").is_err());
        assert!(sanitize_custom_status_text("a\u{0000}b").is_err());
    }

    #[test]
    fn sanitize_custom_status_emoji_accepts_a_composed_sequence() {
        let sanitized = sanitize_custom_status_emoji("👩‍💻").expect("valid");
        assert_eq!(sanitized.as_deref(), Some("👩‍💻"));
    }

    #[test]
    fn sanitize_custom_status_emoji_rejects_over_the_defensive_bound() {
        assert!(sanitize_custom_status_emoji(&"a".repeat(MAX_CUSTOM_EMOJI_LEN)).is_ok());
        assert!(sanitize_custom_status_emoji(&"a".repeat(MAX_CUSTOM_EMOJI_LEN + 1)).is_err());
    }

    #[test]
    fn validate_custom_status_expiry_accepts_a_time_inside_the_window() {
        let now = Utc::now();
        assert!(validate_custom_status_expiry(now + TimeDelta::hours(1), now).is_ok());
        assert!(validate_custom_status_expiry(
            now + TimeDelta::hours(MAX_CUSTOM_STATUS_TTL_HOURS),
            now
        )
        .is_ok());
    }

    #[test]
    fn validate_custom_status_expiry_rejects_the_past_and_the_present() {
        let now = Utc::now();
        assert!(validate_custom_status_expiry(now - TimeDelta::hours(1), now).is_err());
        assert!(validate_custom_status_expiry(now, now).is_err());
    }

    #[test]
    fn validate_custom_status_expiry_rejects_past_the_window() {
        let now = Utc::now();
        assert!(validate_custom_status_expiry(
            now + TimeDelta::hours(MAX_CUSTOM_STATUS_TTL_HOURS) + TimeDelta::seconds(1),
            now
        )
        .is_err());
        assert!(validate_custom_status_expiry(now + TimeDelta::hours(48), now).is_err());
    }

    #[test]
    fn validate_profile_link_count_accepts_up_to_the_limit() {
        assert!(validate_profile_link_count(0).is_ok());
        assert!(validate_profile_link_count(MAX_PROFILE_LINKS).is_ok());
        assert!(validate_profile_link_count(MAX_PROFILE_LINKS + 1).is_err());
    }

    #[test]
    fn sanitize_profile_link_label_trims_and_bounds_the_label() {
        assert_eq!(
            sanitize_profile_link_label("  github  ").expect("valid"),
            "github"
        );
        assert!(sanitize_profile_link_label(&"a".repeat(MAX_LINK_LABEL_LEN)).is_ok());
        assert!(sanitize_profile_link_label(&"a".repeat(MAX_LINK_LABEL_LEN + 1)).is_err());
    }

    #[test]
    fn sanitize_profile_link_label_rejects_an_empty_label() {
        assert!(sanitize_profile_link_label("").is_err());
        assert!(sanitize_profile_link_label("   ").is_err());
    }

    #[test]
    fn sanitize_profile_link_url_accepts_http_and_https() {
        assert!(sanitize_profile_link_url("https://example.com/me").is_ok());
        assert!(sanitize_profile_link_url("http://example.com/me").is_ok());
    }

    #[test]
    fn sanitize_profile_link_url_rejects_other_schemes_with_its_own_field_name() {
        let error = sanitize_profile_link_url("javascript:alert(1)").expect_err("must be rejected");
        let AuthError::Validation(message) = error else {
            panic!("expected a validation error");
        };
        assert!(
            message.contains("profile link url"),
            "the error should name the field it is about, got: {message}"
        );
        assert!(sanitize_profile_link_url("ftp://example.com/me").is_err());
    }

    #[test]
    fn sanitize_profile_link_url_rejects_over_the_column_bound() {
        let prefix = "https://example.com/";
        let at_limit = format!("{prefix}{}", "a".repeat(MAX_LINK_URL_LEN - prefix.len()));
        assert_eq!(at_limit.chars().count(), MAX_LINK_URL_LEN);
        assert!(sanitize_profile_link_url(&at_limit).is_ok());
        assert!(sanitize_profile_link_url(&format!("{at_limit}a")).is_err());
    }
}
