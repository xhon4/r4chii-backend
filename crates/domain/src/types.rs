use chrono::{DateTime, Utc};

use app_core::Uuid;

/// Input to `DomainService::create_server`. Raw, unvalidated user input —
/// validation happens inside `create_server` itself, same pattern as
/// `auth::RegisterInput`.
#[derive(Debug, Clone)]
pub struct CreateServerInput {
    pub name: String,
    /// `None` defers to the `server.visibility` column's own DB default
    /// (`private`) rather than the service layer picking one, so the two
    /// stay in exactly one place.
    pub visibility: Option<String>,
}

/// Input to `DomainService::create_channel`.
#[derive(Debug, Clone)]
pub struct CreateChannelInput {
    pub name: String,
    /// `None` means `text` — the default that keeps every pre-voice caller
    /// working unchanged. Only `text`/`voice` are accepted for a server
    /// channel; `validate_channel_kind` rejects anything else, including the
    /// real-but-serverless `dm`/`group_dm` kinds.
    pub kind: Option<String>,
}

/// Public-safe view of a `server` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSummary {
    pub id: Uuid,
    pub name: String,
    pub icon_url: Option<String>,
    pub visibility: String,
    pub owner_account_id: Uuid,
    /// `Some(..)` only when the caller viewing this summary is that
    /// server's owner — a business rule about who may see the invite code,
    /// decided in the service layer (not `api`), per the least-privilege
    /// default this slice was told to keep.
    pub invite_code: Option<String>,
    pub created_at: DateTime<Utc>,
    /// `None` means this server isn't in the caller's curated "ur spaces"
    /// list; `Some(position)` is its place in that list (lower = earlier).
    pub spaces_position: Option<i32>,
}

/// Public-safe view of a `channel` row. `server_id` is `None` for `dm`/
/// `group_dm` channels, `Some` for `text` channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSummary {
    pub id: Uuid,
    pub server_id: Option<Uuid>,
    pub kind: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    /// `None` = inherit the server's visibility unchanged; `Some(..)`
    /// narrows it. Always `None` for `dm`/`group_dm`.
    pub visibility: Option<String>,
    /// The parent text channel a `thread` row belongs to. `None`
    /// for every other kind.
    pub parent_channel_id: Option<Uuid>,
    /// The message a thread was spawned from, if any (`None` for a
    /// standalone thread or any non-thread kind).
    pub root_message_id: Option<Uuid>,
    /// A thread's display title (`None` for every non-thread kind, which use
    /// `name` instead).
    pub title: Option<String>,
    /// URL-safe, unique per `parent_channel_id` (`None` for every non-thread
    /// kind).
    pub slug: Option<String>,
    /// RAW value of this channel's own column — `false` for every
    /// thread regardless of its parent (a thread never carries its own
    /// restriction, it inherits the parent's — see
    /// `DomainService::member_can_view_channel`).
    pub restricted: bool,
    /// Every account in a `dm`/`group_dm`, oldest membership first. Always
    /// empty for a server channel: `channel_member` is a DM-only concept
    /// here, and a server channel's audience is its membership roster.
    ///
    /// A DM has no `name`, so this is the only thing a client can label one
    /// with — including a DM someone else opened.
    pub participant_ids: Vec<Uuid>,
}

/// Input to `DomainService::rename_channel` — `name` for `text`/`voice`,
/// `title` for `thread`. Exactly one must be `Some`.
#[derive(Debug, Clone)]
pub struct RenameChannelInput {
    pub name: Option<String>,
    pub title: Option<String>,
}

/// Input to `DomainService::reorder_channels` — the full ordered list of
/// every live non-thread channel id in the server.
#[derive(Debug, Clone)]
pub struct ReorderChannelsInput {
    pub ordered_channel_ids: Vec<Uuid>,
}

/// Input to `DomainService::create_thread`.
#[derive(Debug, Clone)]
pub struct CreateThreadInput {
    pub title: String,
    /// The message this thread replies to, if any — `None` for a standalone
    /// thread (a new top-level topic in the channel).
    pub root_message_id: Option<Uuid>,
}

/// One message on the public read path — a smaller, purpose-built
/// shape distinct from `MessageSummary`: no `edited_at`/`deleted_at`/
/// `author_account_id` (the anonymous page has no use for a raw account id
/// and never shows a soft-deleted row at all), and it carries the author's
/// display name directly since there is no client-side account cache to
/// resolve an id against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicMessageSummary {
    pub id: Uuid,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub author_display_name: String,
}

/// One row of the public sitemap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitemapThread {
    pub id: Uuid,
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

/// One `export_job` row. `download_url` is `Some` only once
/// `status == "done"` — cached at completion time by the worker, not
/// re-presigned per request (see `migrations/0014_export_jobs.sql`'s own
/// comment on why).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportJobSummary {
    pub id: Uuid,
    pub server_id: Uuid,
    /// `pending` | `running` | `done` | `failed`.
    pub status: String,
    pub download_url: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Input to `DomainService::search_messages`. Every filter beyond
/// `query` is optional; omitted means "no constraint on that axis".
#[derive(Debug, Clone, Default)]
pub struct SearchInput {
    pub query: String,
    pub author_account_id: Option<Uuid>,
    pub channel_id: Option<Uuid>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
    /// Cursor: the last-seen message id from a previous page — an opaque
    /// UUIDv7 id.
    pub before: Option<Uuid>,
}

/// Input to `DomainService::create_group_dm`. The caller (`account_id` in
/// the service call) is always added as a member alongside these — not
/// repeated here.
#[derive(Debug, Clone)]
pub struct CreateGroupDmInput {
    pub account_ids: Vec<Uuid>,
}

/// Input to `DomainService::send_message`.
#[derive(Debug, Clone)]
pub struct SendMessageInput {
    pub content: String,
}

/// Input to `DomainService::edit_message`.
#[derive(Debug, Clone)]
pub struct EditMessageInput {
    pub content: String,
}

/// Cursor pagination input for `DomainService::list_messages` — an opaque
/// last-seen UUIDv7 id, capped at 100. `limit: None` defers to the service's own default rather than the
/// caller picking one.
#[derive(Debug, Clone, Default)]
pub struct MessagePagination {
    pub limit: Option<u32>,
    pub before: Option<Uuid>,
}

/// A `friendship` row from one specific account's point of view (ROADMAP
/// slice 7). `account_id` is always the OTHER party — never the caller —
/// mirroring how `ChannelSummary` never repeats "me" either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FriendshipSummary {
    pub id: Uuid,
    pub account_id: Uuid,
    /// `pending` | `accepted` (the `friendship.status` CHECK).
    pub status: String,
    pub requested_by: Uuid,
    pub created_at: DateTime<Utc>,
}

/// A `block` row the caller placed (ROADMAP slice 7). Only ever the caller's
/// own outbound blocks — `DomainService::list_blocks` never returns blocks
/// placed *against* the caller (a directional design; a block hides the
/// blocker's action from its target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSummary {
    pub id: Uuid,
    pub account_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Public-safe view of a `message` row. `content` is `Option` specifically
/// so a soft-deleted message can carry `None` while `deleted_at` stays
/// `Some(..)` — M0 delete semantics keep the row in `list_messages` results,
/// never filtered out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_account_id: Uuid,
    pub content: Option<String>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// `None` = not pinned.
    pub pinned_at: Option<DateTime<Utc>>,
}

/// Input to `DomainService::timeout_member`.
#[derive(Debug, Clone)]
pub struct TimeoutInput {
    pub until: DateTime<Utc>,
    pub reason: Option<String>,
}

/// A member of a server: their public profile plus the role they hold
/// there. Never carries `email` — that stays a caller's-own-profile field,
/// the same split the `api` crate draws between `AccountResponse` and
/// `ProfileResponse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerMemberSummary {
    pub account_id: Uuid,
    pub username: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub role: String,
    pub joined_at: DateTime<Utc>,
    /// M2: the member's EXPLICITLY assigned `server_role` ids — never
    /// includes the server's default role, which every member holds
    /// implicitly.
    pub role_ids: Vec<Uuid>,
    /// `None` means the account's own `display_name` renders
    /// instead.
    pub nickname: Option<String>,
    /// `Some(t)` in the future means this member is currently
    /// timed out.
    pub timeout_until: Option<DateTime<Utc>>,
    /// Present only to the timed-out member, server owner, and effective
    /// `ADMIN` viewers while the timeout remains active.
    pub timeout_reason: Option<String>,
}

/// A `server_role` row (M2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSummary {
    pub id: Uuid,
    pub server_id: Uuid,
    pub name: String,
    pub color: Option<String>,
    pub permissions: i64,
    pub position: i32,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    /// When `true`, ANY member may `@`-mention this role.
    pub mentionable: bool,
}

/// Input to `DomainService::create_role`.
#[derive(Debug, Clone)]
pub struct CreateRoleInput {
    pub name: String,
}

/// Input to `DomainService::update_role`. Every field is the FULL new value,
/// not a patch fragment — the service layer reads the existing row for
/// whatever the caller omits (`None`), so this struct itself stays simple.
/// No way to clear a role's color back to "no override" yet — every field
/// here is set-or-leave-alone; add a clear path if that's ever actually
/// needed rather than modeling it speculatively now.
#[derive(Debug, Clone)]
pub struct UpdateRoleInput {
    pub name: Option<String>,
    pub color: Option<String>,
    pub permissions: Option<i64>,
    /// `None` leaves the existing value alone, same "set-or-leave"
    /// shape as every other field here.
    pub mentionable: Option<bool>,
}

/// A `server_ban` row (M2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BanSummary {
    pub account_id: Uuid,
    pub banned_by: Uuid,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}
