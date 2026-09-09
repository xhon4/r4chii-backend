//! Per-channel role permission bits — a separate bitmask
//! namespace from `crate::permissions`'s server-wide one. A channel-scoped
//! bit and a server-scoped bit answer different questions ("can this role
//! see THIS channel" vs "can this role kick ANY member"); conflating their
//! bit positions would make a future server-wide bit collide with a
//! channel-scoped one's meaning for no benefit — see the ADR.

/// Whether a role may see a `restricted` channel at all.
pub const VIEW_CHANNEL: i64 = 1 << 0;

pub use crate::permissions::has;
