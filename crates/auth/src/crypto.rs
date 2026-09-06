//! Pure cryptographic helpers with no database dependency — password
//! hashing/verification (argon2) and session token generation/hashing
//! (rand CSPRNG + SHA-256). Never hand
//! rolled: both primitives come straight from vetted crates.

use std::sync::OnceLock;

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand::RngExt;
use sha2::{Digest, Sha256};

use crate::error::AuthError;

/// Hashes a plaintext password with argon2 (`Argon2::default()`, a fresh
/// CSPRNG-generated salt per call). Returns the PHC-formatted string stored
/// in `auth_identity.password_hash`.
pub(crate) fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        // Argon2::default() uses fixed, valid parameters, so this branch is
        // not reachable in practice for any password we accept (validation
        // already bounds the length) — handled explicitly rather than
        // unwrapped per the no-panic-on-fallible-paths rule.
        .map_err(|err| AuthError::Validation(format!("password hashing failed: {err}")))
        .map(|hash| hash.to_string())
}

/// Verifies a plaintext password against a PHC-formatted argon2 hash.
/// Constant-time via the argon2 crate itself. Returns `false` (never an
/// error) for a malformed stored hash — that's a server-side data problem,
/// not something the caller can act on differently from a wrong password.
pub(crate) fn verify_password(password: &str, stored_hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

static DUMMY_PASSWORD_HASH: OnceLock<String> = OnceLock::new();

/// A valid, precomputed argon2 hash that no real password matches. `login`
/// verifies against this when no real stored hash exists (unknown email,
/// or an identity with no password set), so the CPU cost — and therefore
/// the response latency — is the same whether the account exists or not.
/// Skipping this on the "not found" path would let an attacker enumerate
/// registered emails purely from how fast the login endpoint replies.
pub(crate) fn dummy_password_hash() -> &'static str {
    DUMMY_PASSWORD_HASH.get_or_init(|| {
        hash_password("dummy-password-for-constant-time-verification")
            // Same practically-unreachable case as in `hash_password` itself
            // (fixed, valid Argon2::default() params) — fall back to a
            // literal PHC string rather than panicking on a fallible path.
            .unwrap_or_else(|_| {
                "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$\
                 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                    .to_string()
            })
    })
}

/// Generates a fresh opaque session token: 32 bytes from the process CSPRNG,
/// hex-encoded so it is safe to place in a header, cookie, or JSON body.
pub(crate) fn generate_session_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    hex_encode(&bytes)
}

/// Hashes an opaque session token for storage as `session.token_hash`.
/// SHA-256, not argon2/bcrypt: session tokens are already high-entropy
/// random strings, not human-chosen passwords, so a fast hash is the
/// correct choice.
pub(crate) fn hash_token(raw_token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw_token.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_password_round_trips_with_verify_password() {
        let hash = hash_password("correct horse battery staple").expect("hashing succeeds");

        assert!(verify_password("correct horse battery staple", &hash));
    }

    #[test]
    fn verify_password_rejects_wrong_password() {
        let hash = hash_password("correct horse battery staple").expect("hashing succeeds");

        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn verify_password_rejects_malformed_stored_hash() {
        assert!(!verify_password("anything", "not-a-phc-hash"));
    }

    #[test]
    fn hash_password_uses_a_fresh_salt_each_call() {
        let first = hash_password("same password").expect("hashing succeeds");
        let second = hash_password("same password").expect("hashing succeeds");

        // Same input, different output — proves the salt isn't reused.
        assert_ne!(first, second);
        // But both still verify against the original password.
        assert!(verify_password("same password", &first));
        assert!(verify_password("same password", &second));
    }

    #[test]
    fn generate_session_token_produces_unique_high_entropy_tokens() {
        let a = generate_session_token();
        let b = generate_session_token();

        assert_ne!(a, b);
        // 32 random bytes, hex-encoded => 64 hex characters.
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_token_is_deterministic_and_hex_encoded() {
        let token = "fixed-example-token";

        let first = hash_token(token);
        let second = hash_token(token);

        assert_eq!(first, second, "same input must hash to the same digest");
        // SHA-256 => 32 bytes => 64 hex characters.
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_token_differs_for_different_tokens() {
        assert_ne!(hash_token("token-a"), hash_token("token-b"));
    }

    #[test]
    fn dummy_password_hash_is_valid_and_stable() {
        let first = dummy_password_hash();
        let second = dummy_password_hash();

        // Memoized via OnceLock: same value on every call.
        assert_eq!(first, second);
        // Must be a real, parseable PHC hash, or verify_password would take
        // its fast "malformed hash" exit instead of doing the argon2 work
        // this whole helper exists to force.
        assert!(!verify_password("anything", first));
        assert!(PasswordHash::new(first).is_ok());
    }
}
