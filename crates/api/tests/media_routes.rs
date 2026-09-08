//! Avatar and banner upload, and the route that serves them.
//!
//! These run without object storage configured, which is what `AppState`
//! carries in every test harness here. That covers authentication, the
//! validation of an upload, and the ordering between the two — but the write
//! itself and the signed redirect are only exercised against a live
//! S3-compatible endpoint, which this suite does not stand up.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, Method, Request, StatusCode},
};
use tower::ServiceExt;

mod common;
use common::*;

/// A one pixel PNG. Inline rather than encoded here so these tests need no
/// image library of their own.
const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 224, 170, 56, 1, 0, 1, 218,
    1, 75, 16, 251, 241, 0, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

const BOUNDARY: &str = "boundarytestboundary";

fn multipart_body(bytes: &[u8]) -> Body {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"upload.png\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Body::from(body)
}

fn upload_request(uri: &str, token: Option<&str>, bytes: &[u8]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .extension(ConnectInfo(next_peer_addr()))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        );
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(multipart_body(bytes)).expect("request builds")
}

#[tokio::test]
async fn uploading_without_a_session_is_unauthenticated() {
    let (app, _mail, _container) = test_app().await;

    for uri in [
        "/api/v1/accounts/me/avatar",
        "/api/v1/accounts/me/banner",
    ] {
        let response = app
            .clone()
            .oneshot(upload_request(uri, None, TINY_PNG))
            .await
            .expect("request succeeds");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

/// The stored path is what a profile carries, so leaving this route open would
/// turn a path that leaked into standing access to the object behind it —
/// which is the whole reason the storage URL is signed and short-lived.
#[tokio::test]
async fn serving_media_without_a_session_is_unauthenticated() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/media/avatars/some-account/some-object.jpg")
                .extension(ConnectInfo(next_peer_addr()))
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_upload_that_is_not_an_image_is_rejected() {
    let (app, mail, _container) = test_app().await;
    let (_account_id, token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let response = app
        .oneshot(upload_request(
            "/api/v1/accounts/me/avatar",
            Some(&token),
            b"this is not a png, whatever the filename says",
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["error"]["code"], "invalid_image");
}

/// A malformed upload is the caller's error and is reported as one whether or
/// not this instance has storage configured. Reaching the storage check is
/// also what shows the image itself was accepted.
#[tokio::test]
async fn a_valid_upload_reaches_storage_and_reports_it_missing() {
    let (app, mail, _container) = test_app().await;
    let (_account_id, token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(upload_request(
            "/api/v1/accounts/me/avatar",
            Some(&token),
            TINY_PNG,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "storage_unavailable"
    );
}

#[tokio::test]
async fn a_media_key_cannot_escape_its_prefix() {
    let (app, mail, _container) = test_app().await;
    let (_account_id, token) = register_and_login(&app, &mail, "carol@example.com", "carol").await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/media/avatars/../../etc/passwd")
                .extension(ConnectInfo(next_peer_addr()))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
