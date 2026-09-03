use app_core::Uuid;
use sqlx::PgExecutor;

/// Whether an `account` row exists for `account_id`.
pub async fn exists(executor: impl PgExecutor<'_>, account_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account WHERE id = $1)")
        .bind(account_id)
        .fetch_one(executor)
        .await
}

/// Count of `account_ids` that actually exist — the caller compares this
/// against `account_ids.len()` to find any that don't.
pub async fn existing_count(
    executor: impl PgExecutor<'_>,
    account_ids: &[Uuid],
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM account WHERE id = ANY($1)")
        .bind(account_ids)
        .fetch_one(executor)
        .await
}
