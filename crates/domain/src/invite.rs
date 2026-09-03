//! Invite code generation. A short, URL-safe, random, persistent code that
//! joins a server — "a simple invite mechanism" by design. No
//! expiry/revocation/rotation/multiple-codes-per-server in M0 — out of
//! scope for this slice, see `DomainService::join_via_invite`. CSPRNG via
//! the `rand` crate, same pattern as `auth::crypto`'s session token
//! generation — never hand-rolled.

use rand::RngExt;

const INVITE_CODE_LEN: usize = 10;
const INVITE_CODE_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Generates a fresh CSPRNG invite code, e.g. `"aZ3kP9mQ2x"`. Base62-alphabet
/// (URL-safe, no encoding needed) over 10 characters gives ~59.5 bits of
/// entropy — plenty for M0 scale. Collisions are caught by the
/// `server.invite_code` UNIQUE constraint; `create_server` runs this inside
/// its one transaction, so a (vanishingly unlikely) collision just fails
/// that insert rather than silently colliding with another server's code.
pub(crate) fn generate_invite_code() -> String {
    let mut rng = rand::rng();
    (0..INVITE_CODE_LEN)
        .map(|_| INVITE_CODE_ALPHABET[rng.random_range(0..INVITE_CODE_ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_invite_code_produces_the_expected_length() {
        let code = generate_invite_code();
        assert_eq!(code.chars().count(), INVITE_CODE_LEN);
    }

    #[test]
    fn generate_invite_code_only_uses_url_safe_alphanumeric_characters() {
        let code = generate_invite_code();
        assert!(code.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn generate_invite_code_produces_distinct_codes_across_calls() {
        let a = generate_invite_code();
        let b = generate_invite_code();
        assert_ne!(a, b);
    }
}
