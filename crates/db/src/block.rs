use app_core::Uuid;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// Plain `block` row from the blocker's point of view — `blocker_account_id`
/// is always the caller in every query that produces one, so it's not
/// carried here.
#[derive(sqlx::FromRow)]
pub struct BlockRow {
    pub id: Uuid,
    pub blocked_account_id: Uuid,
    pub created_at: DateTime<Utc>,
}

/// Inserts a `block` row, or does nothing if one already exists for this
/// exact (blocker, blocked) pair — idempotent via `ON CONFLICT DO NOTHING`.
/// `None` means it already existed; the caller should look it up with
/// `find_by_pair`.
pub async fn insert_or_conflict(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    blocker_account_id: Uuid,
    blocked_account_id: Uuid,
) -> Result<Option<BlockRow>, sqlx::Error> {
    sqlx::query_as::<_, BlockRow>(
        "INSERT INTO block (id, blocker_account_id, blocked_account_id) VALUES ($1, $2, $3) \
         ON CONFLICT (blocker_account_id, blocked_account_id) DO NOTHING \
         RETURNING id, blocked_account_id, created_at",
    )
    .bind(id)
    .bind(blocker_account_id)
    .bind(blocked_account_id)
    .fetch_optional(executor)
    .await
}

pub async fn find_by_pair(
    executor: impl PgExecutor<'_>,
    blocker_account_id: Uuid,
    blocked_account_id: Uuid,
) -> Result<BlockRow, sqlx::Error> {
    sqlx::query_as::<_, BlockRow>(
        "SELECT id, blocked_account_id, created_at FROM block \
         WHERE blocker_account_id = $1 AND blocked_account_id = $2",
    )
    .bind(blocker_account_id)
    .bind(blocked_account_id)
    .fetch_one(executor)
    .await
}

pub async fn list_for_account(
    executor: impl PgExecutor<'_>,
    account_id: Uuid,
) -> Result<Vec<BlockRow>, sqlx::Error> {
    sqlx::query_as::<_, BlockRow>(
        "SELECT id, blocked_account_id, created_at FROM block WHERE blocker_account_id = $1",
    )
    .bind(account_id)
    .fetch_all(executor)
    .await
}

/// Returns the number of rows deleted (0 or 1) so the caller can tell "there
/// was nothing to remove" from "removed".
pub async fn delete(
    executor: impl PgExecutor<'_>,
    blocker_account_id: Uuid,
    blocked_account_id: Uuid,
) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM block WHERE blocker_account_id = $1 AND blocked_account_id = $2")
        .bind(blocker_account_id)
        .bind(blocked_account_id)
        .execute(executor)
        .await
        .map(|result| result.rows_affected())
}

/// Every account that has blocked `blocked_account_id` — the reverse of
/// `list_for_account`. Used to mask `blocked_account_id`'s presence from its
/// blockers ("a block hides the blocked user's social presence from the
/// blocker"), directional exactly like the rest of this
/// table: blocking someone hides you from them, not the other way around.
pub async fn blockers_of(
    executor: impl PgExecutor<'_>,
    blocked_account_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT blocker_account_id FROM block WHERE blocked_account_id = $1",
    )
    .bind(blocked_account_id)
    .fetch_all(executor)
    .await
}

/// Whether either party has blocked the other — the shared, symmetric gate
/// for `create_dm`, `send_friend_request`, and `send_message` (dm channels).
pub async fn exists_either_direction(
    executor: impl PgExecutor<'_>,
    a: Uuid,
    b: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS( \
            SELECT 1 FROM block \
            WHERE (blocker_account_id = $1 AND blocked_account_id = $2) \
               OR (blocker_account_id = $2 AND blocked_account_id = $1) \
         )",
    )
    .bind(a)
    .bind(b)
    .fetch_one(executor)
    .await
}

/// Whether `blocker_account_id` has blocked `blocked_account_id` —
/// the directional counterpart to `exists_either_direction`, for callers
/// that must tell which side placed the block rather than just that one
/// exists.
pub async fn is_blocking(
    executor: impl PgExecutor<'_>,
    blocker_account_id: Uuid,
    blocked_account_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM block WHERE blocker_account_id = $1 AND blocked_account_id = $2)",
    )
    .bind(blocker_account_id)
    .bind(blocked_account_id)
    .fetch_one(executor)
    .await
}
