//! Full export. Same harness as `domain_service.rs`. The worker
//! path (`process_next_export_job`) needs real S3-compatible storage and
//! skips when `S3_TEST_ENDPOINT` isn't set — same convention
//! `crates/storage/tests/storage_service.rs` already established.

use domain::{CreateServerInput, DomainError};
use storage::{StorageConfig, StorageService};

mod common;
use common::*;

// Same convention as crates/storage/tests/storage_service.rs's own
// `dev_config()` — tests reach the store through the published host port.
fn dev_storage() -> Option<StorageService> {
    let endpoint = std::env::var("S3_TEST_ENDPOINT").ok()?;
    Some(StorageService::new(&StorageConfig {
        endpoint,
        public_endpoint: std::env::var("S3_PUBLIC_ENDPOINT").ok()?,
        region: std::env::var("S3_REGION").unwrap_or_else(|_| "garage".to_string()),
        bucket: std::env::var("S3_BUCKET").ok()?,
        access_key_id: std::env::var("S3_ACCESS_KEY_ID").ok()?,
        secret_access_key: std::env::var("S3_SECRET_ACCESS_KEY").ok()?,
    }))
}

#[tokio::test]
async fn the_owner_can_request_an_export_and_poll_its_pending_status() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");

    let job = domain
        .request_export(alice, server.id)
        .await
        .expect("owner can request an export");
    assert_eq!(job.status, "pending");
    assert_eq!(job.download_url, None);

    let polled = domain
        .get_export_job(alice, server.id, job.id)
        .await
        .expect("owner can poll the job");
    assert_eq!(polled.id, job.id);
    assert_eq!(polled.status, "pending");
}

#[tokio::test]
async fn a_plain_member_without_admin_cannot_request_an_export() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    domain
        .join_via_invite(bob, &server.invite_code.clone().expect("owner sees invite code"))
        .await
        .expect("bob joins");

    let result = domain.request_export(bob, server.id).await;
    assert!(matches!(result, Err(DomainError::MissingPermission)));
}

#[tokio::test]
async fn polling_a_job_from_a_different_server_is_not_found() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server_a = domain
        .create_server(alice, CreateServerInput { name: "Server A".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let server_b = domain
        .create_server(alice, CreateServerInput { name: "Server B".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");

    let job = domain
        .request_export(alice, server_a.id)
        .await
        .expect("owner can request an export");

    let result = domain.get_export_job(alice, server_b.id, job.id).await;
    assert!(matches!(result, Err(DomainError::ExportJobNotFound)));
}

#[tokio::test]
async fn the_worker_processes_a_pending_job_end_to_end_against_real_storage() {
    let Some(storage) = dev_storage() else {
        eprintln!("skipping: S3_TEST_ENDPOINT and friends not set");
        return;
    };

    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let job = domain
        .request_export(alice, server.id)
        .await
        .expect("owner can request an export");

    let claimed = domain
        .process_next_export_job(&storage)
        .await
        .expect("worker poll succeeds");
    assert!(claimed, "the pending job should have been claimed");

    let polled = domain
        .get_export_job(alice, server.id, job.id)
        .await
        .expect("owner can poll the job");
    assert_eq!(polled.status, "done");
    assert!(polled.download_url.is_some());

    let idle = domain
        .process_next_export_job(&storage)
        .await
        .expect("worker poll succeeds");
    assert!(!idle, "no pending job should remain");
}
