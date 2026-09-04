//! Business-rule validation for server/channel input. Lives here (not the
//! `api` crate) for the same reason as `auth::validation`: this is a
//! business rule, not request parsing — the api crate's job is limited to
//! JSON structural parsing and mapping `DomainError::Validation` to a 400
//! response.

use app_core::Uuid;

use crate::error::DomainError;

const MAX_SERVER_NAME_LEN: usize = 100;
const MAX_CHANNEL_NAME_LEN: usize = 100;
const MAX_ROLE_NAME_LEN: usize = 100;
/// Not spec-pinned — threads carry a `title` shown in search/the
/// public read path; 200 mirrors a generous forum-topic-title ceiling.
const MAX_THREAD_TITLE_LEN: usize = 200;
/// Not spec-pinned — a defense-in-depth ceiling on `server_role` row growth
/// per server, same motivation as `MAX_GROUP_DM_PARTICIPANTS`.
pub(crate) const MAX_ROLES_PER_SERVER: usize = 250;
/// Not spec-pinned — a defense-in-depth ceiling on `channel` row growth per
/// server (text/voice only, threads are unbounded on purpose), same
/// motivation as `MAX_ROLES_PER_SERVER`. 500 mirrors a generous Discord-like
/// ceiling.
pub(crate) const MAX_CHANNELS_PER_SERVER: usize = 500;
/// Not spec-pinned — the API only requires that "content limits are
/// validated server-side", with no number — 4000 mirrors a generous
/// Discord-like ceiling while staying far under Postgres's `TEXT` limit.
const MAX_MESSAGE_CONTENT_LEN: usize = 4000;
/// Not spec-pinned — nothing specifies a group-dm size cap — bounds
/// `channel_member` row growth per
/// group-dm creation call to a sane ceiling, same defense-in-depth
/// motivation as the message content cap above.
const MAX_GROUP_DM_PARTICIPANTS: usize = 50;

pub(crate) fn validate_server_name(name: &str) -> Result<(), DomainError> {
    if name.trim().is_empty() {
        return Err(DomainError::Validation(
            "server name must not be empty".to_string(),
        ));
    }

    if name.chars().count() > MAX_SERVER_NAME_LEN {
        return Err(DomainError::Validation(format!(
            "server name must be at most {MAX_SERVER_NAME_LEN} characters"
        )));
    }

    Ok(())
}

/// Matches the `server.visibility` CHECK constraint
/// (`public|private|unlisted`) exactly — validated here too so a bad value
/// comes back as a 400 `validation_failed` instead of surfacing as an
/// opaque database constraint error.
pub(crate) fn validate_visibility(visibility: &str) -> Result<(), DomainError> {
    match visibility {
        "public" | "private" | "unlisted" => Ok(()),
        _ => Err(DomainError::Validation(
            "visibility must be one of public, private, unlisted".to_string(),
        )),
    }
}

pub(crate) fn validate_channel_name(name: &str) -> Result<(), DomainError> {
    if name.trim().is_empty() {
        return Err(DomainError::Validation(
            "channel name must not be empty".to_string(),
        ));
    }

    if name.chars().count() > MAX_CHANNEL_NAME_LEN {
        return Err(DomainError::Validation(format!(
            "channel name must be at most {MAX_CHANNEL_NAME_LEN} characters"
        )));
    }

    Ok(())
}

/// Narrows a caller-supplied channel kind to the two a *server* channel may
/// have. `dm`/`group_dm` are real kinds but deliberately rejected here: they
/// carry a NULL `server_id` by construction, so creating one through the
/// server-channel route would violate the `channel_check` constraint. `None`
/// defaults to `text`, keeping every pre-voice caller working unchanged.
///
/// Voice is shape-only for now: a channel may be *marked* as voice so a
/// client can show it, but no media transport exists yet — that piece is
/// still pending its own spike.
pub(crate) fn validate_channel_kind(kind: Option<&str>) -> Result<&str, DomainError> {
    match kind {
        None => Ok("text"),
        Some(k @ ("text" | "voice")) => Ok(k),
        Some(_) => Err(DomainError::Validation(
            "channel kind must be one of text, voice".to_string(),
        )),
    }
}

/// Not spec-pinned — bounds the search query text. `websearch_to_tsquery`
/// (the caller, `db::message::search_in_server`) already degrades
/// gracefully on odd syntax, so this only guards the empty/oversized ends,
/// same shape as every other free-text validator here.
const MAX_SEARCH_QUERY_LEN: usize = 500;

pub(crate) fn validate_search_query(query: &str) -> Result<(), DomainError> {
    if query.trim().is_empty() {
        return Err(DomainError::Validation(
            "search query must not be empty".to_string(),
        ));
    }

    if query.chars().count() > MAX_SEARCH_QUERY_LEN {
        return Err(DomainError::Validation(format!(
            "search query must be at most {MAX_SEARCH_QUERY_LEN} characters"
        )));
    }

    Ok(())
}

/// Validates a thread's `title`. Same shape as `validate_channel_name`, just
/// a longer ceiling — a thread title reads more like a topic/subject line
/// than a channel name.
pub(crate) fn validate_thread_title(title: &str) -> Result<(), DomainError> {
    if title.trim().is_empty() {
        return Err(DomainError::Validation(
            "thread title must not be empty".to_string(),
        ));
    }

    if title.chars().count() > MAX_THREAD_TITLE_LEN {
        return Err(DomainError::Validation(format!(
            "thread title must be at most {MAX_THREAD_TITLE_LEN} characters"
        )));
    }

    Ok(())
}

/// URL-safe slug for a thread's canonical URL: lowercased,
/// every run of non-alphanumeric characters collapsed to one `-`, leading/
/// trailing `-` trimmed, capped at 80 characters so a very long title
/// doesn't produce an unwieldy URL. Never empty — a title that slugifies to
/// nothing (all punctuation/emoji) falls back to `"thread"`.
///
/// This alone does not guarantee uniqueness among a parent channel's
/// threads (two different threads can share a title) — the caller
/// (`DomainService::create_thread`) appends a short id-derived suffix for
/// that, which is what the partial unique index
/// (`idx_channel_thread_slug`) actually enforces at the DB level.
pub(crate) fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    let mut last_was_dash = false;
    for ch in title.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    if slug.ends_with('-') {
        slug.pop();
    }
    slug.truncate(80);

    if slug.is_empty() {
        "thread".to_string()
    } else {
        slug
    }
}

pub(crate) fn validate_message_content(content: &str) -> Result<(), DomainError> {
    if content.trim().is_empty() {
        return Err(DomainError::Validation(
            "message content must not be empty".to_string(),
        ));
    }

    if content.chars().count() > MAX_MESSAGE_CONTENT_LEN {
        return Err(DomainError::Validation(format!(
            "message content must be at most {MAX_MESSAGE_CONTENT_LEN} characters"
        )));
    }

    Ok(())
}

/// Scans `content` for `@everyone`, `@here`, and `@<role-slug>`
/// tokens — a token is `@` followed by a run of ASCII alphanumerics/hyphens,
/// lowercased. Returns each DISTINCT token found (without the leading `@`),
/// first-seen order. Pure text scan, no DB access: the caller (`DomainService`)
/// resolves which tokens actually name a real role and whether the poster
/// may use them — a token matching nothing real (a typo, a stray "@" in
/// prose) is simply not a mention and is never rejected.
pub(crate) fn extract_mention_tokens(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = content.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '@' {
            continue;
        }
        let mut token = String::new();
        while let Some(&(_, c)) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '-' {
                token.push(c.to_ascii_lowercase());
                chars.next();
            } else {
                break;
            }
        }
        if !token.is_empty() && !tokens.contains(&token) {
            tokens.push(token);
        }
    }
    tokens
}

pub(crate) fn validate_role_name(name: &str) -> Result<(), DomainError> {
    if name.trim().is_empty() {
        return Err(DomainError::Validation(
            "role name must not be empty".to_string(),
        ));
    }

    if name.chars().count() > MAX_ROLE_NAME_LEN {
        return Err(DomainError::Validation(format!(
            "role name must be at most {MAX_ROLE_NAME_LEN} characters"
        )));
    }

    Ok(())
}

/// `#` + 3/4/6/8 hex digits — the shorthand and alpha-channel forms a CSS
/// color picker can round-trip, not just `#rrggbb`. `None` (no color
/// override) is always valid and never reaches this function.
pub(crate) fn validate_role_color(color: &str) -> Result<(), DomainError> {
    let hex = color.strip_prefix('#');
    let valid_len = matches!(hex.map(str::len), Some(3 | 4 | 6 | 8));

    if !valid_len || !hex.is_some_and(|h| h.chars().all(|c| c.is_ascii_hexdigit())) {
        return Err(DomainError::Validation(
            "color must be a hex string like #a6f704".to_string(),
        ));
    }

    Ok(())
}

/// Dedupes `account_ids`, drops the creator if present (they're always added
/// separately by the caller), and enforces the "at least 2 other
/// participants" floor that distinguishes a group dm from a plain 1:1 dm —
/// and the ceiling above. Returns the cleaned list for the service layer to
/// use directly, so a caller can't smuggle in a duplicate or self-reference
/// by construction.
pub(crate) fn validate_group_dm_participants(
    account_ids: &[Uuid],
    creator_account_id: Uuid,
) -> Result<Vec<Uuid>, DomainError> {
    let mut distinct: Vec<Uuid> = account_ids
        .iter()
        .copied()
        .filter(|id| *id != creator_account_id)
        .collect();
    distinct.sort();
    distinct.dedup();

    if distinct.len() < 2 {
        return Err(DomainError::Validation(
            "a group dm requires at least 2 other participants".to_string(),
        ));
    }

    if distinct.len() > MAX_GROUP_DM_PARTICIPANTS {
        return Err(DomainError::Validation(format!(
            "a group dm supports at most {MAX_GROUP_DM_PARTICIPANTS} other participants"
        )));
    }

    Ok(distinct)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_server_name_accepts_non_empty_text() {
        assert!(validate_server_name("My Server").is_ok());
    }

    #[test]
    fn validate_server_name_rejects_empty_or_whitespace_only() {
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("   ").is_err());
    }

    #[test]
    fn validate_server_name_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_SERVER_NAME_LEN + 1);
        assert!(validate_server_name(&too_long).is_err());
    }

    #[test]
    fn validate_server_name_accepts_exactly_the_max_length() {
        let max_len = "a".repeat(MAX_SERVER_NAME_LEN);
        assert!(validate_server_name(&max_len).is_ok());
    }

    #[test]
    fn validate_visibility_accepts_the_three_known_values() {
        assert!(validate_visibility("public").is_ok());
        assert!(validate_visibility("private").is_ok());
        assert!(validate_visibility("unlisted").is_ok());
    }

    #[test]
    fn validate_visibility_rejects_unknown_values() {
        assert!(validate_visibility("secret").is_err());
        assert!(validate_visibility("").is_err());
    }

    #[test]
    fn validate_channel_name_accepts_non_empty_text() {
        assert!(validate_channel_name("general").is_ok());
    }

    #[test]
    fn validate_channel_name_rejects_empty_or_whitespace_only() {
        assert!(validate_channel_name("").is_err());
        assert!(validate_channel_name("   ").is_err());
    }

    #[test]
    fn validate_channel_name_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_CHANNEL_NAME_LEN + 1);
        assert!(validate_channel_name(&too_long).is_err());
    }

    #[test]
    fn validate_channel_kind_defaults_to_text_when_omitted() {
        assert_eq!(validate_channel_kind(None).expect("None is valid"), "text");
    }

    #[test]
    fn validate_channel_kind_accepts_text_and_voice() {
        assert_eq!(
            validate_channel_kind(Some("text")).expect("text is valid"),
            "text"
        );
        assert_eq!(
            validate_channel_kind(Some("voice")).expect("voice is valid"),
            "voice"
        );
    }

    #[test]
    fn validate_channel_kind_rejects_serverless_and_unknown_kinds() {
        // `dm`/`group_dm` are real kinds, but never creatable through the
        // server-channel route — they'd need a NULL server_id.
        for kind in ["dm", "group_dm", "video", "", "TEXT"] {
            assert!(
                matches!(
                    validate_channel_kind(Some(kind)),
                    Err(DomainError::Validation(_))
                ),
                "kind {kind:?} must be rejected"
            );
        }
    }

    #[test]
    fn validate_message_content_accepts_non_empty_text() {
        assert!(validate_message_content("hello").is_ok());
    }

    #[test]
    fn validate_message_content_rejects_empty_or_whitespace_only() {
        assert!(validate_message_content("").is_err());
        assert!(validate_message_content("   ").is_err());
    }

    #[test]
    fn validate_message_content_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_MESSAGE_CONTENT_LEN + 1);
        assert!(validate_message_content(&too_long).is_err());
    }

    #[test]
    fn validate_message_content_accepts_exactly_the_max_length() {
        let max_len = "a".repeat(MAX_MESSAGE_CONTENT_LEN);
        assert!(validate_message_content(&max_len).is_ok());
    }

    #[test]
    fn validate_role_name_accepts_non_empty_text() {
        assert!(validate_role_name("moderator").is_ok());
    }

    #[test]
    fn validate_role_name_rejects_empty_or_whitespace_only() {
        assert!(validate_role_name("").is_err());
        assert!(validate_role_name("   ").is_err());
    }

    #[test]
    fn validate_role_name_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_ROLE_NAME_LEN + 1);
        assert!(validate_role_name(&too_long).is_err());
    }

    #[test]
    fn validate_role_color_accepts_3_4_6_and_8_digit_hex() {
        assert!(validate_role_color("#abc").is_ok());
        assert!(validate_role_color("#abcd").is_ok());
        assert!(validate_role_color("#a6f704").is_ok());
        assert!(validate_role_color("#a6f704ff").is_ok());
    }

    #[test]
    fn validate_role_color_rejects_missing_hash_wrong_length_or_non_hex() {
        assert!(validate_role_color("a6f704").is_err());
        assert!(validate_role_color("#12345").is_err());
        assert!(validate_role_color("#gggggg").is_err());
        assert!(validate_role_color("#").is_err());
    }

    #[test]
    fn validate_group_dm_participants_accepts_two_distinct_others() {
        let creator = app_core::new_id();
        let a = app_core::new_id();
        let b = app_core::new_id();

        let result = validate_group_dm_participants(&[a, b], creator).expect("valid participants");
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(result, expected);
    }

    #[test]
    fn validate_group_dm_participants_drops_the_creator_if_present() {
        let creator = app_core::new_id();
        let a = app_core::new_id();
        let b = app_core::new_id();

        let result =
            validate_group_dm_participants(&[creator, a, b], creator).expect("valid participants");
        assert_eq!(result.len(), 2);
        assert!(!result.contains(&creator));
    }

    #[test]
    fn validate_group_dm_participants_dedupes_repeated_ids() {
        let creator = app_core::new_id();
        let a = app_core::new_id();
        let b = app_core::new_id();

        let result =
            validate_group_dm_participants(&[a, a, b, b], creator).expect("valid participants");
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn validate_group_dm_participants_rejects_fewer_than_two_others() {
        let creator = app_core::new_id();
        let a = app_core::new_id();

        assert!(matches!(
            validate_group_dm_participants(&[], creator),
            Err(DomainError::Validation(_))
        ));
        assert!(matches!(
            validate_group_dm_participants(&[a], creator),
            Err(DomainError::Validation(_))
        ));
        // Only the creator, deduped away to zero others.
        assert!(matches!(
            validate_group_dm_participants(&[creator], creator),
            Err(DomainError::Validation(_))
        ));
    }

    #[test]
    fn validate_search_query_accepts_non_empty_text() {
        assert!(validate_search_query("hello world").is_ok());
    }

    #[test]
    fn validate_search_query_rejects_empty_or_whitespace_only() {
        assert!(validate_search_query("").is_err());
        assert!(validate_search_query("   ").is_err());
    }

    #[test]
    fn validate_search_query_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_SEARCH_QUERY_LEN + 1);
        assert!(validate_search_query(&too_long).is_err());
    }

    #[test]
    fn validate_thread_title_accepts_non_empty_text() {
        assert!(validate_thread_title("How do I configure X?").is_ok());
    }

    #[test]
    fn validate_thread_title_rejects_empty_or_whitespace_only() {
        assert!(validate_thread_title("").is_err());
        assert!(validate_thread_title("   ").is_err());
    }

    #[test]
    fn validate_thread_title_rejects_over_the_max_length() {
        let too_long = "a".repeat(MAX_THREAD_TITLE_LEN + 1);
        assert!(validate_thread_title(&too_long).is_err());
    }

    #[test]
    fn slugify_lowercases_and_collapses_punctuation_to_single_dashes() {
        assert_eq!(slugify("How do I configure X?"), "how-do-i-configure-x");
        assert_eq!(slugify("  leading and trailing  "), "leading-and-trailing");
        assert_eq!(slugify("multiple---dashes___here"), "multiple-dashes-here");
    }

    #[test]
    fn slugify_falls_back_to_thread_for_all_punctuation_titles() {
        assert_eq!(slugify("???"), "thread");
        assert_eq!(slugify(""), "thread");
    }

    #[test]
    fn slugify_truncates_very_long_titles() {
        let long_title = "a".repeat(200);
        assert_eq!(slugify(&long_title).len(), 80);
    }

    #[test]
    fn validate_group_dm_participants_rejects_over_the_max() {
        let creator = app_core::new_id();
        let too_many: Vec<Uuid> = (0..=MAX_GROUP_DM_PARTICIPANTS).map(|_| app_core::new_id()).collect();

        assert!(matches!(
            validate_group_dm_participants(&too_many, creator),
            Err(DomainError::Validation(_))
        ));
    }
}
