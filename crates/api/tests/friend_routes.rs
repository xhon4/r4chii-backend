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
async fn sending_a_friend_request_returns_201_pending() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["status"], "pending");
    assert_eq!(body["account_id"], bob_id);
}

#[tokio::test]
async fn a_reciprocal_request_accepts_and_returns_201() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    let accept_response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("accept succeeds");

    assert_eq!(accept_response.status(), StatusCode::CREATED);
    let body = body_json(accept_response).await;
    assert_eq!(body["status"], "accepted");
}

#[tokio::test]
async fn sending_a_friend_request_to_yourself_returns_400() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
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
async fn listing_friendships_shows_both_pending_and_accepted() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (carol_id, _carol_token) = register_and_login(&app, &mail, "carol@example.com", "carol").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");
    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("accept succeeds");
    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": carol_id }),
        ))
        .await
        .expect("request succeeds");

    let list_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/friends", &alice_token))
        .await
        .expect("list request succeeds");
    assert_eq!(list_response.status(), StatusCode::OK);
    let body = body_json(list_response).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn removing_a_friendship_returns_204_and_it_no_longer_lists() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    let remove_response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/friends/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("remove request succeeds");
    assert_eq!(remove_response.status(), StatusCode::NO_CONTENT);

    let list_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/friends", &alice_token))
        .await
        .expect("list request succeeds");
    let body = body_json(list_response).await;
    assert!(body["items"].as_array().expect("items array").is_empty());
}

#[tokio::test]
async fn removing_a_nonexistent_friendship_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/friends/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("remove request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "friend_request_not_found");
}

#[tokio::test]
async fn a_friend_request_between_a_blocked_pair_returns_403() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("block request succeeds");

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "blocked");
}

#[tokio::test]
async fn send_friend_request_with_no_token_returns_401() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/friends",
            json!({ "account_id": "018f0000-0000-7000-8000-000000000000" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
