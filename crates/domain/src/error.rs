use thiserror::Error;

/// Domain errors for the `domain` crate. No HTTP knowledge lives here — the
/// `api` crate maps each variant to a status code and error envelope
/// the same pattern as `auth::AuthError`.
#[derive(Debug, Error)]
pub enum DomainError {
    /// Also returned when the server exists but the caller has no
    /// `membership` row for it — same error, same 404, so a non-member can
    /// never learn a server id exists — 404 also covers resources the
    /// caller may not see. Mirrors how
    /// `auth::AuthError::SessionNotFound` collapses "not mine" and
    /// "doesn't exist" into one outcome.
    #[error("server not found")]
    ServerNotFound,

    /// No invite exists for the given code. Deliberately the same 404 shape
    /// as `ServerNotFound` — a bad guess must not distinguish "wrong code"
    /// from "no such server" (same non-leaking rationale).
    #[error("invalid invite code")]
    InvalidInvite,

    /// The caller already has a `membership` row for the target server.
    #[error("already a member of this server")]
    AlreadyMember,

    /// Also returned when the channel exists but the caller has no
    /// `membership` (text channel) or `channel_member` (dm/group_dm) row for
    /// it — same non-leaking rationale as `ServerNotFound`.
    #[error("channel not found")]
    ChannelNotFound,

    /// No `message` row exists with this id in this channel — either it was
    /// never there, or it belongs to a different channel (deliberately the
    /// same shape, a channel-scoped lookup never confirms cross-channel
    /// existence). Also returned for a message that IS soft-deleted: from an
    /// editing standpoint it's effectively gone (see
    /// `DomainService::edit_message`).
    #[error("message not found")]
    MessageNotFound,

    /// The message exists and the caller can see it (they're a member of its
    /// channel), but it was authored by someone else. Safe to distinguish
    /// from `MessageNotFound` here — unlike the ownership-hiding pattern
    /// used for sessions/servers, the caller already knows this message
    /// exists (they can read it via the channel they're a member of), so a
    /// 403 leaks nothing they don't already have.
    #[error("not the author of this message")]
    NotMessageAuthor,

    /// A dm/group-dm participant id (other than the caller) does not name an
    /// existing account. Unlike `ServerNotFound`/`ChannelNotFound`, this is
    /// caller-supplied input, not something the caller is trying to access —
    /// safe to report plainly rather than collapsed into a non-leaking 404.
    #[error("account not found")]
    AccountNotFound,

    /// No `friendship` row exists between the caller and the given account —
    /// returned by `remove_friendship` when there's nothing to decline,
    /// cancel, or unfriend.
    #[error("friend request not found")]
    FriendRequestNotFound,

    /// No `block` row exists between the caller and the given account —
    /// returned by `unblock_account` when there's nothing to remove.
    #[error("block not found")]
    BlockNotFound,

    /// Either party has blocked the other ("prevents DMs and friend
    /// requests between them"). Deliberately the same error whether
    /// the caller blocked the target or vice versa — this collapses both
    /// directions into one outcome so a caller can never learn *which* of
    /// the two placed the block, same non-leaking spirit as
    /// `ChannelNotFound`.
    #[error("blocked")]
    Blocked,

    #[error("validation failed: {0}")]
    Validation(String),

    /// M2's role model. A bad/foreign role id — caller
    /// error on input they already have server-membership context for, same
    /// rationale as `AccountNotFound`, not the non-leaking-404 treatment
    /// `ServerNotFound` gets.
    #[error("role not found")]
    RoleNotFound,

    /// The caller's roles don't carry the specific bit the action needs.
    #[error("missing permission")]
    MissingPermission,

    /// The caller holds the right permission bit, but the target (a role, or
    /// another member's top role) outranks or equals their own top role's
    /// `position`. Independent of `MissingPermission` — a bit says what kind
    /// of action, this says on whom.
    #[error("insufficient role hierarchy")]
    InsufficientHierarchy,

    /// Every server's implicit "everyone" role — cannot be deleted, and
    /// reordering doesn't apply to it (it's always `position = 0`).
    #[error("cannot modify the default role")]
    CannotModifyDefaultRole,

    /// Kick/ban's target is the server's owner. Not a hierarchy question —
    /// the owner has no `position` to be outranked at, they're simply exempt.
    #[error("cannot act on the server owner")]
    CannotActOnOwner,

    /// Kick/ban attempted against the caller's own membership — `leave` is
    /// the self-removal path, and reuses the same underlying service call
    /// with a different `RemovalReason`.
    #[error("cannot act on yourself")]
    CannotActOnSelf,

    /// Defense-in-depth cap mirroring `MAX_GROUP_DM_PARTICIPANTS`'s own
    /// reasoning — bounds `server_role` row growth per server.
    #[error("role limit reached")]
    RoleLimitReached,

    #[error("account is already banned")]
    AlreadyBanned,

    /// Blocks `join_via_invite` for a banned account — same spirit as
    /// `Blocked`, a hard stop rather than a leaky distinction.
    #[error("banned from this server")]
    Banned,

    /// The owner cannot use the self-service `leave` path — deleting the
    /// server is the only way an owner stops being in it.
    #[error("the owner cannot leave; delete the server instead")]
    OwnerCannotLeave,

    /// No `export_job` row for the given id under this server —
    /// same non-leaking rationale as `RoleNotFound` (by the time a caller
    /// can name a `job_id` at all they already passed `require_permission`
    /// for that server).
    #[error("export job not found")]
    ExportJobNotFound,

    /// The export worker's storage upload or presign step failed.
    /// Only ever produced server-side (the worker), never from caller
    /// input — carries the underlying `storage::StorageError`'s message for
    /// the `export_job.error` column, not for direct HTTP exposure.
    #[error("export failed: {0}")]
    ExportFailed(String),

    /// The caller holds `membership.timeout_until` in the future —
    /// blocks sending a message, creating a thread, or joining voice. Reading
    /// is unaffected.
    #[error("you are timed out until the timeout expires")]
    MemberTimedOut,

    /// The message content contains `@everyone`/`@here`, or
    /// `@<role-slug>` for a role that isn't `mentionable`, and the poster
    /// holds neither the matching bit nor an admin/owner bypass.
    #[error("you do not have permission to use that mention")]
    MentionNotAllowed,

    #[error("database error")]
    Database(#[source] sqlx::Error),
}

impl From<sqlx::Error> for DomainError {
    fn from(err: sqlx::Error) -> Self {
        DomainError::Database(err)
    }
}
