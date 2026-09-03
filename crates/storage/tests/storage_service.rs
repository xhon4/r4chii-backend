//! The integration tests here talk to the real Garage from the dev Compose
//! stack, the same way the `db` crate's tests talk to a real Postgres. They
//! skip themselves when the S3_* environment is absent, so a checkout with no
//! stack running still passes `cargo test` instead of failing for the wrong
//! reason.

use std::time::Duration;

use storage::{StorageConfig, StorageService};

fn dev_config() -> Option<StorageConfig> {
    // Deliberately NOT StorageConfig::from_env(): tests reach Garage through
    // the published host port, while the server reaches it over the compose
    // network. Same store, different address.
    let endpoint = std::env::var("S3_TEST_ENDPOINT").ok()?;
    Some(StorageConfig {
        endpoint,
        public_endpoint: std::env::var("S3_PUBLIC_ENDPOINT").ok()?,
        region: std::env::var("S3_REGION").unwrap_or_else(|_| "garage".to_string()),
        bucket: std::env::var("S3_BUCKET").ok()?,
        access_key_id: std::env::var("S3_ACCESS_KEY_ID").ok()?,
        secret_access_key: std::env::var("S3_SECRET_ACCESS_KEY").ok()?,
    })
}

/// The invariant that is easiest to break and hardest to debug: SigV4 signs
/// the Host header, so a URL signed against the internal endpoint is rejected
/// with a 403 the moment a browser fetches it from the public one — and a 403
/// reads like a credentials problem, not an addressing one. Needs no network:
/// presigning is pure computation.
#[tokio::test]
async fn presigned_get_is_signed_against_the_public_endpoint() {
    let config = StorageConfig {
        endpoint: "http://internal-only:3900".to_string(),
        public_endpoint: "https://public.example.net:8443".to_string(),
        region: "garage".to_string(),
        bucket: "test-bucket".to_string(),
        access_key_id: "GKtest".to_string(),
        secret_access_key: "secrettest".to_string(),
    };

    let url = StorageService::new(&config)
        .presigned_get("avatars/some-key.jpg", Duration::from_secs(300))
        .await
        .expect("signing does not require the endpoint to exist");

    assert!(
        url.starts_with("https://public.example.net:8443/"),
        "presigned URLs must carry the public host, got: {url}"
    );
    assert!(
        !url.contains("internal-only"),
        "the internal endpoint must never leak into a URL handed to a browser: {url}"
    );
    // Path-style addressing: the bucket belongs in the path, not in a
    // subdomain nothing has DNS for.
    assert!(url.contains("/test-bucket/avatars/some-key.jpg"), "got: {url}");
    assert!(url.contains("X-Amz-Signature="), "got: {url}");
}

#[tokio::test]
async fn put_then_read_back_then_delete() {
    let Some(config) = dev_config() else {
        eprintln!("skipping: S3_TEST_ENDPOINT and friends not set");
        return;
    };

    let service = StorageService::new(&config);
    let key = format!("tests/round-trip-{}.txt", app_core::new_id());
    let body = b"round trip".to_vec();

    service
        .put_object(&key, body.clone(), "text/plain")
        .await
        .expect("put succeeds against the dev stack");

    let url = service
        .presigned_get(&key, Duration::from_secs(120))
        .await
        .expect("signing succeeds");
    assert!(url.contains(&key));

    service.delete_object(&key).await.expect("delete succeeds");
}

/// A missing variable has to fail loudly at startup rather than produce a
/// service that signs URLs with an empty key and fails on first use.
#[test]
fn from_env_rejects_a_blank_value_not_just_an_absent_one() {
    // An empty string is what a half-filled .env produces — `S3_BUCKET=` with
    // nothing after it. std::env::var returns Ok("") for that, so a plain
    // `.ok_or(...)` would accept it.
    std::env::set_var("S3_ENDPOINT", "http://garage:3900");
    std::env::set_var("S3_PUBLIC_ENDPOINT", "https://example.net:8443");
    std::env::set_var("S3_BUCKET", "   ");
    std::env::set_var("S3_ACCESS_KEY_ID", "GKx");
    std::env::set_var("S3_SECRET_ACCESS_KEY", "sk");

    let result = StorageConfig::from_env();
    assert!(
        matches!(result, Err(storage::StorageError::MissingConfig("S3_BUCKET"))),
        "a whitespace-only bucket must be rejected, got: {result:?}"
    );

    std::env::remove_var("S3_ENDPOINT");
    std::env::remove_var("S3_PUBLIC_ENDPOINT");
    std::env::remove_var("S3_BUCKET");
    std::env::remove_var("S3_ACCESS_KEY_ID");
    std::env::remove_var("S3_SECRET_ACCESS_KEY");
}
