use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};

pub fn build_router(
    pool: db::PgPool,
    mailer: Arc<dyn mailer::Mailer>,
    client_dist_dir: Option<&str>,
) -> Router {
    let domain = domain::DomainService::new(pool.clone());
    let state = api::AppState {
        auth: auth::AuthService::new(pool.clone(), mailer),
        domain: domain.clone(),
        realtime: realtime::Hub::new(domain),
    };

    let router = Router::new()
        .route("/healthz", get(move || healthz(pool.clone())))
        .merge(api::router(state));

    // Serves the built web client for any GET that isn't an API route (a
    // client-side route like /app/servers/{id} has to fall back to
    // index.html so react-router can take over). Only wired when a build is
    // actually present — every test and a bare `cargo run` skip this and
    // expose the API only.
    match client_dist_dir {
        Some(dir) => {
            let index = Path::new(dir).join("index.html");
            // Plain `.fallback`, not `.not_found_service` — the latter forces
            // every fallback response to HTTP 404 (tower_http::set_status),
            // which would make every client-routed SPA path (e.g.
            // /app/servers/{id}) report 404 even though it's serving the
            // real index.html and react-router successfully takes over.
            router.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
        }
        None => router,
    }
}

/// Polls for one pending export job every 10s. No backoff/jitter — this
/// stack has no queue depth worth tuning against yet (single-digit accounts
/// today), and a fixed interval is the simplest thing that is correct. Runs
/// for the life of the process; a poll failure is logged and the loop
/// continues rather than exiting, since one bad tick (a transient DB blip)
/// should not stop every future export from ever processing.
async fn run_export_worker(domain: domain::DomainService, storage: storage::StorageService) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(10));
    loop {
        ticker.tick().await;
        if let Err(err) = domain.process_next_export_job(&storage).await {
            tracing::error!(error = %err, "export worker poll failed");
        }
    }
}

async fn healthz(pool: db::PgPool) -> (axum::http::StatusCode, Json<Value>) {
    match db::ping(&pool).await {
        Ok(()) => (axum::http::StatusCode::OK, Json(json!({ "status": "ok" }))),
        Err(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "unavailable" })),
        ),
    }
}

pub async fn run() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let _ = dotenvy::dotenv();
    let config = app_core::Config::from_env()?;

    let pool = db::build_pool(&config.database_url).await?;
    db::run_migrations(&pool).await?;

    // Refuse to start without mail rather than accepting registrations whose
    // verification code goes nowhere. A signup that can never be completed and
    // never explains why is worse than a server that will not boot, because
    // only one of the two tells the operator what is wrong.
    let smtp = config.smtp.ok_or(
        "SMTP_HOST is not set: registration cannot issue verification codes \
         (see .env.example)",
    )?;

    let mailer: Arc<dyn mailer::Mailer> = Arc::new(mailer::SmtpMailer::new(mailer::SmtpConfig {
        host: smtp.host,
        port: smtp.port,
        username: smtp.username,
        password: smtp.password,
        security: mailer::SmtpSecurity::parse(&smtp.security)?,
        from: smtp.from,
    })?);

    // The export worker is optional — a deployment without S3 configured
    // still starts and serves everything else; export jobs simply never
    // get processed until storage is configured. Real deployments already
    // run Garage, so this only matters for a bare `cargo run`/CI without a
    // `.env`.
    match storage::StorageService::from_env() {
        Ok(storage) => {
            let export_domain = domain::DomainService::new(pool.clone());
            tokio::spawn(run_export_worker(export_domain, storage));
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "S3 storage not configured — export jobs will not be processed"
            );
        }
    }

    let app = build_router(pool, mailer, config.client_dist_dir.as_deref());

    let listener = TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "server listening");
    // tower-governor's PeerIpKeyExtractor (used for the auth rate limits in
    // api::router) reads ConnectInfo<SocketAddr> from the request, which
    // only into_make_service_with_connect_info populates.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;

    Ok(())
}
