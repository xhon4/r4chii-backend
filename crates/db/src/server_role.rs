//! `server_role`, `membership_role`, and `server_ban` — M2's role/permission
//! model. `membership` itself (the `owner`/
//! `member` column, insert/delete) stays in `channel.rs`, where it already
//! lived before this feature existed.

use app_core::Uuid;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct ServerRoleRow {
    pub id: Uuid,
    pub server_id: Uuid,
    pub name: String,
    pub color: Option<String>,
    pub permissions: i64,
    pub position: i32,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    /// When `true`, ANY member may `@`-mention this role
    /// regardless of whether they hold `MENTION_ROLES` themselves.
    pub mentionable: bool,
}

pub async fn insert_role(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    server_id: Uuid,
    name: &str,
    position: i32,
) -> Result<ServerRoleRow, sqlx::Error> {
    sqlx::query_as::<_, ServerRoleRow>(
        "INSERT INTO server_role (id, server_id, name, position) \
         VALUES ($1, $2, $3, $4) \
         RETURNING id, server_id, name, color, permissions, position, is_default, created_at, mentionable",
    )
    .bind(id)
    .bind(server_id)
    .bind(name)
    .bind(position)
    .fetch_one(executor)
    .await
}

/// Inserts the implicit default role a new server gets alongside its owner
/// membership, in the same transaction as `create_server`
/// Never assigned via `membership_role` —
/// every member holds it implicitly.
pub async fn insert_default_role(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    server_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO server_role (id, server_id, name, position, is_default) \
         VALUES ($1, $2, 'everyone', 0, true)",
    )
    .bind(id)
    .bind(server_id)
    .execute(executor)
    .await
    .map(|_| ())
}

/// Scoped to `server_id` too, not just the role's own id — a role id from a
/// DIFFERENT server must 404 exactly like one that doesn't exist at all,
/// never confirm which server it belongs to.
pub async fn find_role(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    role_id: Uuid,
) -> Result<Option<ServerRoleRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerRoleRow>(
        "SELECT id, server_id, name, color, permissions, position, is_default, created_at, mentionable \
         FROM server_role WHERE server_id = $1 AND id = $2",
    )
    .bind(server_id)
    .bind(role_id)
    .fetch_optional(executor)
    .await
}

/// Highest `position` outranks lowest, matching the UI's top-to-bottom role
/// list.
pub async fn list_roles(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<Vec<ServerRoleRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerRoleRow>(
        "SELECT id, server_id, name, color, permissions, position, is_default, created_at, mentionable \
         FROM server_role WHERE server_id = $1 ORDER BY position DESC",
    )
    .bind(server_id)
    .fetch_all(executor)
    .await
}

pub async fn count_roles(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM server_role WHERE server_id = $1")
        .bind(server_id)
        .fetch_one(executor)
        .await
}

/// The position a newly-created role should start at: above every existing
/// role (Nerimity's own convention — a new role starts at the top, the owner
/// reorders it down). `0` (the default role's own position) if the server
/// somehow has no other roles yet.
pub async fn max_role_position(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<i32, sqlx::Error> {
    let max: Option<i32> =
        sqlx::query_scalar("SELECT MAX(position) FROM server_role WHERE server_id = $1")
            .bind(server_id)
            .fetch_one(executor)
            .await?;
    Ok(max.unwrap_or(0))
}

/// Full-row update — the service layer resolves "what should each field be"
/// (merging the caller's partial `PATCH` with the existing row) before this
/// is called, so this never has to model partial updates itself.
#[allow(clippy::too_many_arguments)]
pub async fn update_role(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    role_id: Uuid,
    name: &str,
    color: Option<&str>,
    permissions: i64,
    mentionable: bool,
) -> Result<Option<ServerRoleRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerRoleRow>(
        "UPDATE server_role SET name = $3, color = $4, permissions = $5, mentionable = $6 \
         WHERE server_id = $1 AND id = $2 \
         RETURNING id, server_id, name, color, permissions, position, is_default, created_at, mentionable",
    )
    .bind(server_id)
    .bind(role_id)
    .bind(name)
    .bind(color)
    .bind(permissions)
    .bind(mentionable)
    .fetch_optional(executor)
    .await
}

/// One `UPDATE` per role, not a bulk statement — role counts per server are
/// small (bounded by `RoleLimitReached`), and the service layer already runs
/// this inside one transaction, so atomicity doesn't depend on this being a
/// single query.
pub async fn set_role_position(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    role_id: Uuid,
    position: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE server_role SET position = $3 WHERE server_id = $1 AND id = $2")
        .bind(server_id)
        .bind(role_id)
        .bind(position)
        .execute(executor)
        .await
        .map(|_| ())
}

/// Returns whether a row existed to delete — the default role can never
/// reach this (the service layer rejects deleting it before this is called),
/// but a bad/already-gone id still needs to distinguish "deleted" from
/// "nothing to delete" for the caller.
pub async fn delete_role(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    role_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM server_role WHERE server_id = $1 AND id = $2")
        .bind(server_id)
        .bind(role_id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Every role a membership effectively holds: whatever is explicitly
/// assigned via `membership_role`, PLUS the server's default role — callers
/// never special-case the default, this query folds it in. Used by
/// `member_context` to compute the OR'd permission bitmask and the top
/// `position` in one round trip.
pub async fn roles_for_membership(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    membership_id: Uuid,
) -> Result<Vec<ServerRoleRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerRoleRow>(
        "SELECT id, server_id, name, color, permissions, position, is_default, created_at, mentionable \
         FROM server_role \
         WHERE server_id = $1 \
           AND (is_default OR id IN (SELECT role_id FROM membership_role WHERE membership_id = $2))",
    )
    .bind(server_id)
    .bind(membership_id)
    .fetch_all(executor)
    .await
}

/// The EXPLICITLY assigned role ids for one membership — unlike
/// `roles_for_membership`, this deliberately excludes the default role (every
/// member holds it implicitly; listing it on every member's response would
/// be redundant noise the client already knows). This is what
/// `ServerMemberResponse.role_ids` carries.
pub async fn role_ids_for_membership(
    executor: impl PgExecutor<'_>,
    membership_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>("SELECT role_id FROM membership_role WHERE membership_id = $1")
        .bind(membership_id)
        .fetch_all(executor)
        .await
}

#[derive(sqlx::FromRow)]
struct MemberRoleIdRow {
    account_id: Uuid,
    role_id: Uuid,
}

/// `(account_id, role_id)` pairs for every EXPLICIT role assignment across a
/// server's whole membership — bulk form of `role_ids_for_membership`, used
/// by `list_members` so rendering the roster is one extra query total, not
/// one per row.
pub async fn role_ids_for_server_members(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, MemberRoleIdRow>(
        "SELECT m.account_id, mr.role_id FROM membership_role mr \
         JOIN membership m ON m.id = mr.membership_id \
         WHERE m.server_id = $1",
    )
    .bind(server_id)
    .fetch_all(executor)
    .await?;
    Ok(rows.into_iter().map(|r| (r.account_id, r.role_id)).collect())
}

/// Clears a membership's full explicit role set — step one of "replace it",
/// step two is a loop of `insert_membership_role` for the new list. Split in
/// two rather than one delete-then-insert function because the service layer
/// needs to validate each new role id (existence, hierarchy) between the two
/// steps, inside the same transaction.
pub async fn clear_membership_roles(
    executor: impl PgExecutor<'_>,
    membership_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM membership_role WHERE membership_id = $1")
        .bind(membership_id)
        .execute(executor)
        .await
        .map(|_| ())
}

pub async fn insert_membership_role(
    executor: impl PgExecutor<'_>,
    membership_id: Uuid,
    role_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO membership_role (membership_id, role_id) VALUES ($1, $2)")
        .bind(membership_id)
        .bind(role_id)
        .execute(executor)
        .await
        .map(|_| ())
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct ServerBanRow {
    pub id: Uuid,
    pub server_id: Uuid,
    pub account_id: Uuid,
    pub banned_by: Uuid,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub async fn insert_ban(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    server_id: Uuid,
    account_id: Uuid,
    banned_by: Uuid,
    reason: Option<&str>,
) -> Result<ServerBanRow, sqlx::Error> {
    sqlx::query_as::<_, ServerBanRow>(
        "INSERT INTO server_ban (id, server_id, account_id, banned_by, reason) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, server_id, account_id, banned_by, reason, created_at",
    )
    .bind(id)
    .bind(server_id)
    .bind(account_id)
    .bind(banned_by)
    .bind(reason)
    .fetch_one(executor)
    .await
}

pub async fn is_banned(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    account_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM server_ban WHERE server_id = $1 AND account_id = $2)",
    )
    .bind(server_id)
    .bind(account_id)
    .fetch_one(executor)
    .await
}

pub async fn list_bans(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<Vec<ServerBanRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerBanRow>(
        "SELECT id, server_id, account_id, banned_by, reason, created_at \
         FROM server_ban WHERE server_id = $1 ORDER BY created_at DESC",
    )
    .bind(server_id)
    .fetch_all(executor)
    .await
}

pub async fn delete_ban(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    account_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM server_ban WHERE server_id = $1 AND account_id = $2")
        .bind(server_id)
        .bind(account_id)
        .execute(executor)
        .await?;
    Ok(result.rows_affected() > 0)
}
