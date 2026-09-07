//! Avatar and banner upload, and the route that serves them.
//!
//! These run without object storage configured, which is what `AppState`
//! carries in every test harness here. That covers authentication, the
//! validation of an upload, and the ordering between the two — but the write
//! itself and the signed redirect are only exercised against a live
//! S3-compatible endpoint, which this suite does not stand up.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};
use tower::ServiceExt;

/// A one pixel PNG. Inline rather than encoded here so these tests need no
/// image library of their own.
const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 224, 170, 56, 1, 0, 1, 218,
    1, 75, 16, 251, 241, 0, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

const BOUNDARY: &str = "boundarytestboundary";

async fn test_app() -> (
    axum::Router,
    mailer::CaptureMailer,
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .with_tag("16")
        .start()
        .await
        .expect("postgres container starts");

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = db::build_pool(&database_url).await.expect("pool connects");
    db::run_migrations(&pool).await.expect("migrations run");

    let mail = mailer::CaptureMailer::new();
    let domain = domain::DomainService::new(pool.clone());
    let state = api::AppState {
        auth: auth::AuthService::new(pool, std::sync::Arc::new(mail.clone())),
        domain: domain.clone(),
        realtime: realtime::Hub::new(domain),
        storage: None,
    };

    (api::router(state), mail, container)
}

static NEXT_IP_OCTETS: AtomicU32 = AtomicU32::new(1);

fn next_peer_addr() -> SocketAddr {
    let n = NEXT_IP_OCTETS.fetch_add(1, Ordering::Relaxed);
    let ip = Ipv4Addr::new(10, (n >> 16) as u8, (n >> 8) as u8, n as u8);
    SocketAddr::new(IpAddr::V4(ip), 0)
}

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

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("body is valid JSON")
}

async fn register_and_login(
    app: &axum::Router,
    mail: &mailer::CaptureMailer,
    email: &str,
    username: &str,
) -> String {
    let json_request = |uri: &str, body: Value| {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .extension(ConnectInfo(next_peer_addr()))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request builds")
    };

    app.clone()
        .oneshot(json_request(
            "/api/v1/registrations",
            json!({
                "email": email,
                "username": username,
                "password": "correct horse battery staple",
                "display_name": "Test User",
            }),
        ))
        .await
        .expect("registration request succeeds");

    let code = mail
        .last()
        .expect("a verification mail was sent")
        .body
        .split_whitespace()
        .find(|word| word.len() == 8 && word.chars().all(|c| c.is_ascii_digit()))
        .expect("the mail carries an 8-digit code")
        .to_string();

    app.clone()
        .oneshot(json_request(
            "/api/v1/accounts",
            json!({ "email": email, "code": code }),
        ))
        .await
        .expect("verification request succeeds");

    let login = app
        .clone()
        .oneshot(json_request(
            "/api/v1/sessions",
            json!({ "email": email, "password": "correct horse battery staple" }),
        ))
        .await
        .expect("login request succeeds");

    body_json(login).await["token"]
        .as_str()
        .expect("token present")
        .to_string()
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
    let token = register_and_login(&app, &mail, "alice@example.com", "alice").await;

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
    let token = register_and_login(&app, &mail, "bob@example.com", "bob").await;

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
    let token = register_and_login(&app, &mail, "carol@example.com", "carol").await;

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
