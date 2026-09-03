use app_core::Uuid;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// Plain `server` row, used where the caller's role is already known from
/// context (the creator is always `owner` in `create_server`; a joiner is
/// always `member` in `join_via_invite`) rather than needing a join.
#[derive(sqlx::FromRow)]
pub struct ServerRow {
    pub id: Uuid,
    pub owner_account_id: Uuid,
    pub name: String,
    pub icon_url: Option<String>,
    pub visibility: String,
    pub invite_code: String,
    pub created_at: DateTime<Utc>,
}

/// `server` row joined with the caller's `membership.role`, used by
/// `get_server`/`list_servers` to decide per-row whether `invite_code` is
/// visible (owner only — see `domain::types::ServerSummary::invite_code`).
#[derive(sqlx::FromRow)]
pub struct ServerWithRoleRow {
    pub id: Uuid,
    pub owner_account_id: Uuid,
    pub name: String,
    pub icon_url: Option<String>,
    pub visibility: String,
    pub invite_code: String,
    pub created_at: DateTime<Utc>,
    pub role: String,
}

pub async fn insert(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    owner_account_id: Uuid,
    name: &str,
    visibility: &str,
    invite_code: &str,
) -> Result<ServerRow, sqlx::Error> {
    sqlx::query_as::<_, ServerRow>(
        "INSERT INTO server (id, owner_account_id, name, visibility, invite_code) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, owner_account_id, name, icon_url, visibility, invite_code, created_at",
    )
    .bind(id)
    .bind(owner_account_id)
    .bind(name)
    .bind(visibility)
    .bind(invite_code)
    .fetch_one(executor)
    .await
}

pub async fn list_for_account(
    executor: impl PgExecutor<'_>,
    account_id: Uuid,
) -> Result<Vec<ServerWithRoleRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerWithRoleRow>(
        "SELECT s.id, s.owner_account_id, s.name, s.icon_url, s.visibility, \
                s.invite_code, s.created_at, m.role \
         FROM server s \
         JOIN membership m ON m.server_id = s.id \
         WHERE m.account_id = $1 \
         ORDER BY s.created_at ASC",
    )
    .bind(account_id)
    .fetch_all(executor)
    .await
}

pub async fn get_for_account(
    executor: impl PgExecutor<'_>,
    account_id: Uuid,
    server_id: Uuid,
) -> Result<Option<ServerWithRoleRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerWithRoleRow>(
        "SELECT s.id, s.owner_account_id, s.name, s.icon_url, s.visibility, \
                s.invite_code, s.created_at, m.role \
         FROM server s \
         JOIN membership m ON m.server_id = s.id \
         WHERE s.id = $1 AND m.account_id = $2",
    )
    .bind(server_id)
    .bind(account_id)
    .fetch_optional(executor)
    .await
}

pub async fn find_by_invite_code(
    executor: impl PgExecutor<'_>,
    invite_code: &str,
) -> Result<Option<ServerRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerRow>(
        "SELECT id, owner_account_id, name, icon_url, visibility, invite_code, created_at \
         FROM server WHERE invite_code = $1",
    )
    .bind(invite_code)
    .fetch_optional(executor)
    .await
}

/// Deletes a `server` row outright. Every dependent table (`channel`,
/// `membership`, and — M2 — `server_role`, `membership_role`, `server_ban`)
/// is `ON DELETE CASCADE` from `server` (confirmed against the actual DDL,
/// not assumed), so this one statement is the whole delete-server operation.
/// The caller (`DomainService::delete_server`)
/// has already verified ownership before this is reached.
pub async fn delete(executor: impl PgExecutor<'_>, server_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM server WHERE id = $1")
        .bind(server_id)
        .execute(executor)
        .await
        .map(|_| ())
}

/// Sets `server.visibility` outright — the caller
/// (`DomainService::update_server_visibility`) has already confirmed the
/// caller is the server's owner. Returns `None` if no such server exists.
pub async fn update_visibility(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    visibility: &str,
) -> Result<Option<ServerRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerRow>(
        "UPDATE server SET visibility = $1 WHERE id = $2 \
         RETURNING id, owner_account_id, name, icon_url, visibility, invite_code, created_at",
    )
    .bind(visibility)
    .bind(server_id)
    .fetch_optional(executor)
    .await
}

/// Replaces `server.invite_code` outright — the old code stops
/// resolving immediately (`find_by_invite_code` simply won't match it any
/// more), no grace period. Caller (`DomainService::regenerate_invite_code`)
/// has already checked `MANAGE_INVITES`.
pub async fn update_invite_code(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    invite_code: &str,
) -> Result<Option<ServerRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerRow>(
        "UPDATE server SET invite_code = $1 WHERE id = $2 \
         RETURNING id, owner_account_id, name, icon_url, visibility, invite_code, created_at",
    )
    .bind(invite_code)
    .bind(server_id)
    .fetch_optional(executor)
    .await
}

/// A server's member: their `membership.role` joined with the public-safe
/// columns of their `account` row. Deliberately no `email` — the
/// caller's-own-profile-only field (see `api`'s `AccountResponse` vs
/// `PublicAccountResponse` split); a member list is other people's profiles.
#[derive(sqlx::FromRow)]
pub struct ServerMemberRow {
    pub account_id: Uuid,
    pub username: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub role: String,
    pub joined_at: DateTime<Utc>,
    /// Per-server display override — `None` means the account's
    /// own `display_name` renders instead.
    pub nickname: Option<String>,
    /// `Some(t)` in the future means this member is currently
    /// timed out.
    pub timeout_until: Option<DateTime<Utc>>,
}

/// Every member of `server_id`, oldest membership first (so the owner, who
/// is always the first member, leads the list). Callers must have already
/// gated on the caller's own membership — this function does no
/// authorization of its own.
pub async fn list_members(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
) -> Result<Vec<ServerMemberRow>, sqlx::Error> {
    sqlx::query_as::<_, ServerMemberRow>(
        "SELECT a.id AS account_id, a.username, a.display_name, a.avatar_url, \
                m.role, m.joined_at, m.nickname, m.timeout_until \
         FROM membership m \
         JOIN account a ON a.id = m.account_id \
         WHERE m.server_id = $1 \
         ORDER BY m.joined_at ASC, a.username ASC",
    )
    .bind(server_id)
    .fetch_all(executor)
    .await
}
