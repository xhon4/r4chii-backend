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

    let pool = db::build_pool(&database_url)
        .await
        .expect("pool connects");
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
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
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
    json!({
        "email": email,
        "password": "correct horse battery staple",
    })
}

/// Registers, verifies, and logs in, returning (account_id, bearer_token).
///
/// Goes through the real HTTP flow rather than creating the account directly,
/// so a break in registration surfaces here too instead of only in
/// auth_routes.rs.
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
    let account_id = account_body["id"]
        .as_str()
        .expect("account id present")
        .to_string();

    let login_response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            login_body(email),
        ))
        .await
        .expect("login request succeeds");
    let login_body = body_json(login_response).await;
    let token = login_body["token"]
        .as_str()
        .expect("token present")
        .to_string();

    (account_id, token)
}

#[tokio::test]
async fn creating_a_dm_then_creating_it_again_is_idempotent() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let first = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm request succeeds");
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await;
    assert!(first_body["server_id"].is_null());
    assert_eq!(first_body["kind"], "dm");
    let channel_id = first_body["id"].as_str().expect("channel id present").to_string();

    let second = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm request succeeds");
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = body_json(second).await;
    assert_eq!(second_body["id"], channel_id);
}

#[tokio::test]
async fn starting_a_dm_with_yourself_returns_400() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn starting_a_dm_with_a_nonexistent_account_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let bogus_account_id = "018f0000-0000-7000-8000-000000000000";

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bogus_account_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "account_not_found");
}

#[tokio::test]
async fn a_dm_and_messages_sent_in_it_can_be_listed_by_both_participants() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let dm_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm request succeeds");
    let dm = body_json(dm_response).await;
    let channel_id = dm["id"].as_str().expect("channel id present");

    let send_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
            json!({ "content": "hey bob" }),
        ))
        .await
        .expect("send message request succeeds");
    assert_eq!(send_response.status(), StatusCode::CREATED);

    let bob_list = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &bob_token,
        ))
        .await
        .expect("list request succeeds");
    assert_eq!(bob_list.status(), StatusCode::OK);
    let body = body_json(bob_list).await;
    assert_eq!(body["items"].as_array().expect("items array").len(), 1);

    let list_dms_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/dms", &alice_token))
        .await
        .expect("list dms request succeeds");
    assert_eq!(list_dms_response.status(), StatusCode::OK);
    let dms_body = body_json(list_dms_response).await;
    let items = dms_body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], channel_id);
}

#[tokio::test]
async fn creating_a_group_dm_requires_at_least_two_other_participants() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/group-dms",
            &alice_token,
            json!({ "account_ids": [bob_id] }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn creating_a_group_dm_with_three_accounts_lets_all_of_them_message_in_it() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (carol_id, carol_token) = register_and_login(&app, &mail, "carol@example.com", "carol").await;

    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/group-dms",
            &alice_token,
            json!({ "account_ids": [bob_id, carol_id] }),
        ))
        .await
        .expect("create group dm request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    let group = body_json(response).await;
    assert_eq!(group["kind"], "group_dm");
    let channel_id = group["id"].as_str().expect("channel id present");

    for token in [&bob_token, &carol_token] {
        let list_response = app
            .clone()
            .oneshot(auth_request(
                Method::GET,
                &format!("/api/v1/channels/{channel_id}/messages"),
                token,
            ))
            .await
            .expect("list request succeeds");
        assert_eq!(list_response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn create_dm_with_no_token_returns_401() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/dms",
            json!({ "account_id": "018f0000-0000-7000-8000-000000000000" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
