//! The integration tests here talk to the real Garage from the dev Compose
//! stack, the same way the `db` crate's tests talk to a real Postgres. They
//! skip themselves when the S3_* environment is absent, so a checkout with no
//! stack running still passes `cargo test` instead of failing for the wrong
//! reason.

use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};

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

fn one_response_server(response: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("listener has an address")
    );
    let response = response.to_owned();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let mut request = [0; 4096];
        stream.read(&mut request).expect("client sends a request");
        stream
            .write_all(response.as_bytes())
            .expect("server sends a response");
    });

    (endpoint, worker)
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
    assert!(
        url.contains("/test-bucket/avatars/some-key.jpg"),
        "got: {url}"
    );
    assert!(url.contains("X-Amz-Signature="), "got: {url}");
    assert!(url.contains("X-Amz-Expires=300"), "got: {url}");

    let escaped = StorageService::new(&config)
        .presigned_get("avatars/a space+#?.jpg", Duration::from_secs(300))
        .await
        .expect("signing escaped keys does not require the endpoint to exist");
    assert!(
        escaped.contains("/test-bucket/avatars/a%20space%2B%23%3F.jpg"),
        "object key must be escaped as one path segment per slash: {escaped}"
    );
}

#[tokio::test]
async fn presigned_get_rejects_durations_outside_sigv4_limits() {
    let config = StorageConfig {
        endpoint: "http://internal-only:3900".to_string(),
        public_endpoint: "https://public.example.net:8443".to_string(),
        region: "garage".to_string(),
        bucket: "test-bucket".to_string(),
        access_key_id: "GKtest".to_string(),
        secret_access_key: "secrettest".to_string(),
    };
    let storage = StorageService::new(&config);

    for duration in [
        Duration::ZERO,
        Duration::from_millis(1),
        Duration::from_secs(7 * 24 * 60 * 60 + 1),
    ] {
        assert!(matches!(
            storage
                .presigned_get("avatars/some-key.jpg", duration)
                .await,
            Err(storage::StorageError::InvalidDuration(_))
        ));
    }

    let maximum = storage
        .presigned_get(
            "avatars/some-key.jpg",
            Duration::from_secs(7 * 24 * 60 * 60),
        )
        .await
        .expect("seven days is the maximum valid SigV4 duration");
    assert!(maximum.contains("X-Amz-Expires=604800"), "got: {maximum}");
}

#[tokio::test]
async fn redirects_are_returned_as_put_errors() {
    let (endpoint, worker) = one_response_server(
        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:9/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let storage = StorageService::new(&StorageConfig {
        endpoint,
        public_endpoint: "https://public.example.net:8443".to_string(),
        region: "garage".to_string(),
        bucket: "test-bucket".to_string(),
        access_key_id: "GKtest".to_string(),
        secret_access_key: "secrettest".to_string(),
    });

    let result = storage
        .put_object("avatars/key.jpg", b"body".to_vec(), "image/jpeg")
        .await;
    worker.join().expect("test server completes");

    assert!(matches!(
        result,
        Err(storage::StorageError::Put(message)) if message == "request returned HTTP 307 Temporary Redirect"
    ));
}

#[tokio::test]
async fn put_and_delete_reject_non_success_statuses() {
    let (put_endpoint, put_worker) = one_response_server(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let put_storage = StorageService::new(&StorageConfig {
        endpoint: put_endpoint,
        public_endpoint: "https://public.example.net:8443".to_string(),
        region: "garage".to_string(),
        bucket: "test-bucket".to_string(),
        access_key_id: "GKtest".to_string(),
        secret_access_key: "secrettest".to_string(),
    });
    let put = put_storage
        .put_object("avatars/key.jpg", Vec::new(), "image/jpeg")
        .await;
    put_worker.join().expect("test server completes");
    assert!(matches!(
        put,
        Err(storage::StorageError::Put(message)) if message == "request returned HTTP 500 Internal Server Error"
    ));

    let (delete_endpoint, delete_worker) = one_response_server(
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let delete_storage = StorageService::new(&StorageConfig {
        endpoint: delete_endpoint,
        public_endpoint: "https://public.example.net:8443".to_string(),
        region: "garage".to_string(),
        bucket: "test-bucket".to_string(),
        access_key_id: "GKtest".to_string(),
        secret_access_key: "secrettest".to_string(),
    });
    let delete = delete_storage.delete_object("avatars/key.jpg").await;
    delete_worker.join().expect("test server completes");
    assert!(matches!(
        delete,
        Err(storage::StorageError::Delete(message)) if message == "request returned HTTP 403 Forbidden"
    ));
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

#[tokio::test]
async fn invalid_endpoints_and_debug_output_do_not_leak_configuration() {
    let config = StorageConfig {
        endpoint: "not a URL".to_string(),
        public_endpoint: "https://public.example.net:8443".to_string(),
        region: "garage".to_string(),
        bucket: "test-bucket".to_string(),
        access_key_id: "access-key".to_string(),
        secret_access_key: "secret-key".to_string(),
    };
    let error = StorageService::new(&config)
        .put_object("avatars/key.jpg", Vec::new(), "image/jpeg")
        .await
        .expect_err("invalid endpoints must fail before an HTTP request");

    assert_eq!(error.to_string(), "invalid storage endpoint configuration");
    let debug = format!("{config:?}");
    for value in ["not a URL", "access-key", "secret-key"] {
        assert!(
            !debug.contains(value),
            "debug output leaked {value:?}: {debug}"
        );
    }
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
        matches!(
            result,
            Err(storage::StorageError::MissingConfig("S3_BUCKET"))
        ),
        "a whitespace-only bucket must be rejected, got: {result:?}"
    );

    std::env::remove_var("S3_ENDPOINT");
    std::env::remove_var("S3_PUBLIC_ENDPOINT");
    std::env::remove_var("S3_BUCKET");
    std::env::remove_var("S3_ACCESS_KEY_ID");
    std::env::remove_var("S3_SECRET_ACCESS_KEY");
}
