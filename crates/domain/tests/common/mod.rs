//! Shared setup for the service tests: the services under test and the
//! account registration every suite needs before it can do anything.
//!
//! Helpers that phrase a domain action (creating a server, a channel, a
//! thread) stay in the file that uses them — their shapes differ per suite.
//!
//! A suite whose subject is the account itself may still want a different
//! account shape than [`register`] builds. Defining `register` locally in that
//! file shadows this one, which is the intended way to opt out.

// Each test binary pulls in this whole module and uses part of it.
#![allow(dead_code)]

use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::DomainService;
use test_support::TestDb;

/// The services under test, plus the pool for assertions that read the
/// database directly.
pub async fn test_services_with_pool() -> (DomainService, AuthService, db::PgPool, TestDb) {
    let test_db = test_support::test_db().await;
    let pool = test_db.pool();

    (
        DomainService::new(pool.clone()),
        AuthService::new(
            pool.clone(),
            std::sync::Arc::new(mailer::CaptureMailer::new()),
        ),
        pool,
        test_db,
    )
}

/// The same services, for suites that go through the service layer only.
pub async fn test_services() -> (DomainService, AuthService, TestDb) {
    let (domain, auth, _pool, test_db) = test_services_with_pool().await;
    (domain, auth, test_db)
}

pub fn register_input(email: &str, username: &str) -> RegisterInput {
    RegisterInput {
        email: email.to_string(),
        username: username.to_string(),
        password: "correct horse battery staple".to_string(),
        display_name: "Test User".to_string(),
    }
}

/// Creates a verified account directly, skipping the mail round trip these
/// suites have nothing to say about.
pub async fn register(auth: &AuthService, email: &str, username: &str) -> Uuid {
    auth.create_verified_account(register_input(email, username))
        .await
        .expect("registration succeeds")
        .id
}
