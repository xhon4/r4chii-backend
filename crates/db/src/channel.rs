use app_core::Uuid;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// `server_id` is `Option` — `Some` for a `text`/`voice` channel, `None` for
/// `dm`/`group_dm` (a CHECK constraint enforces exactly this split at the
/// DB level; this struct just mirrors it).
#[derive(sqlx::FromRow)]
pub struct ChannelRow {
    pub id: Uuid,
    pub server_id: Option<Uuid>,
    pub kind: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    /// `NULL` = inherit the server's visibility unchanged; a non-`NULL`
    /// value narrows it. Always `NULL` for `dm`/`group_dm`.
    pub visibility: Option<String>,
    /// The parent text channel a `thread` row belongs to. `NULL`
    /// for every other kind.
    pub parent_channel_id: Option<Uuid>,
    /// The message a thread was spawned from, if any — `NULL` for a
    /// standalone thread (a new top-level topic, not a reply thread) and for
    /// every non-thread kind.
    pub root_message_id: Option<Uuid>,
    /// A thread's display title. `NULL` for `text`/`voice`/`dm`/`group_dm`,
    /// which use `name` instead.
    pub title: Option<String>,
    /// URL-safe, unique per `parent_channel_id` (a thread's canonical URL).
    /// `NULL` for every non-thread kind.
    pub slug: Option<String>,
    /// RAW value of this row's own column — `false` for a thread
    /// regardless of its parent's restriction (threads never carry their own
    /// restriction, they inherit it; see `ChannelAccessRow`/
    /// `VisibilityContextRow` below for the EFFECTIVE, parent-resolved value
    /// used in access decisions).
    pub restricted: bool,
}

/// Everything `DomainService::resolve_read_access` needs to compute a
/// channel's effective visibility and whether it is even server-attached, in
/// one query. `server_id`/`server_visibility` are both `NULL` for
/// `dm`/`group_dm` channels, which sit outside this visibility axis
/// entirely — they are gated by `channel_member` alone.
#[derive(sqlx::FromRow)]
pub struct VisibilityContextRow {
    pub server_id: Option<Uuid>,
    pub kind: String,
    pub channel_visibility: Option<String>,
    pub server_visibility: Option<String>,
    /// EFFECTIVE — a thread's PARENT's `restricted` value, not its
    /// own (always `false`) unused column. `false` for `dm`/`group_dm`.
    pub restricted: bool,
    /// `NULL` for every non-thread kind — needed to know which
    /// `channel_id` a `channel_role_permission` grant lookup should target
    /// (the parent's, for a thread).
    pub parent_channel_id: Option<Uuid>,
}

/// Minimal channel projection used only to decide access — `server_id` is
/// `Option` here (unlike `ChannelRow`) because this row is fetched for
/// dm/group_dm channels too, where the column is NULL.
#[derive(sqlx::FromRow)]
pub struct ChannelAccessRow {
    pub server_id: Option<Uuid>,
    pub kind: String,
    /// EFFECTIVE (parent-resolved for a thread) — see
    /// `VisibilityContextRow`'s own doc comment, same reasoning.
    pub restricted: bool,
    /// `NULL` for every non-thread kind.
    pub parent_channel_id: Option<Uuid>,
}

/// Inserts a server-owned channel. `kind` is caller-supplied but never
/// caller-trusted: `domain`'s `validate_channel_kind` narrows it to
/// `text`/`voice` before this is reached, and the `channel_kind_check` /
/// `channel_check` constraints are the backstop if that ever regresses.
pub async fn insert_server_channel(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    server_id: Uuid,
    kind: &str,
    name: &str,
) -> Result<ChannelRow, sqlx::Error> {
    // `position` is left NULL: no reordering feature in M0 (the column
    // exists in the schema but nothing in this slice's scope touches it).
    // `visibility` is left NULL (inherit the server's) — the visibility
    // model has no creation-time override, only the dedicated update
    // endpoint below.
    sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO channel (id, server_id, kind, name) \
         VALUES ($1, $2, $3, $4) \
         RETURNING id, server_id, kind, name, created_at, visibility, \
                   parent_channel_id, root_message_id, title, slug, restricted",
    )
    .bind(id)
    .bind(server_id)
    .bind(kind)
    .bind(name)
    .fetch_one(executor)
    .await
}

/// Count of `text`/`voice` channels a server owns — threads are excluded,
/// they have no bearing on `MAX_CHANNELS_PER_SERVER` (their growth is
/// unbounded on purpose, scoped per parent channel instead).
pub async fn count_by_server(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM channel WHERE server_id = $1 AND kind IN ('text', 'voice')",
    )
    .bind(server_id)
    .fetch_one(executor)
    .await
}

pub async fn list_by_server(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<Vec<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT id, server_id, kind, name, created_at, visibility, \
                parent_channel_id, root_message_id, title, slug, restricted \
         FROM channel WHERE server_id = $1 ORDER BY created_at ASC",
    )
    .bind(server_id)
    .fetch_all(executor)
    .await
}

/// Inserts a thread — a `channel` row with `kind = 'thread'`,
/// `parent_channel_id` set, and `server_id` copied from the parent so the
/// EXISTING `membership`-based access check (`is_server_channel`/
/// `require_channel_access` in `domain`) authorizes it with zero changes —
/// no separate "walk to the parent" branch needed, because a thread carries
/// its own server id by construction. `root_message_id` is `None` for a
/// standalone thread (a new top-level topic, not a reply thread).
pub async fn insert_thread(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    server_id: Uuid,
    parent_channel_id: Uuid,
    root_message_id: Option<Uuid>,
    title: &str,
    slug: &str,
) -> Result<ChannelRow, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO channel (id, server_id, kind, parent_channel_id, root_message_id, title, slug) \
         VALUES ($1, $2, 'thread', $3, $4, $5, $6) \
         RETURNING id, server_id, kind, name, created_at, visibility, \
                   parent_channel_id, root_message_id, title, slug, restricted",
    )
    .bind(id)
    .bind(server_id)
    .bind(parent_channel_id)
    .bind(root_message_id)
    .bind(title)
    .bind(slug)
    .fetch_one(executor)
    .await
}

/// Every thread under one parent channel, newest first — the "N threads" list
/// a channel view shows.
pub async fn list_threads_by_parent(
    executor: impl PgExecutor<'_>,
    parent_channel_id: Uuid,
) -> Result<Vec<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT id, server_id, kind, name, created_at, visibility, \
                parent_channel_id, root_message_id, title, slug, restricted \
         FROM channel WHERE parent_channel_id = $1 AND kind = 'thread' \
         ORDER BY created_at DESC",
    )
    .bind(parent_channel_id)
    .fetch_all(executor)
    .await
}

/// Batched form of `list_threads_by_parent` — every thread under ANY of
/// `parent_channel_ids`, ordered by parent then newest first, in one query
/// instead of one per parent. `DomainService::build_and_upload_export`'s own
/// way of fetching every server channel's threads in one round trip.
pub async fn list_threads_by_parents(
    executor: impl PgExecutor<'_>,
    parent_channel_ids: &[Uuid],
) -> Result<Vec<ChannelRow>, sqlx::Error> {
    if parent_channel_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, ChannelRow>(
        "SELECT id, server_id, kind, name, created_at, visibility, \
                parent_channel_id, root_message_id, title, slug, restricted \
         FROM channel WHERE parent_channel_id = ANY($1) AND kind = 'thread' \
         ORDER BY parent_channel_id, created_at DESC",
    )
    .bind(parent_channel_ids)
    .fetch_all(executor)
    .await
}

/// A single thread by id, only if it really is one (`kind = 'thread'`) —
/// the public read path's lookup. Passing a non-thread id (a
/// text/voice/dm channel) returns `None`, same as a nonexistent id, since
/// nothing outside a thread has a canonical `/archive/t/{id}` URL.
pub async fn find_thread(
    executor: impl PgExecutor<'_>,
    thread_id: Uuid,
) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT id, server_id, kind, name, created_at, visibility, \
                parent_channel_id, root_message_id, title, slug, restricted \
         FROM channel WHERE id = $1 AND kind = 'thread'",
    )
    .bind(thread_id)
    .fetch_optional(executor)
    .await
}

/// One row of the public sitemap: every thread whose EFFECTIVE
/// visibility (its own override, else its server's) is `public`.
/// `unlisted` is deliberately excluded — reachable by direct link, invisible
/// to discovery by design.
#[derive(sqlx::FromRow)]
pub struct SitemapThreadRow {
    pub id: Uuid,
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

pub async fn list_public_threads(
    executor: impl PgExecutor<'_>,
) -> Result<Vec<SitemapThreadRow>, sqlx::Error> {
    sqlx::query_as::<_, SitemapThreadRow>(
        "SELECT c.id, c.slug, c.created_at \
         FROM channel c \
         JOIN server s ON s.id = c.server_id \
         JOIN channel parent ON parent.id = c.parent_channel_id \
         WHERE c.kind = 'thread' \
         AND coalesce(c.visibility, s.visibility) = 'public' \
         AND c.slug IS NOT NULL \
         -- A restricted parent channel's threads are never
         -- publicly readable regardless of their own visibility override
         -- (resolve_read_access enforces the same rule) — excluded from the
         -- sitemap for the same reason, not just from the read path itself.
         AND NOT parent.restricted \
         ORDER BY c.created_at DESC",
    )
    .fetch_all(executor)
    .await
}

/// Whether `slug` is already taken among `parent_channel_id`'s threads —
/// backs the partial unique index (`idx_channel_thread_slug`) with a clean
/// pre-check so a collision surfaces as a normal retry, not a raw
/// constraint-violation error.
pub async fn thread_slug_exists(
    executor: impl PgExecutor<'_>,
    parent_channel_id: Uuid,
    slug: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM channel \
         WHERE parent_channel_id = $1 AND kind = 'thread' AND slug = $2)",
    )
    .bind(parent_channel_id)
    .bind(slug)
    .fetch_one(executor)
    .await
}

/// The visibility-resolution inputs for one channel — its own
/// override, its server's visibility (if any), its kind, and its server id.
pub async fn visibility_context(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
) -> Result<Option<VisibilityContextRow>, sqlx::Error> {
    sqlx::query_as::<_, VisibilityContextRow>(
        "SELECT c.server_id, c.kind, c.visibility AS channel_visibility, \
                s.visibility AS server_visibility, \
                coalesce(parent.restricted, c.restricted) AS restricted, \
                c.parent_channel_id \
         FROM channel c \
         LEFT JOIN server s ON s.id = c.server_id \
         LEFT JOIN channel parent ON parent.id = c.parent_channel_id \
         WHERE c.id = $1",
    )
    .bind(channel_id)
    .fetch_optional(executor)
    .await
}

/// Sets (or clears, via `visibility: None`) a channel's visibility override.
/// Scoped to `server_id` too, not just `channel_id` — the caller
/// (`DomainService::update_channel_visibility`) already resolved `server_id`
/// from the URL path, and this doubles as confirmation the channel actually
/// belongs to that server rather than trusting the path alone. Returns
/// `None` if no row matched either id.
pub async fn update_visibility(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    server_id: Uuid,
    visibility: Option<&str>,
) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "UPDATE channel SET visibility = $1 \
         WHERE id = $2 AND server_id = $3 \
         RETURNING id, server_id, kind, name, created_at, visibility, \
                   parent_channel_id, root_message_id, title, slug, restricted",
    )
    .bind(visibility)
    .bind(channel_id)
    .bind(server_id)
    .fetch_optional(executor)
    .await
}

pub async fn find_access_by_id(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
) -> Result<Option<ChannelAccessRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelAccessRow>(
        "SELECT c.server_id, c.kind, \
                coalesce(parent.restricted, c.restricted) AS restricted, \
                c.parent_channel_id \
         FROM channel c \
         LEFT JOIN channel parent ON parent.id = c.parent_channel_id \
         WHERE c.id = $1",
    )
    .bind(channel_id)
    .fetch_optional(executor)
    .await
}

/// Sets or clears (`restricted: false`) whether a channel is
/// visibility-restricted at all. Meaningless on a `thread` row (its own
/// `restricted` column is never consulted, resolution always walks to
/// `parent_channel_id` — see `ChannelAccessRow`) but not rejected here; the
/// service layer is where that gets a real error if it matters.
pub async fn set_channel_restricted(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    server_id: Uuid,
    restricted: bool,
) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "UPDATE channel SET restricted = $1 \
         WHERE id = $2 AND server_id = $3 \
         RETURNING id, server_id, kind, name, created_at, visibility, \
                   parent_channel_id, root_message_id, title, slug, restricted",
    )
    .bind(restricted)
    .bind(channel_id)
    .bind(server_id)
    .fetch_optional(executor)
    .await
}

#[derive(sqlx::FromRow)]
struct RestrictedFlagRow {
    id: Uuid,
    restricted: bool,
}

/// RAW `restricted` values (not resolved through any parent) for
/// a batch of channel ids — `DomainService::list_channels`'s own way of
/// resolving a thread row's effective restriction (its parent's raw value)
/// without an N+1 query per thread in the list.
pub async fn restricted_flags_for(
    executor: impl PgExecutor<'_>,
    channel_ids: &[Uuid],
) -> Result<Vec<(Uuid, bool)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, RestrictedFlagRow>(
        "SELECT id, restricted FROM channel WHERE id = ANY($1)",
    )
    .bind(channel_ids)
    .fetch_all(executor)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.restricted)).collect())
}

/// Whether ANY of `role_ids` holds `bit` in `channel_id`'s grant
/// table — `channel_id` here is already the RESOLVED grant target (the
/// parent's id for a thread; the caller resolves that before calling this,
/// same "resolve in Rust, not SQL" split `resolve_read_access` already uses
/// for visibility).
pub async fn role_has_channel_permission(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    role_ids: &[Uuid],
    bit: i64,
) -> Result<bool, sqlx::Error> {
    if role_ids.is_empty() {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS(\
            SELECT 1 FROM channel_role_permission \
            WHERE channel_id = $1 AND role_id = ANY($2) AND permissions & $3 != 0\
         )",
    )
    .bind(channel_id)
    .bind(role_ids)
    .bind(bit)
    .fetch_one(executor)
    .await
}

/// Batched form of `role_has_channel_permission` — every id among
/// `channel_ids` for which ANY of `role_ids` holds `bit`, in one query
/// instead of one per channel. `DomainService::list_channels`'s own way of
/// resolving the grant check for every restricted channel in a listing up
/// front, same "batch resolve in Rust" split `restricted_flags_for` already
/// applies to the restriction flag itself.
pub async fn channel_ids_with_role_permission(
    executor: impl PgExecutor<'_>,
    channel_ids: &[Uuid],
    role_ids: &[Uuid],
    bit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    if channel_ids.is_empty() || role_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "SELECT DISTINCT channel_id FROM channel_role_permission \
         WHERE channel_id = ANY($1) AND role_id = ANY($2) AND permissions & $3 != 0",
    )
    .bind(channel_ids)
    .bind(role_ids)
    .bind(bit)
    .fetch_all(executor)
    .await
}

#[derive(sqlx::FromRow)]
pub struct ChannelRolePermissionRow {
    pub role_id: Uuid,
    pub permissions: i64,
}

/// Every explicit grant on one channel — the settings UI's read
/// path (which roles can see a restricted channel, and with what bits).
pub async fn channel_role_permissions_for(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
) -> Result<Vec<ChannelRolePermissionRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRolePermissionRow>(
        "SELECT role_id, permissions FROM channel_role_permission WHERE channel_id = $1",
    )
    .bind(channel_id)
    .fetch_all(executor)
    .await
}

/// Full replace, not a bit-flip — matches this codebase's existing
/// convention for permission bodies: role-assignment bodies are "the full
/// list, not a diff". `permissions: 0`
/// is indistinguishable from no grant at all for access-decision purposes,
/// but the row is still upserted (not deleted) so the settings UI has
/// something to show as "explicitly set to no access" versus "never
/// touched" — a real distinction for an admin auditing the list.
pub async fn upsert_channel_role_permission(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    role_id: Uuid,
    permissions: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO channel_role_permission (channel_id, role_id, permissions) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (channel_id, role_id) DO UPDATE SET permissions = EXCLUDED.permissions",
    )
    .bind(channel_id)
    .bind(role_id)
    .bind(permissions)
    .execute(executor)
    .await
    .map(|_| ())
}

/// Whether `account_id` holds a `membership` row for `server_id` — the
/// shared authorization check for every server/channel read or write, and
/// for the `text` branch of channel access.
pub async fn membership_exists(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    account_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM membership WHERE server_id = $1 AND account_id = $2)",
    )
    .bind(server_id)
    .bind(account_id)
    .fetch_one(executor)
    .await
}

/// Whether `account_id` holds a `channel_member` row for `channel_id` — the
/// `dm`/`group_dm` branch of channel access.
pub async fn channel_member_exists(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    account_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM channel_member WHERE channel_id = $1 AND account_id = $2)",
    )
    .bind(channel_id)
    .bind(account_id)
    .fetch_one(executor)
    .await
}

pub async fn insert_membership(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    server_id: Uuid,
    account_id: Uuid,
    role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO membership (id, server_id, account_id, role) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(server_id)
        .bind(account_id)
        .bind(role)
        .execute(executor)
        .await
        .map(|_| ())
}

/// A `membership` row's id and coarse `role` (`owner`|`member`) — the two
/// things M2's permission check needs beyond existence: `membership_role`
/// hangs off the id, and the owner-bypass check reads `role` exactly the way
/// `ServerSummary::from` already does.
#[derive(sqlx::FromRow)]
pub struct MembershipRow {
    pub id: Uuid,
    pub role: String,
    /// `Some(t)` where `t` is in the future means the member may
    /// not send messages, create threads, or join voice until it passes.
    pub timeout_until: Option<DateTime<Utc>>,
}

pub async fn find_membership(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    account_id: Uuid,
) -> Result<Option<MembershipRow>, sqlx::Error> {
    sqlx::query_as::<_, MembershipRow>(
        "SELECT id, role, timeout_until FROM membership WHERE server_id = $1 AND account_id = $2",
    )
    .bind(server_id)
    .bind(account_id)
    .fetch_optional(executor)
    .await
}

/// `until: None` clears an early timeout (and its stored reason
/// alongside it — a cleared timeout has nothing left to explain); `Some(t)`
/// sets/replaces both. Returns whether a row existed to update.
pub async fn set_member_timeout(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    account_id: Uuid,
    until: Option<DateTime<Utc>>,
    reason: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE membership SET timeout_until = $3, timeout_reason = $4 \
         WHERE server_id = $1 AND account_id = $2",
    )
    .bind(server_id)
    .bind(account_id)
    .bind(until)
    .bind(reason)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// `nickname: None` clears it back to "no override" (the account's
/// own `display_name` is what renders instead, at the caller's layer).
/// Returns whether a row existed to update.
pub async fn update_nickname(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    account_id: Uuid,
    nickname: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE membership SET nickname = $3 WHERE server_id = $1 AND account_id = $2",
    )
    .bind(server_id)
    .bind(account_id)
    .bind(nickname)
    .execute(executor)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Removes a member's `membership` row — the one step shared by leave, kick,
/// and ban (the shared `remove_member` step).
/// `membership_role` cascades automatically (`ON DELETE CASCADE`). Returns
/// whether a row actually existed, so a caller can distinguish "removed" from
/// "was already gone" without a separate existence check.
pub async fn delete_membership(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    account_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM membership WHERE server_id = $1 AND account_id = $2")
        .bind(server_id)
        .bind(account_id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Every `account_id` authorized to receive realtime events for a
/// `text`/`voice` channel — everyone with a `membership` row on its server.
pub async fn server_member_account_ids(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>("SELECT account_id FROM membership WHERE server_id = $1")
        .bind(server_id)
        .fetch_all(executor)
        .await
}

/// Every `account_id` authorized to receive realtime events for a
/// `dm`/`group_dm` channel — everyone with a `channel_member` row on it.
pub async fn channel_member_account_ids(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>("SELECT account_id FROM channel_member WHERE channel_id = $1")
        .bind(channel_id)
        .fetch_all(executor)
        .await
}

/// Every channel id `account_id` may receive realtime events for:
/// `text`/`voice`/`thread` channels via `membership` (a thread carries its
/// parent's `server_id` by construction, so this needs no separate
/// parent-walk), `dm`/`group_dm` channels via `channel_member`.
pub async fn accessible_channel_ids(
    executor: impl PgExecutor<'_>,
    account_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT c.id FROM channel c \
         JOIN membership m ON m.server_id = c.server_id \
         WHERE c.kind IN ('text','voice','thread') AND m.account_id = $1 \
         UNION \
         SELECT cm.channel_id FROM channel_member cm WHERE cm.account_id = $1",
    )
    .bind(account_id)
    .fetch_all(executor)
    .await
}

/// Every account that shares a server (via `membership`) or a dm/group_dm
/// (via `channel_member`) with `account_id` — the observer set for a
/// `presence.update` event. Set-based equivalent of iterating
/// `accessible_channel_ids` and unioning `authorized_account_ids` per
/// channel: that loop cost one query per accessible channel, and a thread
/// is a channel row, so it grew with every thread ever created
/// in every server the account belongs to. This never touches `channel` at
/// all, so channel/thread count stops mattering.
pub async fn observer_account_ids_for_account(
    executor: impl PgExecutor<'_>,
    account_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT m2.account_id FROM membership m1 \
         JOIN membership m2 ON m2.server_id = m1.server_id \
         WHERE m1.account_id = $1 \
         UNION \
         SELECT cm2.account_id FROM channel_member cm1 \
         JOIN channel_member cm2 ON cm2.channel_id = cm1.channel_id \
         WHERE cm1.account_id = $1",
    )
    .bind(account_id)
    .fetch_all(executor)
    .await
}
