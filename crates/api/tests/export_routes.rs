//! Full export HTTP surface — `POST`/`GET .../servers/{id}/exports`.
//! Same harness as `role_routes.rs`. Only covers job creation/polling — the
//! worker itself needs real S3-compatible storage, which this sandbox does
//! not have (see `crates/domain/tests/export_service.rs`'s own note).

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

async fn test_app() -> (
    axum::Router,
    mailer::CaptureMailer,
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        // postgres:16, the tag production runs (docker-compose.yml).
        // The crate default is 11-alpine: five majors and a different
        // libc away from the database this schema is deployed on.
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

fn request(method: Method, uri: &str) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(next_peer_addr()))
}

fn json_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    request(method, uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds")
}

fn auth_json_request(method: Method, uri: &str, token: &str, body: Value) -> Request<Body> {
    request(method, uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("request builds")
}

fn auth_request(method: Method, uri: &str, token: &str) -> Request<Body> {
    request(method, uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builds")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.expect("body collects").to_bytes();
    serde_json::from_slice(&bytes).expect("body is valid JSON")
}

fn register_body(email: &str, username: &str) -> Value {
    json!({
        "email": email,
        "username": username,
        "password": "correct horse battery staple",
        "display_name": "Test User",
    })
}

fn login_body(email: &str) -> Value {
    json!({ "email": email, "password": "correct horse battery staple" })
}

async fn register_and_login(
    app: &axum::Router,
    mail: &mailer::CaptureMailer,
    email: &str,
    username: &str,
) -> (String, String) {
    let start = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/registrations",
            register_body(email, username),
        ))
        .await
        .expect("registration request succeeds");
    assert_eq!(start.status(), StatusCode::ACCEPTED);

    let code = mail
        .last()
        .expect("a verification mail was sent")
        .body
        .split_whitespace()
        .find(|word| word.len() == 8 && word.chars().all(|c| c.is_ascii_digit()))
        .expect("the mail carries an 8-digit code")
        .to_string();

    let verify_response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/accounts",
            json!({ "email": email, "code": code }),
        ))
        .await
        .expect("verification request succeeds");
    assert_eq!(verify_response.status(), StatusCode::CREATED);
    let account_body = body_json(verify_response).await;
    let account_id = account_body["id"].as_str().expect("account id present").to_string();

    let login_response = app
        .clone()
        .oneshot(json_request(Method::POST, "/api/v1/sessions", login_body(email)))
        .await
        .expect("login request succeeds");
    let login_body = body_json(login_response).await;
    let token = login_body["token"].as_str().expect("token present").to_string();

    (account_id, token)
}

async fn create_server(app: &axum::Router, token: &str, name: &str) -> Value {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/servers",
            token,
            json!({ "name": name }),
        ))
        .await
        .expect("create server request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

async fn join_server(app: &axum::Router, token: &str, invite_code: &str) {
    let response = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            token,
        ))
        .await
        .expect("join request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn the_owner_can_request_and_poll_an_export() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let create_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/exports"),
            &alice_token,
            json!({}),
        ))
        .await
        .expect("request export succeeds");
    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let job = body_json(create_response).await;
    assert_eq!(job["status"], "pending");
    assert_eq!(job["download_url"], Value::Null);
    let job_id = job["id"].as_str().expect("job id present");

    let poll_response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/exports/{job_id}"),
            &alice_token,
        ))
        .await
        .expect("poll export succeeds");
    assert_eq!(poll_response.status(), StatusCode::OK);
    let polled = body_json(poll_response).await;
    assert_eq!(polled["id"], job_id);
    assert_eq!(polled["status"], "pending");
}

#[tokio::test]
async fn a_plain_member_without_admin_cannot_request_an_export() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/exports"),
            &bob_token,
            json!({}),
        ))
        .await
        .expect("request export succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "missing_permission");
}

#[tokio::test]
async fn polling_a_nonexistent_export_job_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let fake_job_id = app_core::new_id();

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/exports/{fake_job_id}"),
            &alice_token,
        ))
        .await
        .expect("poll export succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "export_job_not_found");
}
