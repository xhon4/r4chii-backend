use sqlx::PgExecutor;

/// Takes a transaction-scoped Postgres advisory lock keyed on `key`, released
/// automatically at `COMMIT`/`ROLLBACK`. Callers pick `key` so that
/// unordered pairs (e.g. two account ids) hash identically regardless of
/// argument order — see `domain::service`'s `dm:{low}:{high}` and
/// `friendship:{low}:{high}` key conventions.
pub async fn advisory_xact_lock(executor: impl PgExecutor<'_>, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(executor)
        .await
        .map(|_| ())
}
