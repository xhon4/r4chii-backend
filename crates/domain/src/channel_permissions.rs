//! Per-channel role permission bits — a separate bitmask
//! namespace from `crate::permissions`'s server-wide one. A channel-scoped
//! bit and a server-scoped bit answer different questions ("can this role
//! see THIS channel" vs "can this role kick ANY member"); conflating their
//! bit positions would make a future server-wide bit collide with a
//! channel-scoped one's meaning for no benefit — see the ADR.

/// Whether a role may see a `restricted` channel at all. The only bit this
/// ADR enforces — see below.
pub const VIEW_CHANNEL: i64 = 1 << 0;
/// Reserved, NOT enforced — every role that can view a channel
/// may still post in it exactly as today. Wiring this up means deciding
/// what a channel a role can view-but-not-post-in means (read-only for
/// them? does view already imply participate?), a real question this ADR
/// doesn't need answered to solve the actual request. Stable bit position
/// reserved now, same precedent `permissions::MANAGE_CHANNELS` set through
/// all of M2 before its own routes existed.
pub const SEND_MESSAGE: i64 = 1 << 1;
/// Reserved, NOT enforced — same reasoning as `SEND_MESSAGE`.
pub const JOIN_VOICE: i64 = 1 << 2;

pub fn has(permissions: i64, bit: i64) -> bool {
    permissions & bit == bit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_detects_a_set_bit_among_others() {
        let permissions = VIEW_CHANNEL | JOIN_VOICE;
        assert!(has(permissions, VIEW_CHANNEL));
        assert!(has(permissions, JOIN_VOICE));
        assert!(!has(permissions, SEND_MESSAGE));
    }

    #[test]
    fn has_is_false_for_zero_permissions() {
        assert!(!has(0, VIEW_CHANNEL));
    }

    #[test]
    fn every_bit_is_distinct() {
        let bits = [VIEW_CHANNEL, SEND_MESSAGE, JOIN_VOICE];
        for (i, a) in bits.iter().enumerate() {
            for (j, b) in bits.iter().enumerate() {
                if i != j {
                    assert_eq!(a & b, 0, "bits at {i} and {j} overlap");
                }
            }
        }
    }
}
