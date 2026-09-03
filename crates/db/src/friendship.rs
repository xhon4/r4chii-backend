use app_core::Uuid;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

/// Plain `friendship` row — always canonical (`account_low < account_high`),
/// so turning it into a caller-relative summary needs to know which side
/// the caller is on (done in `domain::service`).
#[derive(sqlx::FromRow)]
pub struct FriendshipRow {
    pub id: Uuid,
    pub account_low: Uuid,
    pub account_high: Uuid,
    pub status: String,
    pub requested_by: Uuid,
    pub created_at: DateTime<Utc>,
}

pub async fn find_by_pair(
    executor: impl PgExecutor<'_>,
    low: Uuid,
    high: Uuid,
) -> Result<Option<FriendshipRow>, sqlx::Error> {
    sqlx::query_as::<_, FriendshipRow>(
        "SELECT id, account_low, account_high, status, requested_by, created_at \
         FROM friendship WHERE account_low = $1 AND account_high = $2",
    )
    .bind(low)
    .bind(high)
    .fetch_optional(executor)
    .await
}

pub async fn insert_pending(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    low: Uuid,
    high: Uuid,
    requested_by: Uuid,
) -> Result<FriendshipRow, sqlx::Error> {
    sqlx::query_as::<_, FriendshipRow>(
        "INSERT INTO friendship (id, account_low, account_high, status, requested_by) \
         VALUES ($1, $2, $3, 'pending', $4) \
         RETURNING id, account_low, account_high, status, requested_by, created_at",
    )
    .bind(id)
    .bind(low)
    .bind(high)
    .bind(requested_by)
    .fetch_one(executor)
    .await
}

pub async fn accept(
    executor: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<FriendshipRow, sqlx::Error> {
    sqlx::query_as::<_, FriendshipRow>(
        "UPDATE friendship SET status = 'accepted' WHERE id = $1 \
         RETURNING id, account_low, account_high, status, requested_by, created_at",
    )
    .bind(id)
    .fetch_one(executor)
    .await
}

pub async fn list_for_account(
    executor: impl PgExecutor<'_>,
    account_id: Uuid,
) -> Result<Vec<FriendshipRow>, sqlx::Error> {
    sqlx::query_as::<_, FriendshipRow>(
        "SELECT id, account_low, account_high, status, requested_by, created_at \
         FROM friendship WHERE account_low = $1 OR account_high = $1",
    )
    .bind(account_id)
    .fetch_all(executor)
    .await
}

/// Deletes the canonical `friendship` row for the pair, whatever its status.
/// Returns the number of rows deleted (0 or 1) so the caller can tell "there
/// was nothing to remove" from "removed".
pub async fn delete_pair(
    executor: impl PgExecutor<'_>,
    low: Uuid,
    high: Uuid,
) -> Result<u64, sqlx::Error> {
    sqlx::query("DELETE FROM friendship WHERE account_low = $1 AND account_high = $2")
        .bind(low)
        .bind(high)
        .execute(executor)
        .await
        .map(|result| result.rows_affected())
}
