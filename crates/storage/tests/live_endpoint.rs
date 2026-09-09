//! Exercises the storage path against a real S3-compatible endpoint.
//!
//! Ignored by default. It needs a running S3-compatible service and the `S3_*`
//! environment that names it, and a plain checkout has neither, so leaving it
//! in the default run would make the suite fail for a missing service rather
//! than for a defect.
//!
//! What it covers that the rest of the suite cannot: `put_object` and
//! `presigned_get` are the two operations whose correctness lives entirely in
//! what the remote service accepts. A signature the service rejects, a
//! path-style setting it disagrees with, or a host baked into the signature
//! that does not match the one the url is fetched from all produce a valid
//! `Result::Ok` here and a 403 at the browser.
//!
//! Run it with the deployment's own configuration loaded, pointing
//! `S3_ENDPOINT` at wherever the service is reachable from the test host:
//!
//! ```sh
//! set -a; . ./.env; set +a
//! S3_ENDPOINT=http://127.0.0.1:3900 \
//!   cargo test -p storage --test live_endpoint -- --ignored --nocapture
//! ```

use std::time::Duration;

/// The longest expiry any caller asks for — the export's download url. Signed
/// here rather than something shorter because an expiry the service refuses is
/// a failure the caller cannot tell apart from a bad key, and a value that
/// works for a day works for a minute.
const LONGEST_PRODUCTION_EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);

use storage::StorageService;

/// Unique per run, so a failed run never leaves a key that makes the next one
/// pass for the wrong reason.
fn test_key() -> String {
    format!(
        "test/live-endpoint/{}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after the unix epoch")
            .as_nanos()
    )
}

#[tokio::test]
#[ignore = "needs a live S3-compatible endpoint and the S3_* environment"]
async fn an_object_round_trips_and_stays_private() {
    let storage = StorageService::from_env().expect("the S3_* environment must be complete");
    let key = test_key();
    let body = b"round trip".to_vec();

    storage
        .put_object(&key, body.clone(), "text/plain")
        .await
        .expect("the endpoint must accept the upload");

    let url = storage
        .presigned_get(&key, LONGEST_PRODUCTION_EXPIRY)
        .await
        .expect("signing must succeed");

    // The signed url has to work from where a browser would fetch it, which
    // is the whole reason `public_endpoint` exists as a separate setting.
    let signed = reqwest::get(&url)
        .await
        .expect("the presigned url must be reachable");
    assert_eq!(
        signed.status(),
        200,
        "presigned GET was rejected: {}",
        signed.text().await.unwrap_or_default()
    );
    assert_eq!(
        signed
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain"),
        "the uploaded content type must be preserved"
    );
    assert_eq!(
        signed
            .bytes()
            .await
            .expect("a body must come back")
            .to_vec(),
        body,
        "the bytes that came back are not the bytes that went in"
    );

    // The bucket being private is what makes the signature worth anything: the
    // same url without its query string must be refused.
    let unsigned_url = url.split('?').next().expect("a url has a path");
    let unsigned = reqwest::get(unsigned_url)
        .await
        .expect("the endpoint must answer an unsigned request");
    assert!(
        unsigned.status().is_client_error(),
        "an unsigned GET returned {} — the bucket is not private",
        unsigned.status()
    );

    storage
        .delete_object(&key)
        .await
        .expect("the endpoint must accept the delete");

    let after_delete = storage
        .presigned_get(&key, LONGEST_PRODUCTION_EXPIRY)
        .await
        .expect("signing a missing key is still just arithmetic");
    assert_eq!(
        reqwest::get(&after_delete)
            .await
            .expect("the endpoint must answer")
            .status(),
        404,
        "the object survived its delete"
    );
}
