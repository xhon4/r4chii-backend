use app_core::Uuid;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgExecutor};

/// One `export_job` row. `storage_key`/`download_url`/`error`
/// are `NULL` until the worker reaches a terminal state.
#[derive(sqlx::FromRow)]
pub struct ExportJobRow {
    pub id: Uuid,
    pub server_id: Uuid,
    pub requested_by: Uuid,
    pub status: String,
    pub storage_key: Option<String>,
    pub download_url: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

pub async fn insert_pending(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    server_id: Uuid,
    requested_by: Uuid,
) -> Result<ExportJobRow, sqlx::Error> {
    sqlx::query_as::<_, ExportJobRow>(
        "INSERT INTO export_job (id, server_id, requested_by) \
         VALUES ($1, $2, $3) \
         RETURNING id, server_id, requested_by, status, storage_key, download_url, \
                   error, created_at, completed_at",
    )
    .bind(id)
    .bind(server_id)
    .bind(requested_by)
    .fetch_one(executor)
    .await
}

/// Scoped to `server_id` too, not just `job_id` — a caller can only ever
/// reach a job through `/servers/{id}/exports/{job_id}`, and this doubles
/// as confirmation the job actually belongs to that server rather than
/// trusting the path alone (same shape as `db::channel::update_visibility`).
pub async fn find_for_server(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    job_id: Uuid,
) -> Result<Option<ExportJobRow>, sqlx::Error> {
    sqlx::query_as::<_, ExportJobRow>(
        "SELECT id, server_id, requested_by, status, storage_key, download_url, \
                error, created_at, completed_at \
         FROM export_job WHERE id = $1 AND server_id = $2",
    )
    .bind(job_id)
    .bind(server_id)
    .fetch_optional(executor)
    .await
}

/// Claims one `pending` job for processing, atomically. `FOR UPDATE SKIP
/// LOCKED` is the standard Postgres job-queue pattern with zero extra
/// infrastructure (no Redis, no queue system) — safe even if a
/// future scale-out runs more than one backend instance, since each
/// instance's poll loop simply skips rows another instance already locked.
/// The caller MUST commit or roll back `conn`'s transaction promptly to
/// release the lock.
pub async fn claim_next_pending(conn: &mut PgConnection) -> Result<Option<ExportJobRow>, sqlx::Error> {
    let job = sqlx::query_as::<_, ExportJobRow>(
        "SELECT id, server_id, requested_by, status, storage_key, download_url, \
                error, created_at, completed_at \
         FROM export_job \
         WHERE status = 'pending' \
         ORDER BY created_at ASC \
         LIMIT 1 \
         FOR UPDATE SKIP LOCKED",
    )
    .fetch_optional(&mut *conn)
    .await?;

    if let Some(job) = &job {
        sqlx::query("UPDATE export_job SET status = 'running' WHERE id = $1")
            .bind(job.id)
            .execute(&mut *conn)
            .await?;
    }

    Ok(job)
}

pub async fn mark_done(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    storage_key: &str,
    download_url: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE export_job \
         SET status = 'done', storage_key = $1, download_url = $2, completed_at = now() \
         WHERE id = $3",
    )
    .bind(storage_key)
    .bind(download_url)
    .bind(id)
    .execute(executor)
    .await
    .map(|_| ())
}

pub async fn mark_failed(executor: impl PgExecutor<'_>, id: Uuid, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE export_job SET status = 'failed', error = $1, completed_at = now() WHERE id = $2")
        .bind(error)
        .bind(id)
        .execute(executor)
        .await
        .map(|_| ())
}
