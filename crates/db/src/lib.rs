use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use thiserror::Error;

pub use sqlx::PgPool;

pub mod account;
pub mod block;
pub mod channel;
pub mod dm;
pub mod export;
pub mod friendship;
pub mod lock;
pub mod message;
pub mod server;
pub mod server_role;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("failed to connect to database: {0}")]
    Connect(#[source] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    #[error("query failed: {0}")]
    Query(#[source] sqlx::Error),
}

// Path is relative to this crate's Cargo.toml (crates/db/), so it climbs
// to the workspace root where the frozen `migrations/` directory lives.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn build_pool(database_url: &str) -> Result<PgPool, DbError> {
    PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(3))
        .connect(database_url)
        .await
        .map_err(DbError::Connect)
}

pub async fn run_migrations(pool: &PgPool) -> Result<(), DbError> {
    MIGRATOR.run(pool).await.map_err(DbError::Migrate)
}

pub async fn ping(pool: &PgPool) -> Result<(), DbError> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(DbError::Query)
}
