//! Bitwise role permissions (M2).
//!
//! A bit says WHAT a role can do. It says nothing about WHO it can be used
//! against — that's `position` (the hierarchy), checked separately by
//! `DomainService::check_hierarchy`. Keeping the two independent is the one
//! idea most worth preserving from the reference design this was adapted
//! from.

/// Bypasses every bit below (checked in `DomainService::require_permission`),
/// but NOT the hierarchy check — an admin role still can't touch an
/// equal-or-higher one unless its holder is also the owner.
pub const ADMIN: i64 = 1 << 0;
pub const MANAGE_ROLES: i64 = 1 << 1;
/// Gates creating a channel, flipping its `restricted` override, and
/// setting a role's per-channel grant (`create_channel`,
/// `update_channel_restricted`, `set_channel_role_permission`).
pub const MANAGE_CHANNELS: i64 = 1 << 2;
pub const KICK: i64 = 1 << 3;
pub const BAN: i64 = 1 << 4;
/// Gates flipping a channel's `visibility` override. Flipping
/// `server.visibility` itself stays owner-only (checked directly, like
/// `delete_server`), the same all-or-nothing tier a whole-community
/// decision gets rather than a per-channel bit.
pub const MANAGE_VISIBILITY: i64 = 1 << 5;
/// Gates using the literal `@everyone`/`@here` tokens in a
/// message's content — not a notification system (none exists yet), just
/// whether the server accepts the token at all from this poster.
pub const MENTION_EVERYONE: i64 = 1 << 6;
/// Gates `@<role-slug>` tokens for roles that are NOT themselves
/// `mentionable = true` — a role with that flag set can be mentioned by
/// anyone regardless of this bit (see `server_role.mentionable`).
pub const MENTION_ROLES: i64 = 1 << 7;
/// Setting another member's nickname. Setting your OWN stays
/// baseline (no bit needed) — this only gates touching someone else's.
pub const MANAGE_NICKNAMES: i64 = 1 << 8;
/// Setting or clearing `membership.timeout_until` on another
/// member. Targets a specific member, so `check_hierarchy` applies, same
/// tier as `KICK`/`BAN`.
pub const TIMEOUT_MEMBERS: i64 = 1 << 9;
/// Soft-deleting a message authored by someone else, in a server
/// channel. No hierarchy check — content moderation, not an action against
/// the author's standing (they keep every role and every access).
pub const MANAGE_MESSAGES: i64 = 1 << 10;
/// Pinning/unpinning a message. Kept separate from
/// `MANAGE_MESSAGES` — a server can hand out "pin the good stuff" without
/// also handing out delete power over everyone's messages.
pub const PIN_MESSAGES: i64 = 1 << 11;
/// Seeing and regenerating `server.invite_code` — previously
/// owner-only. Still exactly one code per server; this only widens who may
/// see/rotate it.
pub const MANAGE_INVITES: i64 = 1 << 12;

pub fn has(permissions: i64, bit: i64) -> bool {
    permissions & bit == bit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_detects_a_set_bit_among_others() {
        let permissions = MANAGE_ROLES | BAN;
        assert!(has(permissions, MANAGE_ROLES));
        assert!(has(permissions, BAN));
        assert!(!has(permissions, KICK));
        assert!(!has(permissions, ADMIN));
    }

    #[test]
    fn has_is_false_for_zero_permissions() {
        assert!(!has(0, ADMIN));
    }

    #[test]
    fn every_bit_is_distinct() {
        let bits = [
            ADMIN,
            MANAGE_ROLES,
            MANAGE_CHANNELS,
            KICK,
            BAN,
            MANAGE_VISIBILITY,
            MENTION_EVERYONE,
            MENTION_ROLES,
            MANAGE_NICKNAMES,
            TIMEOUT_MEMBERS,
            MANAGE_MESSAGES,
            PIN_MESSAGES,
            MANAGE_INVITES,
        ];
        for (i, a) in bits.iter().enumerate() {
            for (j, b) in bits.iter().enumerate() {
                if i != j {
                    assert_eq!(a & b, 0, "bits at {i} and {j} overlap");
                }
            }
        }
    }
}
