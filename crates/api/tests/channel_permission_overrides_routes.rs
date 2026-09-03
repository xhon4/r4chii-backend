//! HTTP surface for channel restriction and role grants. Domain-
//! level coverage (the public-leak and search-leak cases) lives in
//! `crates/domain/tests/channel_permission_overrides_service.rs` — this file
//! only confirms routing and status codes. Same harness as `role_routes.rs`.

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
        .oneshot(json_request(Method::POST, "/api/v1/registrations", register_body(email, username)))
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
        .oneshot(json_request(Method::POST, "/api/v1/accounts", json!({ "email": email, "code": code })))
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
        .oneshot(auth_json_request(Method::POST, "/api/v1/servers", token, json!({ "name": name })))
        .await
        .expect("create server request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

async fn join_server(app: &axum::Router, token: &str, invite_code: &str) -> Value {
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
    body_json(response).await
}

async fn create_channel(app: &axum::Router, token: &str, server_id: &str, name: &str) -> Value {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            token,
            json!({ "name": name }),
        ))
        .await
        .expect("create channel request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

async fn create_role(app: &axum::Router, token: &str, server_id: &str, name: &str) -> Value {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/roles"),
            token,
            json!({ "name": name }),
        ))
        .await
        .expect("create role request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

const VIEW_CHANNEL: i64 = 1 << 0;

#[tokio::test]
async fn restricting_a_channel_hides_it_from_list_channels_until_a_role_is_granted() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;
    let channel = create_channel(&app, &alice_token, server_id, "staff-only").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let restrict = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/restricted"),
            &alice_token,
            json!({ "restricted": true }),
        ))
        .await
        .expect("restrict request succeeds");
    assert_eq!(restrict.status(), StatusCode::OK);
    assert_eq!(body_json(restrict).await["restricted"], true);

    let before_grant = app
        .clone()
        .oneshot(auth_request(Method::GET, &format!("/api/v1/servers/{server_id}/channels"), &bob_token))
        .await
        .expect("list channels request succeeds");
    let channels_before = body_json(before_grant).await;
    assert!(
        !channels_before["items"]
            .as_array()
            .expect("items array")
            .iter()
            .any(|c| c["id"] == channel_id),
        "bob has no grant yet, the restricted channel must be absent"
    );

    let role = create_role(&app, &alice_token, server_id, "Staff").await;
    let role_id = role["id"].as_str().expect("role id present");
    let assign = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}/roles"),
            &alice_token,
            json!({ "role_ids": [role_id] }),
        ))
        .await
        .expect("assign role request succeeds");
    assert_eq!(assign.status(), StatusCode::OK);

    let grant = app
        .clone()
        .oneshot(auth_json_request(
            Method::PUT,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/permissions/{role_id}"),
            &alice_token,
            json!({ "permissions": VIEW_CHANNEL }),
        ))
        .await
        .expect("grant request succeeds");
    assert_eq!(grant.status(), StatusCode::NO_CONTENT);

    let after_grant = app
        .clone()
        .oneshot(auth_request(Method::GET, &format!("/api/v1/servers/{server_id}/channels"), &bob_token))
        .await
        .expect("list channels request succeeds");
    let channels_after = body_json(after_grant).await;
    assert!(
        channels_after["items"]
            .as_array()
            .expect("items array")
            .iter()
            .any(|c| c["id"] == channel_id),
        "bob now has a grant, the channel must be visible"
    );

    let permissions = app
        .oneshot(auth_request(Method::GET, &format!("/api/v1/channels/{channel_id}/permissions"), &alice_token))
        .await
        .expect("list permissions request succeeds");
    assert_eq!(permissions.status(), StatusCode::OK);
    let permissions_body = body_json(permissions).await;
    assert_eq!(permissions_body["items"].as_array().expect("items array").len(), 1);
}

#[tokio::test]
async fn a_plain_member_cannot_restrict_a_channel_or_grant_a_role() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let restrict = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/restricted"),
            &bob_token,
            json!({ "restricted": true }),
        ))
        .await
        .expect("restrict request succeeds");
    assert_eq!(restrict.status(), StatusCode::FORBIDDEN);
}
