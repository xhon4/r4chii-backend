//! A real Postgres for integration tests, shared across the tests of one
//! binary.
//!
//! One container is started per test binary and one fresh database is handed
//! to each test, copied from a template the migrations were applied to once.
//! Tests stay isolated from each other because each owns a separate database,
//! not because each owns a separate server.
//!
//! The container is reference counted through the [`TestDb`] every test holds,
//! so it stops when the last test using it finishes. Nothing keeps it alive
//! past the run that started it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use sqlx::{AssertSqlSafe, Connection, PgConnection};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ContainerAsync, ImageExt},
};
use tokio::sync::Mutex;

/// Carries the migrated schema every test database is copied from.
const TEMPLATE_DB: &str = "r4chii_test_template";

struct Server {
    /// Connection string for the `postgres` maintenance database, used to
    /// issue `CREATE DATABASE`.
    admin_url: String,
    _container: ContainerAsync<Postgres>,
}

/// Weak on purpose: the strong references live in the [`TestDb`] values tests
/// hold, so the container is dropped once none of them remain.
fn server_slot() -> &'static Mutex<Weak<Server>> {
    static SLOT: OnceLock<Mutex<Weak<Server>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Weak::new()))
}

static NEXT_DB: AtomicU32 = AtomicU32::new(0);

/// The running server, starting one if none is currently held.
///
/// The lock is held across the whole startup so that concurrent tests wait for
/// a single container rather than racing to start one each.
async fn server() -> Arc<Server> {
    let mut slot = server_slot().lock().await;

    if let Some(running) = slot.upgrade() {
        return running;
    }

    let container = Postgres::default()
        // postgres:16, the tag production runs. The crate default is
        // 11-alpine: five majors and a different libc away from the database
        // this schema is deployed on.
        .with_tag("16")
        .start()
        .await
        .expect("postgres container starts");

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let admin_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    create_database(&admin_url, TEMPLATE_DB, None).await;

    // The migrations run once, here. The pool is closed before any copy is
    // taken: Postgres refuses a template that still has a session attached.
    let template_url = format!("postgres://postgres:postgres@{host}:{port}/{TEMPLATE_DB}");
    let template_pool = db::build_pool(&template_url)
        .await
        .expect("template pool connects");
    db::run_migrations(&template_pool)
        .await
        .expect("migrations run");
    template_pool.close().await;

    let server = Arc::new(Server {
        admin_url,
        _container: container,
    });
    *slot = Arc::downgrade(&server);

    server
}

/// Issues one `CREATE DATABASE` over its own connection.
///
/// Raw SQL rather than a bound query: a database name cannot be a bind
/// parameter, and `CREATE DATABASE` cannot run through the extended protocol a
/// bound query would use.
///
/// `AssertSqlSafe` is answered by the callers, not waved past: the only names
/// reaching here are [`TEMPLATE_DB`] and the `r4chii_test_{n}` this module
/// builds from a counter. No caller-supplied value is interpolated.
async fn create_database(admin_url: &str, name: &str, template: Option<&str>) {
    let mut conn = PgConnection::connect(admin_url)
        .await
        .expect("admin connection opens");

    let statement = match template {
        Some(template) => format!(r#"CREATE DATABASE "{name}" TEMPLATE "{template}""#),
        None => format!(r#"CREATE DATABASE "{name}""#),
    };

    sqlx::raw_sql(AssertSqlSafe(statement))
        .execute(&mut conn)
        .await
        .expect("database is created");

    conn.close().await.expect("admin connection closes");
}

/// One test's own database, and the handle keeping the shared container up.
///
/// Hold this for the whole test. Dropping it releases the database and, once
/// no test holds one, the container.
pub struct TestDb {
    pool: db::PgPool,
    _server: Arc<Server>,
}

impl TestDb {
    /// A pool onto this test's database. Cloning a pool is cheap and every
    /// clone reaches the same database.
    pub fn pool(&self) -> db::PgPool {
        self.pool.clone()
    }
}

/// A fresh, fully migrated database of this test's own.
pub async fn test_db() -> TestDb {
    let server = server().await;

    let name = format!("r4chii_test_{}", NEXT_DB.fetch_add(1, Ordering::Relaxed));
    create_database(&server.admin_url, &name, Some(TEMPLATE_DB)).await;

    let base = server
        .admin_url
        .rsplit_once('/')
        .expect("the admin url names a database")
        .0;
    let pool = db::build_pool(&format!("{base}/{name}"))
        .await
        .expect("test pool connects");

    TestDb {
        pool,
        _server: server,
    }
}
