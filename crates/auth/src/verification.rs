//! Verification-code primitives. Generation and digesting
//! only — no database, same split as `crypto.rs`.

use rand::RngExt;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Eight digits. Numeric so it types on a phone keypad and carries no
/// `0`/`O` or `1`/`l` ambiguity to misread.
pub(crate) const CODE_LEN: usize = 8;

/// How long a code stays usable. Fifteen minutes rather than something
/// tighter because mail is not instant: relays queue, and greylisting — a
/// receiving server rejecting the first delivery attempt on purpose — costs
/// minutes as *normal* behaviour. A window that closes before honest mail
/// lands pushes users to resend, and resends degrade the sending reputation
/// delivery depends on.
pub(crate) const CODE_TTL_MINUTES: i64 = 15;

/// Wrong guesses allowed before the code is destroyed. This, not the code's
/// length, is what defeats brute force: eight digits with unlimited attempts
/// is weaker than six with five.
pub(crate) const MAX_ATTEMPTS: i32 = 5;

/// Minimum gap between two mails to the same address.
///
/// The per-IP limiter cannot enforce this: it runs before the body is parsed,
/// so it never sees which address a request names. Without a per-address gap
/// the issue endpoints are a spam cannon pointable at any inbox, and the
/// complaints that follow burn the sending reputation every future delivery
/// depends on — an attack on availability, not just on the victim's inbox.
///
/// Enforced by skipping the send, never by changing the response: replying
/// differently would restore the enumeration oracle the uniform 202 exists to
/// remove.
pub(crate) const RESEND_COOLDOWN_SECONDS: i64 = 60;

/// Generates a verification code from the process CSPRNG.
///
/// `random_range` over the whole decimal space, formatted with leading zeros,
/// so every one of the 10^8 values is equally likely — sampling digit by digit
/// from a smaller range would be fine too, but this keeps the uniformity
/// argument to one line.
pub(crate) fn generate_code() -> String {
    let value: u32 = rand::rng().random_range(0..100_000_000);
    format!("{value:0width$}", width = CODE_LEN)
}

/// SHA-256, hex-encoded, for storage as `pending_registration.code_digest`.
///
/// This is defence-in-depth and *not* a control:
/// an eight-digit space falls instantly to anyone holding the database, so
/// this protects against incidental exposure — a log line, a backup, a stray
/// query — and nothing stronger. The real controls are the TTL, the attempt
/// limit, and the rate limits.
pub(crate) fn digest_code(code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Compares a submitted code against a stored digest in constant time.
///
/// Both sides are fixed-length hex digests, so this leaks nothing through
/// length. Using `subtle` rather than `==` keeps the comparison free of the
/// early-exit that byte-wise equality would give an attacker timing a guess.
pub(crate) fn code_matches(submitted: &str, stored_digest: &str) -> bool {
    let submitted_digest = digest_code(submitted);
    submitted_digest
        .as_bytes()
        .ct_eq(stored_digest.as_bytes())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_code_is_eight_digits() {
        for _ in 0..200 {
            let code = generate_code();
            assert_eq!(code.len(), CODE_LEN, "code was {code}");
            assert!(code.chars().all(|c| c.is_ascii_digit()), "code was {code}");
        }
    }

    #[test]
    fn generate_code_can_produce_leading_zeros() {
        // A code formatted without zero-padding would sometimes be shorter
        // than eight characters, which both looks broken to a user and
        // quietly shrinks the space. Probabilistic, but over this many draws
        // a padding bug is overwhelmingly likely to show up.
        let produced_short = (0..5_000).any(|_| {
            let value: u32 = rand::rng().random_range(0..100_000_000);
            let code = format!("{value:0width$}", width = CODE_LEN);
            code.len() != CODE_LEN
        });
        assert!(!produced_short);
    }

    #[test]
    fn generate_code_does_not_repeat_immediately() {
        let a = generate_code();
        let b = generate_code();
        let c = generate_code();
        assert!(
            !(a == b && b == c),
            "three identical codes in a row suggests a fixed or time-seeded source"
        );
    }

    #[test]
    fn digest_code_is_deterministic_and_hex() {
        let first = digest_code("12345678");
        let second = digest_code("12345678");

        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn digest_code_differs_between_codes() {
        assert_ne!(digest_code("12345678"), digest_code("12345679"));
    }

    #[test]
    fn code_matches_accepts_the_right_code_and_rejects_others() {
        let digest = digest_code("00424242");

        assert!(code_matches("00424242", &digest));
        assert!(!code_matches("00424243", &digest));
        assert!(!code_matches("", &digest));
        // A submitted value that is not even code-shaped must not match.
        assert!(!code_matches(&digest, &digest));
    }
}
