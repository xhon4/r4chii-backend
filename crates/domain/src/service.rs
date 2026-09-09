use std::collections::{HashMap, HashSet};

use app_core::{new_id, Uuid};
use chrono::Utc;
use db::PgPool;

use crate::channel_permissions;
use crate::error::DomainError;
use crate::invite::generate_invite_code;
use crate::permissions;
use crate::types::{
    BanSummary, BlockSummary, ChannelSummary, CreateChannelInput, CreateGroupDmInput,
    CreateRoleInput, CreateServerInput, CreateThreadInput, EditMessageInput, ExportJobSummary,
    FriendshipSummary, MessagePagination, MessageSummary, PublicMessageSummary, RoleSummary,
    SearchInput, SendMessageInput, ServerMemberSummary, ServerSummary, SitemapThread, TimeoutInput,
    UpdateRoleInput,
};
use crate::validation::{
    extract_mention_tokens, slugify, validate_channel_kind, validate_channel_name,
    validate_group_dm_participants, validate_message_content, validate_role_color,
    validate_role_name, validate_search_query, validate_server_name, validate_thread_title,
    validate_visibility, MAX_CHANNELS_PER_SERVER, MAX_ROLES_PER_SERVER,
};

/// The channel kinds that live inside a server and are therefore authorized
/// by a `membership` row, as opposed to `dm`/`group_dm`, which are
/// authorized by a `channel_member` row. Voice joins `text` here because a
/// voice channel belongs to a server exactly like a text one does; `thread`
/// joins them too because a thread row carries its own
/// `server_id`, copied from its parent at creation.
const SERVER_CHANNEL_KINDS: [&str; 3] = ["text", "voice", "thread"];

fn is_server_channel(kind: &str) -> bool {
    SERVER_CHANNEL_KINDS.contains(&kind)
}

const DEFAULT_VISIBILITY: &str = "private";

/// Containment ordering for the visibility axis: a child's effective
/// visibility must rank no higher than its parent's. `private` (0) is always
/// legal as an override regardless of the parent's rank, because narrowing to
/// nothing is never "broader". Unknown strings rank as `private` — `validate_visibility`
/// rejects them before this is ever reached, so this arm is unreachable in
/// practice, not a silent default.
fn visibility_rank(visibility: &str) -> u8 {
    match visibility {
        "public" => 2,
        "unlisted" => 1,
        _ => 0,
    }
}

/// Default page size for `list_messages` when the caller omits `limit` — 50
/// matches the API's own cursor pagination convention (not spec-pinned as a
/// hard default, but chosen to match it).
pub const DEFAULT_MESSAGE_LIMIT: u32 = 50;

/// Hard cap on `list_messages`' `limit` ("`limit` capped at 100").
pub const MAX_MESSAGE_LIMIT: u32 = 100;

/// Servers, channels, memberships, DMs, friends, blocks, and messages — the
/// business/authorization layer. All SQL lives in the `db` crate's
/// repository modules (`db::server`, `db::channel`, `db::dm`,
/// `db::friendship`, `db::block`, `db::message`, `db::account`, `db::lock`)
/// per the crate-boundary rule; this file orchestrates them, owns transaction
/// boundaries, and maps rows to domain types.
#[derive(Clone)]
pub struct DomainService {
    pool: PgPool,
}

impl From<db::server::ServerWithRoleRow> for ServerSummary {
    fn from(row: db::server::ServerWithRoleRow) -> Self {
        // Least-privilege default: only the owner's own view of a server
        // carries its invite code.
        let invite_code = (row.role == "owner").then_some(row.invite_code);
        Self {
            id: row.id,
            name: row.name,
            icon_url: row.icon_url,
            visibility: row.visibility,
            owner_account_id: row.owner_account_id,
            invite_code,
            created_at: row.created_at,
        }
    }
}

/// `role_ids` defaults empty here — `list_members` fills it in afterward from
/// the bulk `role_ids_for_server_members` query, since a single row has no
/// way to know its own membership_id's assignments without an N+1 query.
impl From<db::server::ServerMemberRow> for ServerMemberSummary {
    fn from(row: db::server::ServerMemberRow) -> Self {
        Self {
            account_id: row.account_id,
            username: row.username,
            display_name: row.display_name,
            avatar_url: row.avatar_url,
            role: row.role,
            joined_at: row.joined_at,
            role_ids: Vec::new(),
            nickname: row.nickname,
            timeout_until: row.timeout_until,
        }
    }
}

impl From<db::server_role::ServerRoleRow> for RoleSummary {
    fn from(row: db::server_role::ServerRoleRow) -> Self {
        Self {
            id: row.id,
            server_id: row.server_id,
            name: row.name,
            color: row.color,
            permissions: row.permissions,
            position: row.position,
            is_default: row.is_default,
            created_at: row.created_at,
            mentionable: row.mentionable,
        }
    }
}

impl From<db::server_role::ServerBanRow> for BanSummary {
    fn from(row: db::server_role::ServerBanRow) -> Self {
        Self {
            account_id: row.account_id,
            banned_by: row.banned_by,
            reason: row.reason,
            created_at: row.created_at,
        }
    }
}

impl From<db::channel::ChannelRow> for ChannelSummary {
    fn from(row: db::channel::ChannelRow) -> Self {
        Self {
            id: row.id,
            server_id: row.server_id,
            kind: row.kind,
            name: row.name,
            created_at: row.created_at,
            visibility: row.visibility,
            parent_channel_id: row.parent_channel_id,
            root_message_id: row.root_message_id,
            title: row.title,
            slug: row.slug,
            restricted: row.restricted,
        }
    }
}

/// Who may read a channel/thread's content: an authenticated
/// member sees it via the ordinary authenticated surface; an unauthenticated
/// caller (or an authenticated non-member) may still see it if its effective
/// visibility is `public`/`unlisted` — the case the public read path
/// exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadAccess {
    Member,
    Public,
}

/// The facts `decide_profile_visibility` needs about one profile view.
/// Carries the raw `ProfileRow`; none of it is safe to serialize directly.
#[derive(Debug, Clone)]
pub struct ProfileContext {
    pub profile: db::profile::ProfileRow,
    pub relationship: crate::profile_visibility::ProfileViewerRelationship,
    pub caller_blocked_owner: bool,
    pub owner_blocked_caller: bool,
    pub server_context: Option<db::profile::ServerContextRow>,
    pub has_shared_server_context: bool,
}

impl ProfileContext {
    /// Projects the gathered facts into the policy's input.
    pub fn visibility_input(&self) -> crate::profile_visibility::ProfileVisibilityInput {
        crate::profile_visibility::ProfileVisibilityInput {
            relationship: self.relationship,
            vis_bio: crate::profile_visibility::ProfileVisibility::from_db(&self.profile.vis_bio),
            vis_communities: crate::profile_visibility::ProfileVisibility::from_db(
                &self.profile.vis_communities,
            ),
            vis_friends: crate::profile_visibility::ProfileVisibility::from_db(
                &self.profile.vis_friends,
            ),
            caller_blocked_owner: self.caller_blocked_owner,
            owner_blocked_caller: self.owner_blocked_caller,
            has_shared_server_context: self.has_shared_server_context,
            is_deleted: self.profile.deleted_at.is_some(),
        }
    }
}

impl From<db::message::MessageRow> for MessageSummary {
    fn from(row: db::message::MessageRow) -> Self {
        // Soft-deleted rows stay in results but never carry their content
        // back out — this is the one place that rule is enforced, so no
        // caller can forget it.
        let content = if row.deleted_at.is_some() {
            None
        } else {
            Some(row.content)
        };

        Self {
            id: row.id,
            channel_id: row.channel_id,
            author_account_id: row.author_account_id,
            content,
            created_at: row.created_at,
            edited_at: row.edited_at,
            deleted_at: row.deleted_at,
            pinned_at: row.pinned_at,
        }
    }
}

/// Projects a canonical `db::friendship::FriendshipRow` into `account_id`'s
/// point of view — `account_id` (the friendship is stored order-agnostic in
/// `account_low`/`account_high`) never appears in the result, only the other
/// party does, mirroring `ChannelSummary`'s "never repeat the caller" shape.
fn friendship_summary_for(
    row: db::friendship::FriendshipRow,
    account_id: Uuid,
) -> FriendshipSummary {
    let other = if row.account_low == account_id {
        row.account_high
    } else {
        row.account_low
    };

    FriendshipSummary {
        id: row.id,
        account_id: other,
        status: row.status,
        requested_by: row.requested_by,
        created_at: row.created_at,
    }
}

impl From<db::block::BlockRow> for BlockSummary {
    fn from(row: db::block::BlockRow) -> Self {
        Self {
            id: row.id,
            account_id: row.blocked_account_id,
            created_at: row.created_at,
        }
    }
}

/// Everything `require_permission`/`check_hierarchy` need about one member:
/// whether they're the owner (bypasses every check below), the OR of every
/// held role's bits (including the implicit default role), and the highest
/// `position` among those roles (M2).
struct MemberContext {
    is_owner: bool,
    permissions: i64,
    top_position: i32,
    /// Every role id this member effectively holds, INCLUDING the implicit
    /// default role — the channel permission model needs the actual set (not just the OR'd bit-
    /// mask) to check `channel_role_permission` grants per role.
    role_ids: Vec<Uuid>,
}

impl DomainService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create_server(
        &self,
        account_id: Uuid,
        input: CreateServerInput,
    ) -> Result<ServerSummary, DomainError> {
        validate_server_name(&input.name)?;

        let visibility = match input.visibility {
            Some(visibility) => {
                validate_visibility(&visibility)?;
                visibility
            }
            // Matches the `server.visibility` column's own DB default —
            // decided here so validation of an omitted value is a no-op
            // rather than skipped silently.
            None => DEFAULT_VISIBILITY.to_string(),
        };

        let invite_code = generate_invite_code();
        let server_id = new_id();
        let membership_id = new_id();
        let default_role_id = new_id();

        let mut tx = self.pool.begin().await?;

        let server = db::server::insert(
            &mut *tx,
            server_id,
            account_id,
            &input.name,
            &visibility,
            &invite_code,
        )
        .await?;

        db::channel::insert_membership(&mut *tx, membership_id, server.id, account_id, "owner")
            .await?;

        // M2: every server gets its implicit "everyone" role at creation —
        // see 0006_server_roles.sql's backfill for servers that predate this.
        db::server_role::insert_default_role(&mut *tx, default_role_id, server.id).await?;

        tx.commit().await?;

        // The creator is always the owner, so the invite code is always
        // visible in this response — no need for the role-join path here.
        Ok(ServerSummary {
            id: server.id,
            name: server.name,
            icon_url: server.icon_url,
            visibility: server.visibility,
            owner_account_id: server.owner_account_id,
            invite_code: Some(server.invite_code),
            created_at: server.created_at,
        })
    }

    pub async fn list_servers(&self, account_id: Uuid) -> Result<Vec<ServerSummary>, DomainError> {
        let rows = db::server::list_for_account(&self.pool, account_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_server(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<ServerSummary, DomainError> {
        let row = db::server::get_for_account(&self.pool, account_id, server_id)
            .await?
            // Covers both "doesn't exist" and "exists but caller isn't a
            // member" — same error, same 404, never distinguished.
            .ok_or(DomainError::ServerNotFound)?;

        // invite_code is visible to the owner OR to anyone holding
        // MANAGE_INVITES/ADMIN — widened from the previous owner-only rule.
        // Scoped to this single-server fetch only (one extra query, cheap);
        // `list_servers` keeps the simpler owner-only rule via `ServerSummary`'s
        // blanket `From` impl below, rather than paying an extra query per row
        // in a bulk listing for a field most list consumers don't need inline.
        let invite_code = if row.role == "owner" {
            Some(row.invite_code.clone())
        } else {
            let ctx = self.member_context(account_id, server_id).await?;
            let can_see_invite = ctx.is_owner
                || permissions::has(ctx.permissions, permissions::ADMIN)
                || permissions::has(ctx.permissions, permissions::MANAGE_INVITES);
            can_see_invite.then(|| row.invite_code.clone())
        };

        Ok(ServerSummary {
            id: row.id,
            name: row.name,
            icon_url: row.icon_url,
            visibility: row.visibility,
            owner_account_id: row.owner_account_id,
            invite_code,
            created_at: row.created_at,
        })
    }

    /// Requires `MANAGE_INVITES` (owner/`ADMIN` already bypass via
    /// `require_permission`). Replaces `server.invite_code` outright — the
    /// old code stops resolving immediately, no grace period.
    pub async fn regenerate_invite_code(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<ServerSummary, DomainError> {
        self.require_permission(account_id, server_id, permissions::MANAGE_INVITES)
            .await?;

        let new_code = generate_invite_code();
        let updated = db::server::update_invite_code(&self.pool, server_id, &new_code)
            .await?
            .ok_or(DomainError::ServerNotFound)?;

        // The caller just proved they may see the invite code by holding
        // MANAGE_INVITES, so it's always included in this response.
        Ok(ServerSummary {
            id: updated.id,
            name: updated.name,
            icon_url: updated.icon_url,
            visibility: updated.visibility,
            owner_account_id: updated.owner_account_id,
            invite_code: Some(updated.invite_code),
            created_at: updated.created_at,
        })
    }

    /// Gated on `MANAGE_CHANNELS` (owner/`ADMIN` bypass via
    /// `require_permission`, same tier as `update_channel_restricted` and
    /// `set_channel_role_permission`) and capped at
    /// `MAX_CHANNELS_PER_SERVER`, same defense-in-depth pattern
    /// `MAX_ROLES_PER_SERVER` already applies to `create_role`.
    pub async fn create_channel(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        input: CreateChannelInput,
    ) -> Result<ChannelSummary, DomainError> {
        self.require_permission(account_id, server_id, permissions::MANAGE_CHANNELS)
            .await?;
        validate_channel_name(&input.name)?;
        let kind = validate_channel_kind(input.kind.as_deref())?;

        let count = db::channel::count_by_server(&self.pool, server_id).await?;
        if count as usize >= MAX_CHANNELS_PER_SERVER {
            return Err(DomainError::ChannelLimitReached);
        }

        let channel =
            db::channel::insert_server_channel(&self.pool, new_id(), server_id, kind, &input.name)
                .await?;

        Ok(channel.into())
    }

    /// Every member of `server_id`, with the role each holds. Gated on the
    /// caller's own membership: a non-member gets `ServerNotFound`, the same
    /// non-leaking 404 the rest of the server surface uses, so this never
    /// confirms a server exists to someone who can't see it.
    ///
    /// Scoped to shared-server visibility on purpose — this is the
    /// account-discovery path for starting a DM or friend request with
    /// someone you already share a server with, NOT a global user directory
    /// ("privacy as a feature" is a deliberate product pillar).
    pub async fn list_members(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<Vec<ServerMemberSummary>, DomainError> {
        self.require_membership(account_id, server_id).await?;

        let rows = db::server::list_members(&self.pool, server_id).await?;
        // One extra bulk query rather than one per row (M2) — see
        // `role_ids_for_server_members`'s own doc comment.
        let role_pairs =
            db::server_role::role_ids_for_server_members(&self.pool, server_id).await?;

        let mut members: Vec<ServerMemberSummary> = rows.into_iter().map(Into::into).collect();
        for member in &mut members {
            member.role_ids = role_pairs
                .iter()
                .filter(|(account, _)| *account == member.account_id)
                .map(|(_, role_id)| *role_id)
                .collect();
        }

        Ok(members)
    }

    /// A `restricted` channel/thread is simply absent from this
    /// list for a caller without a qualifying grant — never returned then
    /// hidden client-side.
    pub async fn list_channels(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<Vec<ChannelSummary>, DomainError> {
        self.require_membership(account_id, server_id).await?;

        let rows = db::channel::list_by_server(&self.pool, server_id).await?;

        let ctx = self.member_context(account_id, server_id).await?;
        let bypass = ctx.is_owner || permissions::has(ctx.permissions, permissions::ADMIN);
        if bypass {
            return Ok(rows.into_iter().map(Into::into).collect());
        }

        // A thread row's OWN `restricted` is always false — its
        // parent's raw value is what matters, so batch-resolve every
        // distinct parent id referenced among these rows up front rather
        // than one query per thread.
        let parent_ids: Vec<Uuid> = {
            let mut ids: Vec<Uuid> = rows.iter().filter_map(|r| r.parent_channel_id).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        let parent_restricted: std::collections::HashMap<Uuid, bool> = if parent_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            db::channel::restricted_flags_for(&self.pool, &parent_ids)
                .await?
                .into_iter()
                .collect()
        };

        // Resolve every row's (effective_restricted, grant_channel_id) up
        // front, then batch-fetch the grant check for every distinct
        // restricted grant target in one query — same split
        // `restricted_flags_for` above already applies to the restriction
        // flag itself, now extended to the grant check that used to run one
        // query per restricted channel in the loop below.
        let resolved: Vec<(db::channel::ChannelRow, bool, Uuid)> = rows
            .into_iter()
            .map(|row| {
                let (effective_restricted, grant_channel_id) = match row.parent_channel_id {
                    Some(parent_id) => (
                        *parent_restricted.get(&parent_id).unwrap_or(&false),
                        parent_id,
                    ),
                    None => (row.restricted, row.id),
                };
                (row, effective_restricted, grant_channel_id)
            })
            .collect();

        let restricted_grant_ids: Vec<Uuid> = {
            let mut ids: Vec<Uuid> = resolved
                .iter()
                .filter(|(_, restricted, _)| *restricted)
                .map(|(_, _, grant_id)| *grant_id)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        let granted: std::collections::HashSet<Uuid> =
            db::channel::channel_ids_with_role_permission(
                &self.pool,
                &restricted_grant_ids,
                &ctx.role_ids,
                channel_permissions::VIEW_CHANNEL,
            )
            .await?
            .into_iter()
            .collect();

        let visible = resolved
            .into_iter()
            .filter(|(_, effective_restricted, grant_channel_id)| {
                !effective_restricted || granted.contains(grant_channel_id)
            })
            .map(|(row, _, _)| row.into())
            .collect();

        Ok(visible)
    }

    /// Creates a thread under `parent_channel_id` — a `channel`
    /// row with `kind = 'thread'`. Authorized exactly like posting a message
    /// there: any account with access to the parent may start one, no new
    /// permission bit. `root_message_id` is `None` for a standalone thread
    /// (a new top-level topic) or `Some` for a reply-thread spawned from an
    /// existing message, which must belong to the same parent channel and
    /// not be soft-deleted.
    pub async fn create_thread(
        &self,
        account_id: Uuid,
        parent_channel_id: Uuid,
        input: CreateThreadInput,
    ) -> Result<ChannelSummary, DomainError> {
        let parent = self
            .require_channel_access(account_id, parent_channel_id)
            .await?;

        if parent.kind == "thread" {
            return Err(DomainError::Validation(
                "a thread cannot be created inside another thread".to_string(),
            ));
        }
        if !is_server_channel(&parent.kind) {
            return Err(DomainError::Validation(
                "threads can only be created in a server channel".to_string(),
            ));
        }
        let server_id = parent.server_id.ok_or(DomainError::ChannelNotFound)?;
        self.require_not_timed_out(account_id, server_id).await?;

        validate_thread_title(&input.title)?;

        if let Some(root_message_id) = input.root_message_id {
            let root = db::message::find_owner(&self.pool, root_message_id, parent_channel_id)
                .await?
                .ok_or(DomainError::MessageNotFound)?;
            if root.deleted_at.is_some() {
                return Err(DomainError::MessageNotFound);
            }
        }

        let id = new_id();
        // A short id-derived suffix makes the slug unique per parent by
        // construction (a fresh UUIDv7 colliding is not a case worth coding
        // a retry loop for) — see `slugify`'s own doc comment.
        let suffix: String = id.as_simple().to_string().chars().take(8).collect();
        let slug = format!("{}-{suffix}", slugify(&input.title));

        let thread = db::channel::insert_thread(
            &self.pool,
            id,
            server_id,
            parent_channel_id,
            input.root_message_id,
            &input.title,
            &slug,
        )
        .await?;

        Ok(thread.into())
    }

    /// Every thread under `parent_channel_id`, newest first.
    /// Authorization mirrors `list_channels`: access to the parent is
    /// required, nothing thread-specific.
    pub async fn list_threads(
        &self,
        account_id: Uuid,
        parent_channel_id: Uuid,
    ) -> Result<Vec<ChannelSummary>, DomainError> {
        self.require_channel_access(account_id, parent_channel_id)
            .await?;

        let rows = db::channel::list_threads_by_parent(&self.pool, parent_channel_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Sets (or clears, via `visibility: None`) a channel's visibility
    /// override. Gated on `MANAGE_VISIBILITY`; an override may
    /// never rank BROADER than the server's own visibility — a private
    /// server cannot have a public channel, because there is no public entry
    /// point into a private server for that channel to be reached from.
    pub async fn update_channel_visibility(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        channel_id: Uuid,
        visibility: Option<String>,
    ) -> Result<ChannelSummary, DomainError> {
        self.require_permission(account_id, server_id, permissions::MANAGE_VISIBILITY)
            .await?;

        if let Some(value) = &visibility {
            validate_visibility(value)?;
        }

        let server = db::server::get_for_account(&self.pool, account_id, server_id)
            .await?
            .ok_or(DomainError::ServerNotFound)?;

        if let Some(value) = &visibility {
            if visibility_rank(value) > visibility_rank(&server.visibility) {
                return Err(DomainError::Validation(
                    "channel visibility cannot be broader than its server's visibility".to_string(),
                ));
            }
        }

        let updated = db::channel::update_visibility(
            &self.pool,
            channel_id,
            server_id,
            visibility.as_deref(),
        )
        .await?
        .ok_or(DomainError::ChannelNotFound)?;

        Ok(updated.into())
    }

    /// Sets/clears whether a channel is visibility-restricted at
    /// all — gated on `MANAGE_CHANNELS` (the bit M2 already reserved for
    /// channel administration, not a new one). Rejects a `thread` id: a
    /// thread never carries its own restriction, it inherits its parent's
    /// (`ChannelAccessRow`'s own doc comment), so flipping it here would be a
    /// no-op that silently does nothing — better to say so than pretend it
    /// worked.
    pub async fn update_channel_restricted(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        channel_id: Uuid,
        restricted: bool,
    ) -> Result<ChannelSummary, DomainError> {
        self.require_permission(account_id, server_id, permissions::MANAGE_CHANNELS)
            .await?;

        let channel = self.lookup_channel(channel_id).await?;
        if channel.kind == "thread" {
            return Err(DomainError::Validation(
                "a thread inherits its parent channel's restriction and cannot be restricted on its own"
                    .to_string(),
            ));
        }

        let updated =
            db::channel::set_channel_restricted(&self.pool, channel_id, server_id, restricted)
                .await?
                .ok_or(DomainError::ChannelNotFound)?;

        Ok(updated.into())
    }

    /// Full replace of one role's grant on one channel — gated on
    /// `MANAGE_CHANNELS`, same tier as `update_channel_restricted` and the
    /// same bit Nerimity's own channel-permission endpoint uses. `bits` is
    /// masked to `VIEW_CHANNEL` so a stray or unenforced bit can never be
    /// persisted.
    /// Rejects a `thread` id for the same reason `update_channel_restricted`
    /// does.
    pub async fn set_channel_role_permission(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        channel_id: Uuid,
        role_id: Uuid,
        bits: i64,
    ) -> Result<(), DomainError> {
        self.require_permission(account_id, server_id, permissions::MANAGE_CHANNELS)
            .await?;

        let channel = self.lookup_channel(channel_id).await?;
        if channel.kind == "thread" {
            return Err(DomainError::Validation(
                "a thread has no channel_role_permission grants of its own — set them on its parent channel"
                    .to_string(),
            ));
        }
        if channel.server_id != Some(server_id) {
            return Err(DomainError::ChannelNotFound);
        }

        db::server_role::find_role(&self.pool, server_id, role_id)
            .await?
            .ok_or(DomainError::RoleNotFound)?;

        db::channel::upsert_channel_role_permission(
            &self.pool,
            channel_id,
            role_id,
            bits & channel_permissions::VIEW_CHANNEL,
        )
        .await?;

        Ok(())
    }

    /// Every explicit grant on one channel — the settings UI's
    /// read path. Self-contained: goes through `require_channel_access` (not
    /// just plain membership), so a member who cannot see a restricted
    /// channel cannot list its grants either, same rule as every other read
    /// on it.
    pub async fn list_channel_role_permissions(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
    ) -> Result<Vec<(Uuid, i64)>, DomainError> {
        self.require_channel_access(account_id, channel_id).await?;

        let rows = db::channel::channel_role_permissions_for(&self.pool, channel_id).await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.role_id, r.permissions))
            .collect())
    }

    /// Sets `server.visibility` outright — owner-only, checked
    /// directly rather than via `require_permission`, the same all-or-nothing
    /// tier `delete_server` uses: this is a whole-community decision, not a
    /// per-channel one, so no permission bit gates it.
    pub async fn update_server_visibility(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        visibility: String,
    ) -> Result<ServerSummary, DomainError> {
        validate_visibility(&visibility)?;

        let server = db::server::get_for_account(&self.pool, account_id, server_id)
            .await?
            .ok_or(DomainError::ServerNotFound)?;

        if server.owner_account_id != account_id {
            return Err(DomainError::MissingPermission);
        }

        let updated = db::server::update_visibility(&self.pool, server_id, &visibility)
            .await?
            .ok_or(DomainError::ServerNotFound)?;

        // The caller is always the owner here, so the invite code is always
        // visible in this response — same reasoning `create_server` uses.
        Ok(ServerSummary {
            id: updated.id,
            name: updated.name,
            icon_url: updated.icon_url,
            visibility: updated.visibility,
            owner_account_id: updated.owner_account_id,
            invite_code: Some(updated.invite_code),
            created_at: updated.created_at,
        })
    }

    /// Resolves who may read a channel/thread's content: an
    /// authenticated member always may; anyone else (including an
    /// unauthenticated `actor: None`, the case the public read path
    /// needs) may only if the channel's EFFECTIVE visibility — its own
    /// override, or its server's visibility if the override is unset — is
    /// `public` or `unlisted`. `dm`/`group_dm` channels sit outside this axis
    /// entirely (no `server_id` to resolve a visibility from) and always
    /// resolve to `ChannelNotFound` for a non-member, same as today.
    ///
    /// A `restricted` channel is checked FIRST and short-circuits
    /// everything below — an anonymous caller (`actor: None`) holds no role
    /// and can never satisfy a grant, so a restricted channel is NEVER
    /// publicly readable regardless of its own `visibility`; an authenticated
    /// caller still needs a real grant (or owner/`ADMIN`), not just
    /// membership. This is the one place a restricted channel inside an
    /// otherwise-public server could leak into the public read path or
    /// search if it were missed — both call this function.
    pub async fn resolve_read_access(
        &self,
        actor: Option<Uuid>,
        channel_id: Uuid,
    ) -> Result<ReadAccess, DomainError> {
        let ctx = db::channel::visibility_context(&self.pool, channel_id)
            .await?
            .ok_or(DomainError::ChannelNotFound)?;

        let server_id = ctx.server_id.ok_or(DomainError::ChannelNotFound)?;
        let server_visibility = ctx.server_visibility.ok_or(DomainError::ChannelNotFound)?;
        let effective = ctx.channel_visibility.unwrap_or(server_visibility);

        if ctx.restricted {
            let Some(account_id) = actor else {
                return Err(DomainError::ChannelNotFound);
            };
            let grant_channel_id = ctx.parent_channel_id.unwrap_or(channel_id);
            return if self
                .member_can_view_channel(account_id, server_id, grant_channel_id)
                .await?
            {
                Ok(ReadAccess::Member)
            } else {
                Err(DomainError::ChannelNotFound)
            };
        }

        if let Some(account_id) = actor {
            if is_server_channel(&ctx.kind)
                && db::channel::membership_exists(&self.pool, server_id, account_id).await?
            {
                return Ok(ReadAccess::Member);
            }
        }

        match effective.as_str() {
            "public" | "unlisted" => Ok(ReadAccess::Public),
            // Includes an unrecognized value, which should be unreachable
            // given the column's CHECK constraint and `validate_visibility`
            // gating every write — treated as the narrowest case rather than
            // panicking on a row that should never exist.
            _ => Err(DomainError::ChannelNotFound),
        }
    }

    /// The public read path's thread view: resolves visibility
    /// via `resolve_read_access` with `actor: None` (unauthenticated), then
    /// returns the thread itself plus an oldest-first page of its messages
    /// — the order an anonymous reader actually reads top to bottom in,
    /// unlike the SPA's newest-first chat view. Errors with
    /// `ChannelNotFound` for anything not resolvable as a publicly-readable
    /// thread — a private/unlisted thread, a non-thread id, or a genuinely
    /// nonexistent one all collapse to the same outcome, so an anonymous
    /// caller can never distinguish "private" from "doesn't exist".
    pub async fn get_public_thread(
        &self,
        thread_id: Uuid,
        after: Option<Uuid>,
    ) -> Result<(ChannelSummary, Vec<PublicMessageSummary>), DomainError> {
        self.resolve_read_access(None, thread_id).await?;

        let thread = db::channel::find_thread(&self.pool, thread_id)
            .await?
            .ok_or(DomainError::ChannelNotFound)?;

        let rows = db::message::list_page_ascending_with_author(
            &self.pool,
            thread_id,
            after,
            DEFAULT_MESSAGE_LIMIT as i64,
        )
        .await?;

        let messages = rows
            .into_iter()
            .map(|row| PublicMessageSummary {
                id: row.id,
                content: row.content,
                created_at: row.created_at,
                author_display_name: row.author_display_name,
            })
            .collect();

        Ok((thread.into(), messages))
    }

    /// Every publicly-visible thread, newest first — the sitemap's source of
    /// truth. No caller/authorization to check: this only ever
    /// returns what is already public, the same set an anonymous
    /// `get_public_thread` call on any of these ids would already succeed
    /// against.
    pub async fn list_public_threads(&self) -> Result<Vec<SitemapThread>, DomainError> {
        let rows = db::channel::list_public_threads(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|row| SitemapThread {
                id: row.id,
                slug: row.slug,
                created_at: row.created_at,
            })
            .collect())
    }

    /// Gets or creates the 1:1 `dm` channel between `account_id` and
    /// `other_account_id` (ROADMAP slice 6). Returns `(channel, true)` when a
    /// new channel was created, `(channel, false)` when an existing one was
    /// found — the caller (`api::handlers::create_dm`) maps that to 201 vs
    /// 200.
    pub async fn create_dm(
        &self,
        account_id: Uuid,
        other_account_id: Uuid,
    ) -> Result<(ChannelSummary, bool), DomainError> {
        if account_id == other_account_id {
            return Err(DomainError::Validation(
                "cannot start a dm with yourself".to_string(),
            ));
        }

        let other_exists = db::account::exists(&self.pool, other_account_id).await?;
        if !other_exists {
            return Err(DomainError::AccountNotFound);
        }

        // A block "prevents DMs ... between them" — checked before the
        // lock/existing-channel lookup below so a blocked pair can never
        // get as far as creating (or resurrecting) a shared dm.
        if self
            .blocked_either_direction(account_id, other_account_id)
            .await?
        {
            return Err(DomainError::Blocked);
        }

        let mut tx = self.pool.begin().await?;

        // Serializes concurrent "start a dm with X" calls for the same
        // unordered pair so two racing requests can never both pass the
        // "does it exist yet" check below and create two channels for the
        // same pair — same TOCTOU concern `auth::AuthService::login`'s
        // per-account lock addresses, just keyed on the pair via a Postgres
        // advisory lock since there's no row to `SELECT ... FOR UPDATE`
        // before the channel exists yet.
        let (low, high) = if account_id < other_account_id {
            (account_id, other_account_id)
        } else {
            (other_account_id, account_id)
        };
        db::lock::advisory_xact_lock(&mut *tx, &format!("dm:{low}:{high}")).await?;

        let existing = db::dm::find_existing_dm(&mut *tx, account_id, other_account_id).await?;

        if let Some(channel) = existing {
            tx.commit().await?;
            return Ok((channel.into(), false));
        }

        let channel_id = new_id();
        let channel = db::dm::insert_dm_channel(&mut *tx, channel_id).await?;

        db::dm::insert_two_channel_members(
            &mut *tx,
            new_id(),
            channel_id,
            account_id,
            new_id(),
            other_account_id,
        )
        .await?;

        tx.commit().await?;

        Ok((channel.into(), true))
    }

    /// Creates a new `group_dm` channel with `account_id` (the caller) plus
    /// every id in `input.account_ids` (ROADMAP slice 6). Unlike
    /// `create_dm`, this is NOT idempotent — every call makes a new group,
    /// matching ordinary "create group" semantics elsewhere (there's no
    /// natural key to deduplicate an arbitrary N-person group on).
    pub async fn create_group_dm(
        &self,
        account_id: Uuid,
        input: CreateGroupDmInput,
    ) -> Result<ChannelSummary, DomainError> {
        let participants = validate_group_dm_participants(&input.account_ids, account_id)?;

        let existing_count = db::account::existing_count(&self.pool, &participants).await?;
        if existing_count as usize != participants.len() {
            return Err(DomainError::AccountNotFound);
        }

        for participant_id in &participants {
            if self
                .blocked_either_direction(account_id, *participant_id)
                .await?
            {
                return Err(DomainError::Blocked);
            }
        }

        let mut tx = self.pool.begin().await?;

        let channel_id = new_id();
        let channel = db::dm::insert_group_dm_channel(&mut *tx, channel_id).await?;

        // Creator + every validated participant, one row each — `input`'s
        // ids are already deduped and creator-filtered by
        // `validate_group_dm_participants`.
        for member_id in std::iter::once(account_id).chain(participants) {
            db::dm::insert_channel_member(&mut *tx, new_id(), channel_id, member_id).await?;
        }

        tx.commit().await?;

        Ok(channel.into())
    }

    /// Every dm/group_dm channel `account_id` is a member of (ROADMAP slice
    /// 6) — the DM-list equivalent of `list_servers`/`list_channels`.
    pub async fn list_dms(&self, account_id: Uuid) -> Result<Vec<ChannelSummary>, DomainError> {
        let rows = db::dm::list_for_account(&self.pool, account_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Sends a friend request to `target_account_id`, or accepts it if
    /// `target_account_id` already sent one to `account_id` (ROADMAP slice
    /// 7). Returns `(friendship, true)` when this call changed something
    /// (a new pending row, or a pending -> accepted transition), `(.., false)`
    /// when it was a no-op (already pending in the same direction, or
    /// already accepted) — same created/no-op split as `create_dm`, mapped
    /// by the caller (`api::handlers::send_friend_request`) to 201 vs 200.
    pub async fn send_friend_request(
        &self,
        account_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<(FriendshipSummary, bool), DomainError> {
        if account_id == target_account_id {
            return Err(DomainError::Validation(
                "cannot send a friend request to yourself".to_string(),
            ));
        }

        let target_exists = db::account::exists(&self.pool, target_account_id).await?;
        if !target_exists {
            return Err(DomainError::AccountNotFound);
        }

        if self
            .blocked_either_direction(account_id, target_account_id)
            .await?
        {
            return Err(DomainError::Blocked);
        }

        let (low, high) = if account_id < target_account_id {
            (account_id, target_account_id)
        } else {
            (target_account_id, account_id)
        };

        let mut tx = self.pool.begin().await?;

        // Same TOCTOU concern as `create_dm`'s pair lock: without this,
        // simultaneous requests from both sides could each miss the other's
        // insert and either duplicate the row or hit a unique-constraint
        // error instead of cleanly accepting.
        db::lock::advisory_xact_lock(&mut *tx, &format!("friendship:{low}:{high}")).await?;

        let existing = db::friendship::find_by_pair(&mut *tx, low, high).await?;

        let (row, changed) = match existing {
            None => {
                let row = db::friendship::insert_pending(&mut *tx, new_id(), low, high, account_id)
                    .await?;
                (row, true)
            }
            Some(row) if row.status == "pending" && row.requested_by != account_id => {
                let row = db::friendship::accept(&mut *tx, row.id).await?;
                (row, true)
            }
            // Already pending in the same direction, or already accepted —
            // nothing to do.
            Some(row) => (row, false),
        };

        tx.commit().await?;

        Ok((friendship_summary_for(row, account_id), changed))
    }

    /// Every `friendship` row `account_id` is a party to, pending or
    /// accepted (ROADMAP slice 7).
    pub async fn list_friendships(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<FriendshipSummary>, DomainError> {
        let rows = db::friendship::list_for_account(&self.pool, account_id).await?;

        Ok(rows
            .into_iter()
            .map(|row| friendship_summary_for(row, account_id))
            .collect())
    }

    /// Removes the `friendship` row between `account_id` and
    /// `target_account_id`, whatever its status — declining/cancelling a
    /// pending request and unfriending an accepted one are the same
    /// operation on this schema's single canonical row.
    pub async fn remove_friendship(
        &self,
        account_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<(), DomainError> {
        let (low, high) = if account_id < target_account_id {
            (account_id, target_account_id)
        } else {
            (target_account_id, account_id)
        };

        let deleted = db::friendship::delete_pair(&self.pool, low, high).await?;

        if deleted == 0 {
            return Err(DomainError::FriendRequestNotFound);
        }

        Ok(())
    }

    /// Blocks `target_account_id` (ROADMAP slice 7). Idempotent — blocking
    /// someone already blocked just returns the existing row with
    /// `created = false`, same created/no-op split as `create_dm`. Also
    /// removes any `friendship` row between the two: a block "prevents ...
    /// friend requests between them", and leaving a
    /// stale `accepted` friendship active after a block would contradict
    /// that — this closes the gap rather than silently leaving the old
    /// friendship in place.
    pub async fn block_account(
        &self,
        account_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<(BlockSummary, bool), DomainError> {
        if account_id == target_account_id {
            return Err(DomainError::Validation("cannot block yourself".to_string()));
        }

        let target_exists = db::account::exists(&self.pool, target_account_id).await?;
        if !target_exists {
            return Err(DomainError::AccountNotFound);
        }

        let mut tx = self.pool.begin().await?;

        // No advisory lock needed here (unlike create_dm/send_friend_request):
        // `block` has no reciprocal row the other party could race to
        // create — the only possible concurrent writer of THIS row is
        // `account_id` calling twice, which `ON CONFLICT DO NOTHING` already
        // makes atomic.
        let inserted =
            db::block::insert_or_conflict(&mut *tx, new_id(), account_id, target_account_id)
                .await?;

        let (row, created) = match inserted {
            Some(row) => (row, true),
            None => {
                let row = db::block::find_by_pair(&mut *tx, account_id, target_account_id).await?;
                (row, false)
            }
        };

        let (low, high) = if account_id < target_account_id {
            (account_id, target_account_id)
        } else {
            (target_account_id, account_id)
        };
        db::friendship::delete_pair(&mut *tx, low, high).await?;

        tx.commit().await?;

        Ok((row.into(), created))
    }

    /// Every `block` row `account_id` has placed (ROADMAP slice 7). Never
    /// blocks placed *against* `account_id` — a directional design means a
    /// block is only ever visible to the account that placed it.
    pub async fn list_blocks(&self, account_id: Uuid) -> Result<Vec<BlockSummary>, DomainError> {
        let rows = db::block::list_for_account(&self.pool, account_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Removes the block `account_id` placed on `target_account_id`.
    pub async fn unblock_account(
        &self,
        account_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<(), DomainError> {
        let deleted = db::block::delete(&self.pool, account_id, target_account_id).await?;

        if deleted == 0 {
            return Err(DomainError::BlockNotFound);
        }

        Ok(())
    }

    /// Shared block gate for `create_dm`, `send_friend_request`, and
    /// `send_message` (dm channels) — a block "prevents DMs and friend
    /// requests between them", checked symmetrically since
    /// either party may have placed it.
    async fn blocked_either_direction(&self, a: Uuid, b: Uuid) -> Result<bool, DomainError> {
        Ok(db::block::exists_either_direction(&self.pool, a, b).await?)
    }

    pub async fn join_via_invite(
        &self,
        account_id: Uuid,
        invite_code: &str,
    ) -> Result<ServerSummary, DomainError> {
        let server = db::server::find_by_invite_code(&self.pool, invite_code)
            .await?
            .ok_or(DomainError::InvalidInvite)?;

        // Explicit check is the primary path (matches AlreadyMember to a
        // clean 409 rather than an opaque constraint error); the unique
        // violation mapped below on the insert is a defensive fallback for
        // the small residual race between this check and the insert, fine
        // at M0 concurrency scale (mirrors `auth::AuthService::login`'s
        // concurrency-cap handling).
        let already_member =
            db::channel::membership_exists(&self.pool, server.id, account_id).await?;

        if already_member {
            return Err(DomainError::AlreadyMember);
        }

        // M2: a banned account cannot rejoin via any invite to this server,
        // new code or old — checked before the insert, same spot
        // `AlreadyMember` is checked.
        if db::server_role::is_banned(&self.pool, server.id, account_id).await? {
            return Err(DomainError::Banned);
        }

        db::channel::insert_membership(&self.pool, new_id(), server.id, account_id, "member")
            .await
            .map_err(map_membership_conflict)?;

        // A joining member is never the owner, so the invite code is never
        // visible in this response.
        Ok(ServerSummary {
            id: server.id,
            name: server.name,
            icon_url: server.icon_url,
            visibility: server.visibility,
            owner_account_id: server.owner_account_id,
            invite_code: None,
            created_at: server.created_at,
        })
    }

    // ---- M2: roles & permissions ----

    async fn member_context(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<MemberContext, DomainError> {
        let membership = db::channel::find_membership(&self.pool, server_id, account_id)
            .await?
            // Same non-leaking 404 as `require_membership` — a non-member
            // must never learn a server exists via this path either.
            .ok_or(DomainError::ServerNotFound)?;

        let roles =
            db::server_role::roles_for_membership(&self.pool, server_id, membership.id).await?;

        Ok(MemberContext {
            is_owner: membership.role == "owner",
            permissions: roles.iter().fold(0i64, |acc, r| acc | r.permissions),
            top_position: roles.iter().map(|r| r.position).max().unwrap_or(0),
            role_ids: roles.iter().map(|r| r.id).collect(),
        })
    }

    /// The owner bypasses every bit; an `ADMIN` role bypasses the specific
    /// bit check but NOT the hierarchy check callers layer on top via
    /// `check_hierarchy` — a permission bit and the hierarchy are
    /// independent axes.
    async fn require_permission(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        bit: i64,
    ) -> Result<MemberContext, DomainError> {
        let ctx = self.member_context(account_id, server_id).await?;

        if ctx.is_owner || permissions::has(ctx.permissions, permissions::ADMIN) {
            return Ok(ctx);
        }

        if !permissions::has(ctx.permissions, bit) {
            return Err(DomainError::MissingPermission);
        }

        Ok(ctx)
    }

    fn check_hierarchy(ctx: &MemberContext, target_position: i32) -> Result<(), DomainError> {
        if ctx.is_owner {
            return Ok(());
        }
        if target_position >= ctx.top_position {
            return Err(DomainError::InsufficientHierarchy);
        }
        Ok(())
    }

    /// Masks a client-supplied permission bitmask down to bits this build
    /// actually knows about — an old/foreign bit can never be persisted and
    /// later mean something nobody intended once it's assigned meaning.
    fn known_permission_bits(raw: i64) -> i64 {
        raw & permissions::KNOWN_PERMISSION_BITS
    }

    pub async fn create_role(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        input: CreateRoleInput,
    ) -> Result<RoleSummary, DomainError> {
        self.require_permission(account_id, server_id, permissions::MANAGE_ROLES)
            .await?;
        validate_role_name(&input.name)?;

        let count = db::server_role::count_roles(&self.pool, server_id).await?;
        if count as usize >= MAX_ROLES_PER_SERVER {
            return Err(DomainError::RoleLimitReached);
        }

        // New roles start above every existing one (Nerimity's own
        // convention) — the owner reorders it down from there.
        let position = db::server_role::max_role_position(&self.pool, server_id).await? + 1;

        let role =
            db::server_role::insert_role(&self.pool, new_id(), server_id, &input.name, position)
                .await?;

        Ok(role.into())
    }

    pub async fn list_roles(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<Vec<RoleSummary>, DomainError> {
        self.require_membership(account_id, server_id).await?;
        let rows = db::server_role::list_roles(&self.pool, server_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn update_role(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        role_id: Uuid,
        input: UpdateRoleInput,
    ) -> Result<RoleSummary, DomainError> {
        let ctx = self
            .require_permission(account_id, server_id, permissions::MANAGE_ROLES)
            .await?;

        let existing = db::server_role::find_role(&self.pool, server_id, role_id)
            .await?
            .ok_or(DomainError::RoleNotFound)?;

        Self::check_hierarchy(&ctx, existing.position)?;

        let name = match input.name {
            Some(name) => {
                validate_role_name(&name)?;
                name
            }
            None => existing.name,
        };
        let color = match input.color {
            Some(color) => {
                validate_role_color(&color)?;
                Some(color)
            }
            None => existing.color,
        };
        let raw_permissions = input.permissions.unwrap_or(existing.permissions);
        // A caller can only ever GRANT bits they themselves hold (or are the
        // owner) — otherwise a MANAGE_ROLES-only moderator could hand a role
        // the ADMIN or BAN bit they don't have themselves, an escalation the
        // permission check alone wouldn't catch (MANAGE_ROLES says they may
        // edit A role, not that they may grant every possible bit).
        let permissions_value = if ctx.is_owner {
            Self::known_permission_bits(raw_permissions)
        } else {
            Self::known_permission_bits(raw_permissions) & ctx.permissions
        };
        let mentionable = input.mentionable.unwrap_or(existing.mentionable);

        let updated = db::server_role::update_role(
            &self.pool,
            server_id,
            role_id,
            &name,
            color.as_deref(),
            permissions_value,
            mentionable,
        )
        .await?
        .ok_or(DomainError::RoleNotFound)?;

        Ok(updated.into())
    }

    pub async fn delete_role(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        role_id: Uuid,
    ) -> Result<(), DomainError> {
        let ctx = self
            .require_permission(account_id, server_id, permissions::MANAGE_ROLES)
            .await?;

        let existing = db::server_role::find_role(&self.pool, server_id, role_id)
            .await?
            .ok_or(DomainError::RoleNotFound)?;

        if existing.is_default {
            return Err(DomainError::CannotModifyDefaultRole);
        }

        Self::check_hierarchy(&ctx, existing.position)?;

        let deleted = db::server_role::delete_role(&self.pool, server_id, role_id).await?;
        if !deleted {
            return Err(DomainError::RoleNotFound);
        }

        Ok(())
    }

    /// `ordered_role_ids` is top-to-bottom as the caller displays it (highest
    /// authority first) — the FULL set of non-default roles, not a partial
    /// reorder, so this can validate it's exactly the server's current role
    /// set before touching anything.
    pub async fn reorder_roles(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        ordered_role_ids: Vec<Uuid>,
    ) -> Result<Vec<RoleSummary>, DomainError> {
        let ctx = self
            .require_permission(account_id, server_id, permissions::MANAGE_ROLES)
            .await?;

        let existing = db::server_role::list_roles(&self.pool, server_id).await?;
        let non_default_ids: HashSet<Uuid> = existing
            .iter()
            .filter(|r| !r.is_default)
            .map(|r| r.id)
            .collect();
        let provided_ids: HashSet<Uuid> = ordered_role_ids.iter().copied().collect();

        if provided_ids.len() != ordered_role_ids.len() || provided_ids != non_default_ids {
            return Err(DomainError::Validation(
                "must list every non-default role exactly once".to_string(),
            ));
        }

        let top = ordered_role_ids.len() as i32;
        let old_positions: HashMap<Uuid, i32> =
            existing.iter().map(|r| (r.id, r.position)).collect();
        // Every role touched by the reorder is hierarchy-checked against
        // both its old AND new position — otherwise a MANAGE_ROLES-only
        // moderator could reorder their own role above a role they could
        // never directly edit/delete, and inherit that role's authority.
        for (index, role_id) in ordered_role_ids.iter().enumerate() {
            let new_position = top - index as i32;
            Self::check_hierarchy(&ctx, old_positions[role_id])?;
            Self::check_hierarchy(&ctx, new_position)?;
        }

        let mut tx = self.pool.begin().await?;
        for (index, role_id) in ordered_role_ids.iter().enumerate() {
            // Highest position = first in the list; the default role stays
            // fixed at 0 below everything, so positions start at 1.
            let position = top - index as i32;
            db::server_role::set_role_position(&mut *tx, server_id, *role_id, position).await?;
        }
        tx.commit().await?;

        let rows = db::server_role::list_roles(&self.pool, server_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Replaces one member's full explicit role set. `role_ids` is the
    /// complete new list, not a diff — every added AND removed role is
    /// hierarchy-checked against the caller's own top role.
    pub async fn set_member_roles(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        target_account_id: Uuid,
        role_ids: Vec<Uuid>,
    ) -> Result<Vec<Uuid>, DomainError> {
        let ctx = self
            .require_permission(account_id, server_id, permissions::MANAGE_ROLES)
            .await?;

        let target_membership =
            db::channel::find_membership(&self.pool, server_id, target_account_id)
                .await?
                .ok_or(DomainError::AccountNotFound)?;

        let current_role_ids: HashSet<Uuid> =
            db::server_role::role_ids_for_membership(&self.pool, target_membership.id)
                .await?
                .into_iter()
                .collect();
        let next_role_ids: HashSet<Uuid> = role_ids.iter().copied().collect();

        // Batch-fetch every role either side of the diff could reference —
        // the touched (symmetric-difference) set plus the full next list —
        // in one query instead of one per id, keyed by id for the two
        // lookups below.
        let union_ids: Vec<Uuid> = current_role_ids.union(&next_role_ids).copied().collect();
        let roles_by_id: std::collections::HashMap<Uuid, db::server_role::ServerRoleRow> =
            db::server_role::find_roles(&self.pool, server_id, &union_ids)
                .await?
                .into_iter()
                .map(|role| (role.id, role))
                .collect();

        // Every id in the new list must genuinely belong to this server —
        // otherwise a foreign role id would silently vanish from
        // `current_role_ids`'s perspective while still failing the
        // `membership_role` insert's FK, surfacing as an opaque 500 instead
        // of a clean 404.
        for role_id in &next_role_ids {
            if !roles_by_id.contains_key(role_id) {
                return Err(DomainError::RoleNotFound);
            }
        }

        // Every role that appears on exactly one side (added or removed)
        // needs its own hierarchy check — a diff-of-one that happens to net
        // out even is still an attempt to touch that specific role. A
        // current-side id not in `roles_by_id` would mean a role vanished
        // out from under an existing assignment — same `RoleNotFound` shape
        // `find_role` gave per-id before this was batched.
        let mut touched = Vec::new();
        for role_id in current_role_ids.symmetric_difference(&next_role_ids) {
            let role = roles_by_id
                .get(role_id)
                .cloned()
                .ok_or(DomainError::RoleNotFound)?;
            if role.is_default {
                return Err(DomainError::CannotModifyDefaultRole);
            }
            touched.push(role);
        }
        for role in &touched {
            Self::check_hierarchy(&ctx, role.position)?;
        }

        let mut tx = self.pool.begin().await?;
        db::server_role::clear_membership_roles(&mut *tx, target_membership.id).await?;
        for role_id in &next_role_ids {
            db::server_role::insert_membership_role(&mut *tx, target_membership.id, *role_id)
                .await?;
        }
        tx.commit().await?;

        Ok(role_ids)
    }

    pub async fn list_server_bans(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<Vec<BanSummary>, DomainError> {
        self.require_permission(account_id, server_id, permissions::BAN)
            .await?;
        let rows = db::server_role::list_bans(&self.pool, server_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn unban_member(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<(), DomainError> {
        self.require_permission(account_id, server_id, permissions::BAN)
            .await?;
        let removed = db::server_role::delete_ban(&self.pool, server_id, target_account_id).await?;
        if !removed {
            return Err(DomainError::AccountNotFound);
        }
        Ok(())
    }

    /// Returns the recipient list for the `member.leave` announcement — see
    /// `remove_member`'s doc comment for why this can't be resolved later.
    pub async fn leave_server(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let membership = db::channel::find_membership(&self.pool, server_id, account_id)
            .await?
            .ok_or(DomainError::ServerNotFound)?;

        if membership.role == "owner" {
            return Err(DomainError::OwnerCannotLeave);
        }

        self.remove_member(server_id, account_id).await
    }

    /// Returns the recipient list for the `member.leave` announcement — see
    /// `remove_member`'s doc comment for why this can't be resolved later.
    pub async fn kick_member(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        if account_id == target_account_id {
            return Err(DomainError::CannotActOnSelf);
        }

        let ctx = self
            .require_permission(account_id, server_id, permissions::KICK)
            .await?;

        let target = db::channel::find_membership(&self.pool, server_id, target_account_id)
            .await?
            .ok_or(DomainError::AccountNotFound)?;
        if target.role == "owner" {
            return Err(DomainError::CannotActOnOwner);
        }

        let target_roles =
            db::server_role::roles_for_membership(&self.pool, server_id, target.id).await?;
        let target_top_position = target_roles.iter().map(|r| r.position).max().unwrap_or(0);
        Self::check_hierarchy(&ctx, target_top_position)?;

        self.remove_member(server_id, target_account_id).await
    }

    /// Returns the recipient list for the `member.leave` announcement (see
    /// `remove_member`'s doc comment) alongside the created ban, so the `api`
    /// handler doesn't need a second round trip to build its response.
    pub async fn ban_member(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        target_account_id: Uuid,
        reason: Option<String>,
    ) -> Result<(Vec<Uuid>, BanSummary), DomainError> {
        if account_id == target_account_id {
            return Err(DomainError::CannotActOnSelf);
        }

        let ctx = self
            .require_permission(account_id, server_id, permissions::BAN)
            .await?;

        // Checked BEFORE the membership lookup below: a ban already removes
        // the target's `membership` row (`remove_member`), so a second ban
        // attempt against the same account would otherwise find no
        // membership and report `AccountNotFound` — technically true, but
        // the wrong error for "this account is already banned", which is
        // the more specific and more useful thing to say here.
        if db::server_role::is_banned(&self.pool, server_id, target_account_id).await? {
            return Err(DomainError::AlreadyBanned);
        }

        let target = db::channel::find_membership(&self.pool, server_id, target_account_id)
            .await?
            .ok_or(DomainError::AccountNotFound)?;
        if target.role == "owner" {
            return Err(DomainError::CannotActOnOwner);
        }

        let target_roles =
            db::server_role::roles_for_membership(&self.pool, server_id, target.id).await?;
        let target_top_position = target_roles.iter().map(|r| r.position).max().unwrap_or(0);
        Self::check_hierarchy(&ctx, target_top_position)?;

        let ban = db::server_role::insert_ban(
            &self.pool,
            new_id(),
            server_id,
            target_account_id,
            account_id,
            reason.as_deref(),
        )
        .await?;

        let recipients = self.remove_member(server_id, target_account_id).await?;
        Ok((recipients, ban.into()))
    }

    /// The one step shared by leave/kick/ban: resolve who needs to hear
    /// about it, THEN delete the `membership` row (`membership_role`
    /// cascades automatically) — the reference design's central
    /// idea, kept as literally one function.
    ///
    /// Order matters and is why this returns the recipient list instead of
    /// leaving that to the `api` handler: resolving AFTER the delete would
    /// miss the target being removed (their `membership` row, which
    /// `server_member_account_ids` reads, is already gone) and, for a kick or
    /// ban, that is exactly the one account that most needs to hear it. The
    /// realtime publish itself still happens in the handler — `domain` has
    /// no dependency on `realtime` — this just hands back
    /// the list resolved at the only correct moment to resolve it.
    ///
    /// Deliberately does NOT touch `server_ban` — `ban_member` inserts that
    /// row itself before calling this, so this stays the same single step
    /// regardless of which caller reaches it.
    async fn remove_member(
        &self,
        server_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let mut recipients = self.server_member_account_ids(server_id).await?;
        if !recipients.contains(&target_account_id) {
            recipients.push(target_account_id);
        }

        let removed =
            db::channel::delete_membership(&self.pool, server_id, target_account_id).await?;
        if !removed {
            return Err(DomainError::AccountNotFound);
        }

        Ok(recipients)
    }

    /// Owner-only, checked here rather than via `require_permission` — this
    /// is an all-or-nothing ownership bit, not role-based, same reasoning
    /// the reference design gives.
    /// Deletion itself is one query: every dependent table cascades from
    /// `server` (`channel`, `membership`, `server_role`, `membership_role`,
    /// `server_ban` — confirmed against the actual DDL, not assumed).
    pub async fn delete_server(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<(), DomainError> {
        let server = db::server::get_for_account(&self.pool, account_id, server_id)
            .await?
            .ok_or(DomainError::ServerNotFound)?;

        if server.owner_account_id != account_id {
            return Err(DomainError::MissingPermission);
        }

        db::server::delete(&self.pool, server_id).await?;
        Ok(())
    }

    /// Shared membership gate for every server/channel read or write in
    /// this slice: a caller may only act on a server (or its channels) if
    /// they hold a `membership` row for it — the authorization invariant,
    /// enforced here so it lives in exactly one place.
    async fn require_membership(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<(), DomainError> {
        let exists = db::channel::membership_exists(&self.pool, server_id, account_id).await?;

        if !exists {
            return Err(DomainError::ServerNotFound);
        }

        Ok(())
    }

    pub async fn send_message(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
        input: SendMessageInput,
    ) -> Result<MessageSummary, DomainError> {
        let channel = self.require_channel_access(account_id, channel_id).await?;
        validate_message_content(&input.content)?;

        // Timeout and mention gates only apply to server channels —
        // a dm/group_dm has no membership row to time out and no roles to
        // mention.
        if is_server_channel(&channel.kind) {
            let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
            self.require_not_timed_out(account_id, server_id).await?;

            let tokens = extract_mention_tokens(&input.content);
            if !tokens.is_empty() {
                self.check_mention_permissions(account_id, server_id, &tokens)
                    .await?;
            }
        }

        // A block placed after a 1:1 dm already exists must still stop new
        // messages in it ("prevents DMs" would otherwise be trivially
        // bypassed by messaging in a dm opened before the block).
        // Scoped to `kind == "dm"` only: a `group_dm` can have more than two
        // members, so "the other participant" isn't well-defined there, and
        // enforcing a block against one member of a group is a moderation
        // concern out of M0 scope.
        if channel.kind == "dm" {
            let other_account_id =
                db::dm::other_channel_member(&self.pool, channel_id, account_id).await?;

            if let Some(other_account_id) = other_account_id {
                if self
                    .blocked_either_direction(account_id, other_account_id)
                    .await?
                {
                    return Err(DomainError::Blocked);
                }
            }
        }

        let message =
            db::message::insert(&self.pool, new_id(), channel_id, account_id, &input.content)
                .await?;

        Ok(message.into())
    }

    pub async fn edit_message(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
        message_id: Uuid,
        input: EditMessageInput,
    ) -> Result<MessageSummary, DomainError> {
        let channel = self
            .require_own_message(account_id, channel_id, message_id)
            .await?;
        validate_message_content(&input.content)?;

        // Same mention gate `send_message` runs — an edit that inserts a
        // mention the author couldn't have posted originally must be
        // rejected exactly the same way, not just checked at creation time.
        if is_server_channel(&channel.kind) {
            let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
            let tokens = extract_mention_tokens(&input.content);
            if !tokens.is_empty() {
                self.check_mention_permissions(account_id, server_id, &tokens)
                    .await?;
            }
        }

        let message =
            db::message::update_content(&self.pool, message_id, channel_id, &input.content)
                .await?
                // `require_own_message` just confirmed this row exists and is ours —
                // `None` here would mean it vanished in the narrow window between
                // that check and this statement (M0 concurrency scale, same
                // residual-race tradeoff as `join_via_invite`'s membership check).
                .ok_or(DomainError::MessageNotFound)?;

        Ok(message.into())
    }

    /// The author may always delete their own message (unchanged);
    /// in a server channel, a `MANAGE_MESSAGES` holder may also delete
    /// someone else's — content moderation, not an action against the
    /// author's standing, so no hierarchy check (unlike kick/ban/timeout).
    /// A `dm`/`group_dm` channel has no roles to hold that bit in, so it
    /// stays author-only there, exactly as M0 shipped it.
    pub async fn delete_message(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
        message_id: Uuid,
    ) -> Result<(), DomainError> {
        let channel = self.require_channel_access(account_id, channel_id).await?;

        let row = db::message::find_owner(&self.pool, message_id, channel_id)
            .await?
            .ok_or(DomainError::MessageNotFound)?;
        if row.deleted_at.is_some() {
            return Err(DomainError::MessageNotFound);
        }

        if row.author_account_id != account_id {
            if !is_server_channel(&channel.kind) {
                return Err(DomainError::NotMessageAuthor);
            }
            let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
            self.require_permission(account_id, server_id, permissions::MANAGE_MESSAGES)
                .await?;
        }

        // Soft-delete only — the row stays ("M0 delete is author
        // soft-delete").
        db::message::soft_delete(&self.pool, message_id, channel_id).await?;

        Ok(())
    }

    /// Pins a message — requires `PIN_MESSAGES`. Idempotent: pinning
    /// an already-pinned message succeeds without changing `pinned_at`.
    pub async fn pin_message(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
        message_id: Uuid,
    ) -> Result<MessageSummary, DomainError> {
        let channel = self.require_channel_access(account_id, channel_id).await?;
        let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
        self.require_permission(account_id, server_id, permissions::PIN_MESSAGES)
            .await?;

        let message = db::message::pin(&self.pool, message_id, channel_id)
            .await?
            .ok_or(DomainError::MessageNotFound)?;

        Ok(message.into())
    }

    /// Unpins a message — same gate as pinning. Idempotent.
    pub async fn unpin_message(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
        message_id: Uuid,
    ) -> Result<MessageSummary, DomainError> {
        let channel = self.require_channel_access(account_id, channel_id).await?;
        let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
        self.require_permission(account_id, server_id, permissions::PIN_MESSAGES)
            .await?;

        let message = db::message::unpin(&self.pool, message_id, channel_id)
            .await?
            .ok_or(DomainError::MessageNotFound)?;

        Ok(message.into())
    }

    /// Reading pins needs no permission bit — same baseline tier as
    /// reading the channel's messages themselves.
    pub async fn list_pinned_messages(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
    ) -> Result<Vec<MessageSummary>, DomainError> {
        self.require_channel_access(account_id, channel_id).await?;

        let rows = db::message::list_pinned(&self.pool, channel_id).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Sets or clears (`until: None`) another member's timeout.
    /// Targets a specific member, so `check_hierarchy` applies, same tier as
    /// kick/ban — cannot silence someone who outranks or equals you even
    /// with the bit.
    pub async fn timeout_member(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        target_account_id: Uuid,
        input: TimeoutInput,
    ) -> Result<(), DomainError> {
        if account_id == target_account_id {
            return Err(DomainError::CannotActOnSelf);
        }

        if input.until <= Utc::now() {
            return Err(DomainError::Validation(
                "timeout `until` must be a future timestamp".to_string(),
            ));
        }

        let ctx = self
            .require_permission(account_id, server_id, permissions::TIMEOUT_MEMBERS)
            .await?;

        let target = db::channel::find_membership(&self.pool, server_id, target_account_id)
            .await?
            .ok_or(DomainError::AccountNotFound)?;
        if target.role == "owner" {
            return Err(DomainError::CannotActOnOwner);
        }

        let target_roles =
            db::server_role::roles_for_membership(&self.pool, server_id, target.id).await?;
        let target_top_position = target_roles.iter().map(|r| r.position).max().unwrap_or(0);
        Self::check_hierarchy(&ctx, target_top_position)?;

        let updated = db::channel::set_member_timeout(
            &self.pool,
            server_id,
            target_account_id,
            Some(input.until),
            input.reason.as_deref(),
        )
        .await?;
        if !updated {
            return Err(DomainError::AccountNotFound);
        }

        Ok(())
    }

    /// Clears a timeout early — same permission and hierarchy gate
    /// as setting one.
    pub async fn clear_timeout(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        target_account_id: Uuid,
    ) -> Result<(), DomainError> {
        let ctx = self
            .require_permission(account_id, server_id, permissions::TIMEOUT_MEMBERS)
            .await?;

        let target = db::channel::find_membership(&self.pool, server_id, target_account_id)
            .await?
            .ok_or(DomainError::AccountNotFound)?;

        let target_roles =
            db::server_role::roles_for_membership(&self.pool, server_id, target.id).await?;
        let target_top_position = target_roles.iter().map(|r| r.position).max().unwrap_or(0);
        Self::check_hierarchy(&ctx, target_top_position)?;

        let updated =
            db::channel::set_member_timeout(&self.pool, server_id, target_account_id, None, None)
                .await?;
        if !updated {
            return Err(DomainError::AccountNotFound);
        }

        Ok(())
    }

    /// Sets another member's server nickname — requires
    /// `MANAGE_NICKNAMES` + hierarchy against the target. Setting YOUR OWN
    /// stays baseline (no bit, no hierarchy check) — this branch is never
    /// reached for `account_id == target_account_id`, which the `api` handler
    /// routes straight past the permission check (see
    /// `handlers::update_member_nickname`).
    pub async fn update_member_nickname(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        target_account_id: Uuid,
        nickname: Option<String>,
    ) -> Result<(), DomainError> {
        if account_id != target_account_id {
            let ctx = self
                .require_permission(account_id, server_id, permissions::MANAGE_NICKNAMES)
                .await?;

            let target = db::channel::find_membership(&self.pool, server_id, target_account_id)
                .await?
                .ok_or(DomainError::AccountNotFound)?;

            let target_roles =
                db::server_role::roles_for_membership(&self.pool, server_id, target.id).await?;
            let target_top_position = target_roles.iter().map(|r| r.position).max().unwrap_or(0);
            Self::check_hierarchy(&ctx, target_top_position)?;
        } else {
            // Still must be a member to set your own nickname.
            self.require_membership(account_id, server_id).await?;
        }

        let updated = db::channel::update_nickname(
            &self.pool,
            server_id,
            target_account_id,
            nickname.as_deref(),
        )
        .await?;
        if !updated {
            return Err(DomainError::AccountNotFound);
        }

        Ok(())
    }

    /// Blocks write actions (send message, create thread, join
    /// voice) for a timed-out member — reading is unaffected. A no-op for a
    /// caller with no `timeout_until` or one already in the past.
    async fn require_not_timed_out(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<(), DomainError> {
        let membership = db::channel::find_membership(&self.pool, server_id, account_id)
            .await?
            .ok_or(DomainError::ServerNotFound)?;

        if let Some(until) = membership.timeout_until {
            if until > Utc::now() {
                return Err(DomainError::MemberTimedOut);
            }
        }

        Ok(())
    }

    /// Whether `account_id` is currently timed out in
    /// `channel_id`'s server — the gateway's voice-join path calls this
    /// indirectly, through `can_join_voice`, which is the ONLY authorization
    /// the gateway's `VoiceJoin` branch runs (it does not also consult
    /// `authorized_account_ids`, unlike the rest of the gateway's fan-out).
    /// `false` for a non-server channel or a non-member — `can_join_voice`
    /// (and `require_channel_access`, which it calls first) already gate
    /// real access; this only answers the timeout question.
    pub async fn is_member_timed_out(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
    ) -> Result<bool, DomainError> {
        let channel = self.lookup_channel(channel_id).await?;
        self.is_member_timed_out_for_channel(account_id, &channel)
            .await
    }

    /// Same check as [`Self::is_member_timed_out`], but for a channel already
    /// looked up by the caller. `can_join_voice` calls this directly with the
    /// `ChannelAccessRow` `require_channel_access` already returned, rather
    /// than calling `is_member_timed_out` and paying a second
    /// `lookup_channel` round trip for the same `channel_id` on every voice
    /// join.
    async fn is_member_timed_out_for_channel(
        &self,
        account_id: Uuid,
        channel: &db::channel::ChannelAccessRow,
    ) -> Result<bool, DomainError> {
        if !is_server_channel(&channel.kind) {
            return Ok(false);
        }
        let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;

        let membership = db::channel::find_membership(&self.pool, server_id, account_id).await?;
        Ok(membership
            .and_then(|m| m.timeout_until)
            .is_some_and(|until| until > Utc::now()))
    }

    /// `tokens` are lowercase, `@`-stripped candidates from
    /// `extract_mention_tokens` — resolves which ones name something real
    /// (`everyone`/`here`, or a role's slug) and whether `account_id` may use
    /// them. A token matching nothing real is silently ignored (not a
    /// mention, just text that happens to start with `@`).
    async fn check_mention_permissions(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        tokens: &[String],
    ) -> Result<(), DomainError> {
        let wants_everyone = tokens.iter().any(|t| t == "everyone" || t == "here");
        let role_tokens: Vec<&String> = tokens
            .iter()
            .filter(|t| t.as_str() != "everyone" && t.as_str() != "here")
            .collect();

        if !wants_everyone && role_tokens.is_empty() {
            return Ok(());
        }

        let ctx = self.member_context(account_id, server_id).await?;
        let bypass = ctx.is_owner || permissions::has(ctx.permissions, permissions::ADMIN);

        if wants_everyone
            && !bypass
            && !permissions::has(ctx.permissions, permissions::MENTION_EVERYONE)
        {
            return Err(DomainError::MentionNotAllowed);
        }

        if !role_tokens.is_empty() && !bypass {
            let has_mention_roles = permissions::has(ctx.permissions, permissions::MENTION_ROLES);
            if !has_mention_roles {
                let roles = db::server_role::list_roles(&self.pool, server_id).await?;
                for token in &role_tokens {
                    let matched_role = roles.iter().find(|r| &slugify(&r.name) == *token);
                    if let Some(role) = matched_role {
                        if !role.mentionable {
                            return Err(DomainError::MentionNotAllowed);
                        }
                    }
                    // No matching role: plain text, not a mention, not an error.
                }
            }
        }

        Ok(())
    }

    pub async fn list_messages(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
        pagination: MessagePagination,
    ) -> Result<Vec<MessageSummary>, DomainError> {
        self.require_channel_access(account_id, channel_id).await?;

        let limit = pagination
            .limit
            .unwrap_or(DEFAULT_MESSAGE_LIMIT)
            .min(MAX_MESSAGE_LIMIT) as i64;

        // Newest-first, `before` pages backward in time
        // (the frozen cursor pagination convention).
        // Soft-deleted rows are NOT filtered out here — only
        // `MessageRow::into<MessageSummary>` nulls their content.
        let rows = db::message::list_page(&self.pool, channel_id, pagination.before, limit).await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Full-text search across `server_id`'s messages. Gated at
    /// server-membership granularity like `list_channels`/`list_members` —
    /// a member's search covers every channel they could otherwise read,
    /// with no extra per-channel check needed (a member already sees every
    /// channel via `require_channel_access` regardless of its
    /// visibility override; visibility only restricts a
    /// NON-member, which this method does not serve — that is
    /// the public read path's concern, scoped separately once it exists).
    pub async fn search_messages(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        input: SearchInput,
    ) -> Result<Vec<MessageSummary>, DomainError> {
        self.require_membership(account_id, server_id).await?;
        validate_search_query(&input.query)?;

        let limit = input
            .limit
            .unwrap_or(DEFAULT_MESSAGE_LIMIT)
            .min(MAX_MESSAGE_LIMIT) as i64;

        // A restricted channel's messages must never surface here
        // for a caller without a real grant — search is server-wide, unlike
        // every other read path, so this is the one place that filter has to
        // be applied explicitly rather than inherited from a per-channel
        // check.
        let ctx = self.member_context(account_id, server_id).await?;
        let bypass = ctx.is_owner || permissions::has(ctx.permissions, permissions::ADMIN);

        let rows = db::message::search_in_server(
            &self.pool,
            server_id,
            &input.query,
            input.author_account_id,
            input.channel_id,
            input.created_after,
            input.created_before,
            input.before,
            limit,
            bypass,
            &ctx.role_ids,
            channel_permissions::VIEW_CHANNEL,
        )
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Resolves who should receive realtime events for a channel — the
    /// `realtime` crate's `Hub::publish_*` calls this to find authorized
    /// recipients at publish time (authorization is always checked
    /// server-side at publish time). No caller to
    /// authorize here; this answers "who's allowed to hear about this
    /// channel", not "may this caller act on it".
    /// Every account id that should hear about a server-wide event — a role
    /// change, a member joining/leaving, the server itself being deleted
    /// (M2). Thin wrapper over the same
    /// `db::channel::server_member_account_ids` a server channel's
    /// `authorized_account_ids` already uses; exposed directly here because
    /// role/member events are server-scoped, not channel-scoped.
    pub async fn server_member_account_ids(
        &self,
        server_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        Ok(db::channel::server_member_account_ids(&self.pool, server_id).await?)
    }

    pub async fn authorized_account_ids(&self, channel_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        let channel = self.lookup_channel(channel_id).await?;

        let ids = if is_server_channel(&channel.kind) {
            let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
            db::channel::server_member_account_ids(&self.pool, server_id).await?
        } else {
            db::channel::channel_member_account_ids(&self.pool, channel_id).await?
        };

        Ok(ids)
    }

    /// Checks whether `account_id` is authorized to join `channel_id` for a voice call:
    /// 1. Channel exists and caller has access (`require_channel_access`, enforcing
    ///    membership and `channel.restricted` with `channel_permissions::VIEW_CHANNEL`).
    /// 2. Channel is a voice channel (`channel.kind == "voice"`).
    /// 3. Caller is not timed out (`!is_member_timed_out`).
    pub async fn can_join_voice(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
    ) -> Result<bool, DomainError> {
        let channel = match self.require_channel_access(account_id, channel_id).await {
            Ok(channel) => channel,
            Err(DomainError::ChannelNotFound) => return Ok(false),
            Err(err) => return Err(err),
        };

        if channel.kind != "voice" {
            return Ok(false);
        }

        // `channel` was already looked up by `require_channel_access` above;
        // calling `is_member_timed_out(account_id, channel_id)` here would
        // look it up again for the same `channel_id` on every voice-join
        // attempt.
        if self
            .is_member_timed_out_for_channel(account_id, &channel)
            .await?
        {
            return Ok(false);
        }

        Ok(true)
    }

    /// Checks whether `channel_id` belongs to `server_id`.
    pub async fn channel_belongs_to_server(
        &self,
        channel_id: Uuid,
        server_id: Uuid,
    ) -> Result<bool, DomainError> {
        match self.lookup_channel(channel_id).await {
            Ok(channel) => Ok(channel.server_id == Some(server_id)),
            Err(DomainError::ChannelNotFound) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Every channel id `account_id` may receive realtime events for — the
    /// gateway's `ready` event payload (the set of channel ids the
    /// account may receive events for). Server channels via `membership`,
    /// dm/group_dm channels via `channel_member` — the same two paths
    /// `require_channel_access` checks, just enumerated instead of tested
    /// against one channel.
    pub async fn accessible_channel_ids(&self, account_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        let ids = db::channel::accessible_channel_ids(&self.pool, account_id).await?;
        Ok(ids)
    }

    /// Who may observe `account_id`'s presence — the recipients of a
    /// `presence.update` event.
    ///
    /// Scope is "everyone sharing a server or a dm/group_dm with the
    /// subject" — the same reach `message.create` fan-out already allows,
    /// resolved by one set-based query
    /// (`db::channel::observer_account_ids_for_account`) rather than a query
    /// per accessible channel: that loop's cost grew with thread count
    /// (a thread is itself a channel row), which made it unbounded by
    /// server size in a way the original per-channel-cost estimate didn't
    /// account for.
    ///
    /// The subject's own connections are included, because it is a member of
    /// its own channels — a second tab legitimately learns the account came
    /// online.
    ///
    /// Anyone who has blocked `account_id` is excluded — "a block hides the
    /// blocked user's social presence from the blocker". Directional,
    /// matching the rest of `block`: `account_id` blocking
    /// someone else has no effect here, only being blocked does.
    pub async fn presence_observer_account_ids(
        &self,
        account_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        let observers =
            db::channel::observer_account_ids_for_account(&self.pool, account_id).await?;
        let blockers: HashSet<Uuid> = db::block::blockers_of(&self.pool, account_id)
            .await?
            .into_iter()
            .collect();

        Ok(observers
            .into_iter()
            .filter(|observer| !blockers.contains(observer))
            .collect())
    }

    /// Every account `account_id` has blocked — used to mask presence for a
    /// viewer reading a member list (`ApiError`-free, plain ids; richer
    /// `BlockSummary` is `list_blocks`).
    pub async fn blocked_account_ids(&self, account_id: Uuid) -> Result<Vec<Uuid>, DomainError> {
        let rows = db::block::list_for_account(&self.pool, account_id).await?;
        Ok(rows.into_iter().map(|row| row.blocked_account_id).collect())
    }

    /// Gathers the facts `decide_profile_visibility` needs for `caller_id`
    /// viewing `target_id`: relationship, both directional blocks, and,
    /// when `server_id` is given, whether the two share that server. Makes
    /// no visibility decision and does not resolve realtime presence.
    pub async fn get_profile_context(
        &self,
        caller_id: Uuid,
        target_id: Uuid,
        server_id: Option<Uuid>,
    ) -> Result<ProfileContext, DomainError> {
        let mut profiles = db::profile::get_profiles_bulk(&self.pool, &[target_id]).await?;
        let profile = profiles.pop().ok_or(DomainError::AccountNotFound)?;

        let relationship = if caller_id == target_id {
            crate::profile_visibility::ProfileViewerRelationship::SelfView
        } else {
            let (low, high) = if caller_id < target_id {
                (caller_id, target_id)
            } else {
                (target_id, caller_id)
            };
            match db::friendship::find_by_pair(&self.pool, low, high).await? {
                Some(row) if row.status == "accepted" => {
                    crate::profile_visibility::ProfileViewerRelationship::Friend
                }
                _ => crate::profile_visibility::ProfileViewerRelationship::None,
            }
        };

        let (caller_blocked_owner, owner_blocked_caller) = if caller_id == target_id {
            (false, false)
        } else {
            (
                db::block::is_blocking(&self.pool, caller_id, target_id).await?,
                db::block::is_blocking(&self.pool, target_id, caller_id).await?,
            )
        };

        let (server_context, has_shared_server_context) = match server_id {
            Some(server_id) => {
                let target_context =
                    db::profile::get_server_context(&self.pool, server_id, target_id).await?;
                let caller_is_member =
                    db::channel::find_membership(&self.pool, server_id, caller_id)
                        .await?
                        .is_some();
                let shared = target_context.is_some() && caller_is_member;
                (if shared { target_context } else { None }, shared)
            }
            None => (None, false),
        };

        Ok(ProfileContext {
            profile,
            relationship,
            caller_blocked_owner,
            owner_blocked_caller,
            server_context,
            has_shared_server_context,
        })
    }

    /// Resolves a username to the same context `get_profile_context` builds.
    pub async fn get_profile_context_by_username(
        &self,
        caller_id: Uuid,
        username: &str,
        server_id: Option<Uuid>,
    ) -> Result<ProfileContext, DomainError> {
        let target_id = db::profile::find_id_by_username(&self.pool, username)
            .await?
            .ok_or(DomainError::AccountNotFound)?;
        self.get_profile_context(caller_id, target_id, server_id)
            .await
    }

    /// The same facts as `get_profile_context`, for many accounts at once.
    /// The caller's relationship and block sets are each read once for the
    /// whole page rather than per account, so the query count does not grow
    /// with `target_ids`. Carries no server context. Accounts that do not
    /// exist are absent from the result; the order of `target_ids` is kept.
    pub async fn get_profile_contexts_bulk(
        &self,
        caller_id: Uuid,
        target_ids: &[Uuid],
    ) -> Result<Vec<ProfileContext>, DomainError> {
        let profiles = db::profile::get_profiles_bulk(&self.pool, target_ids).await?;

        let friends: HashSet<Uuid> = db::friendship::list_for_account(&self.pool, caller_id)
            .await?
            .into_iter()
            .filter(|row| row.status == "accepted")
            .map(|row| {
                if row.account_low == caller_id {
                    row.account_high
                } else {
                    row.account_low
                }
            })
            .collect();
        let caller_blocked: HashSet<Uuid> = db::block::list_for_account(&self.pool, caller_id)
            .await?
            .into_iter()
            .map(|row| row.blocked_account_id)
            .collect();
        let blocked_caller: HashSet<Uuid> = db::block::blockers_of(&self.pool, caller_id)
            .await?
            .into_iter()
            .collect();

        Ok(profiles
            .into_iter()
            .map(|profile| {
                let target_id = profile.id;
                let relationship = if target_id == caller_id {
                    crate::profile_visibility::ProfileViewerRelationship::SelfView
                } else if friends.contains(&target_id) {
                    crate::profile_visibility::ProfileViewerRelationship::Friend
                } else {
                    crate::profile_visibility::ProfileViewerRelationship::None
                };

                ProfileContext {
                    profile,
                    relationship,
                    caller_blocked_owner: caller_blocked.contains(&target_id),
                    owner_blocked_caller: blocked_caller.contains(&target_id),
                    server_context: None,
                    has_shared_server_context: false,
                }
            })
            .collect())
    }

    /// Shared channel-access gate for every message method: a caller may
    /// only act on a channel if they hold a `membership` row for its server
    /// (`text` channels) or a `channel_member` row for the channel itself
    /// (`dm`/`group_dm` channels) — the authorization invariants, enforced
    /// here so message endpoints and (per ROADMAP slice
    /// 6) DM endpoints share exactly one implementation. Not found OR not
    /// authorized both collapse to `ChannelNotFound`, same non-leaking
    /// pattern as `require_membership`.
    async fn require_channel_access(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
    ) -> Result<db::channel::ChannelAccessRow, DomainError> {
        let channel = self.lookup_channel(channel_id).await?;

        let authorized = if is_server_channel(&channel.kind) {
            let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
            db::channel::membership_exists(&self.pool, server_id, account_id).await?
        } else {
            db::channel::channel_member_exists(&self.pool, channel_id, account_id).await?
        };

        if !authorized {
            return Err(DomainError::ChannelNotFound);
        }

        // Membership alone isn't enough for a `restricted` channel
        // — the caller also needs a role grant (or owner/ADMIN). `restricted`
        // here is already the EFFECTIVE value (`find_access_by_id` resolves
        // a thread to its parent's), and `dm`/`group_dm` channels are never
        // `restricted` (the column only ever applies to server channels), so
        // this is a no-op for them.
        if channel.restricted {
            let server_id = channel.server_id.ok_or(DomainError::ChannelNotFound)?;
            let grant_channel_id = channel.parent_channel_id.unwrap_or(channel_id);
            if !self
                .member_can_view_channel(account_id, server_id, grant_channel_id)
                .await?
            {
                return Err(DomainError::ChannelNotFound);
            }
        }

        Ok(channel)
    }

    /// Owner/`ADMIN` bypass; otherwise ANY of the caller's held
    /// roles needs a `VIEW_CHANNEL` grant on `grant_channel_id` — already
    /// resolved to the parent's id for a thread by every caller of this
    /// function. Pure OR across roles, no deny semantics (see the ADR for
    /// why that's enough).
    async fn member_can_view_channel(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        grant_channel_id: Uuid,
    ) -> Result<bool, DomainError> {
        let ctx = self.member_context(account_id, server_id).await?;
        if ctx.is_owner || permissions::has(ctx.permissions, permissions::ADMIN) {
            return Ok(true);
        }
        Ok(db::channel::role_has_channel_permission(
            &self.pool,
            grant_channel_id,
            &ctx.role_ids,
            channel_permissions::VIEW_CHANNEL,
        )
        .await?)
    }

    async fn lookup_channel(
        &self,
        channel_id: Uuid,
    ) -> Result<db::channel::ChannelAccessRow, DomainError> {
        db::channel::find_access_by_id(&self.pool, channel_id)
            .await?
            .ok_or(DomainError::ChannelNotFound)
    }

    /// Shared lookup + authorization gate for `edit_message`/`delete_message`:
    /// confirms the channel is accessible, the message exists in it, isn't
    /// already soft-deleted, and belongs to `account_id`. Factored out so
    /// the two callers share exactly one implementation rather than
    /// duplicating this chain of checks.
    async fn require_own_message(
        &self,
        account_id: Uuid,
        channel_id: Uuid,
        message_id: Uuid,
    ) -> Result<db::channel::ChannelAccessRow, DomainError> {
        let channel = self.require_channel_access(account_id, channel_id).await?;

        let row = db::message::find_owner(&self.pool, message_id, channel_id)
            .await?
            .ok_or(DomainError::MessageNotFound)?;

        // A soft-deleted message is treated as gone for editing purposes
        // even though it still exists in `list_messages` results.
        if row.deleted_at.is_some() {
            return Err(DomainError::MessageNotFound);
        }

        if row.author_account_id != account_id {
            return Err(DomainError::NotMessageAuthor);
        }

        Ok(channel)
    }

    // ---- full export ----

    /// Enqueues a new export job for `server_id`. Gated behind
    /// `ADMIN` (or ownership, which `require_permission` already bypasses
    /// with) — a full-server dump is a bulk administrative action, not
    /// something an ordinary member gets. Processing happens later, out of
    /// band, by `process_next_export_job`.
    pub async fn request_export(
        &self,
        account_id: Uuid,
        server_id: Uuid,
    ) -> Result<ExportJobSummary, DomainError> {
        self.require_permission(account_id, server_id, permissions::ADMIN)
            .await?;

        let job = db::export::insert_pending(&self.pool, new_id(), server_id, account_id).await?;
        Ok(export_job_summary(job))
    }

    /// Polls the status of one export job. Same `ADMIN`-or-owner gate as
    /// `request_export` — a job's existence and its `download_url` are not
    /// meant for anyone outside that tier, since the download is the whole
    /// server's content regardless of who requested it.
    pub async fn get_export_job(
        &self,
        account_id: Uuid,
        server_id: Uuid,
        job_id: Uuid,
    ) -> Result<ExportJobSummary, DomainError> {
        self.require_permission(account_id, server_id, permissions::ADMIN)
            .await?;

        let job = db::export::find_for_server(&self.pool, server_id, job_id)
            .await?
            .ok_or(DomainError::ExportJobNotFound)?;

        Ok(export_job_summary(job))
    }

    /// Claims and processes exactly one `pending` export job, if any exist.
    /// Returns `Ok(true)` if a job was claimed (whether it then
    /// succeeded or failed — both are terminal states written to the row,
    /// not propagated as an `Err` here, since a poll-loop caller has no
    /// request to fail), `Ok(false)` if the queue was empty. Intended to be
    /// called in a loop by `crates/server`'s worker task, never from an HTTP
    /// handler — this is the one export function that needs
    /// `storage::StorageService`, kept out of every other export method
    /// (and out of `AppState`) on purpose; see
    /// `migrations/0014_export_jobs.sql`'s own note on why.
    pub async fn process_next_export_job(
        &self,
        storage: &storage::StorageService,
    ) -> Result<bool, DomainError> {
        let mut tx = self.pool.begin().await?;
        let Some(job) = db::export::claim_next_pending(&mut tx).await? else {
            tx.commit().await?;
            return Ok(false);
        };
        tx.commit().await?;

        match self
            .build_and_upload_export(storage, job.server_id, job.id)
            .await
        {
            Ok((storage_key, download_url)) => {
                db::export::mark_done(&self.pool, job.id, &storage_key, &download_url).await?;
            }
            Err(err) => {
                db::export::mark_failed(&self.pool, job.id, &err.to_string()).await?;
            }
        }

        Ok(true)
    }

    /// Builds the full JSON dump of `server_id`'s own channels, threads, and
    /// messages, uploads it, and returns `(storage_key, download_url)`.
    /// Markdown rendering and zipping (the export design's other named format) are
    /// deliberately not implemented in this slice — JSON alone already
    /// delivers the ADR's core promise ("a user's own data, to themselves,
    /// zero legal risk"); a second rendered format is additive follow-up,
    /// not required to prove the job/storage plumbing works.
    async fn build_and_upload_export(
        &self,
        storage: &storage::StorageService,
        server_id: Uuid,
        job_id: Uuid,
    ) -> Result<(String, String), DomainError> {
        let channels = db::channel::list_by_server(&self.pool, server_id).await?;
        let top_level_ids: Vec<Uuid> = channels
            .iter()
            .filter(|c| c.kind != "thread")
            .map(|c| c.id)
            .collect();

        // Every thread under every top-level channel, and every message in
        // every top-level channel AND thread, fetched in two round trips
        // total instead of two per channel plus one per thread — grouped
        // back into per-channel/per-thread buckets here in Rust, same
        // "batch resolve" split the rest of the crate already uses for
        // listing paths (see `list_channels`'s own restricted-flag/grant
        // batching).
        let threads = db::channel::list_threads_by_parents(&self.pool, &top_level_ids).await?;
        let thread_ids: Vec<Uuid> = threads.iter().map(|t| t.id).collect();
        let mut threads_by_parent: std::collections::HashMap<Uuid, Vec<db::channel::ChannelRow>> =
            std::collections::HashMap::new();
        for thread in threads {
            if let Some(parent_id) = thread.parent_channel_id {
                threads_by_parent.entry(parent_id).or_default().push(thread);
            }
        }

        let all_ids: Vec<Uuid> = top_level_ids.iter().copied().chain(thread_ids).collect();
        let messages = db::message::list_all_for_export_batch(&self.pool, &all_ids).await?;
        let mut messages_by_channel: std::collections::HashMap<Uuid, Vec<db::message::MessageRow>> =
            std::collections::HashMap::new();
        for message in messages {
            messages_by_channel
                .entry(message.channel_id)
                .or_default()
                .push(message);
        }

        let empty_messages: Vec<db::message::MessageRow> = Vec::new();
        let empty_threads: Vec<db::channel::ChannelRow> = Vec::new();

        let mut channels_json = Vec::new();
        for channel in channels.iter().filter(|c| c.kind != "thread") {
            let channel_messages = messages_by_channel
                .get(&channel.id)
                .unwrap_or(&empty_messages);
            let channel_threads = threads_by_parent.get(&channel.id).unwrap_or(&empty_threads);

            let threads_json: Vec<_> = channel_threads
                .iter()
                .map(|thread| {
                    let thread_messages =
                        messages_by_channel.get(&thread.id).unwrap_or(&empty_messages);
                    serde_json::json!({
                        "id": thread.id.to_string(),
                        "title": thread.title,
                        "created_at": thread.created_at,
                        "messages": thread_messages.iter().map(message_export_json).collect::<Vec<_>>(),
                    })
                })
                .collect();

            channels_json.push(serde_json::json!({
                "id": channel.id.to_string(),
                "kind": channel.kind,
                "name": channel.name,
                "created_at": channel.created_at,
                "messages": channel_messages.iter().map(message_export_json).collect::<Vec<_>>(),
                "threads": threads_json,
            }));
        }

        let export = serde_json::json!({
            "server_id": server_id.to_string(),
            "exported_at": chrono::Utc::now(),
            "channels": channels_json,
        });

        let bytes = serde_json::to_vec_pretty(&export).map_err(|err| {
            DomainError::ExportFailed(format!("failed to serialize export: {err}"))
        })?;

        let storage_key = format!("exports/{server_id}/{job_id}.json");
        storage
            .put_object(&storage_key, bytes, "application/json")
            .await
            .map_err(|err| DomainError::ExportFailed(err.to_string()))?;

        let download_url = storage
            .presigned_get(&storage_key, std::time::Duration::from_secs(24 * 60 * 60))
            .await
            .map_err(|err| DomainError::ExportFailed(err.to_string()))?;

        Ok((storage_key, download_url))
    }
}

fn export_job_summary(job: db::export::ExportJobRow) -> ExportJobSummary {
    ExportJobSummary {
        id: job.id,
        server_id: job.server_id,
        status: job.status,
        download_url: job.download_url,
        error: job.error,
        created_at: job.created_at,
        completed_at: job.completed_at,
    }
}

/// A `message` row's export shape — id, author, content, and
/// timestamps; no `deleted_at` (soft-deleted rows never reach this, per
/// `list_all_for_export`'s own filter).
fn message_export_json(row: &db::message::MessageRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id.to_string(),
        "author_account_id": row.author_account_id.to_string(),
        "content": row.content,
        "created_at": row.created_at,
        "edited_at": row.edited_at,
    })
}

/// `membership` has exactly one UNIQUE constraint, `(server_id,
/// account_id)`, so any unique violation on this insert can only be that
/// one — unlike `auth::map_account_conflict`, there is no need to inspect
/// the constraint name to disambiguate.
fn map_membership_conflict(err: sqlx::Error) -> DomainError {
    if let sqlx::Error::Database(db_err) = &err {
        if db_err.is_unique_violation() {
            return DomainError::AlreadyMember;
        }
    }
    DomainError::Database(err)
}
