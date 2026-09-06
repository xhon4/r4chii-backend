//! Business-rule validation for registration input. Lives here (not the
//! `api` crate) because it is a business rule, not request parsing — the
//! api crate's job is limited to JSON structural parsing and mapping
//! `AuthError::Validation` to a 400 response.

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
}
