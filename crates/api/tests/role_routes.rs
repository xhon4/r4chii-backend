//! M2: role CRUD, hierarchy, and permission checks.
//! Same harness as `server_routes.rs`.

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

const MANAGE_ROLES: i64 = 1 << 1;

#[tokio::test]
async fn creating_a_server_creates_an_implicit_default_role() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/roles"),
            &alice_token,
        ))
        .await
        .expect("list roles request succeeds");
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["is_default"], true);
    assert_eq!(items[0]["name"], "everyone");
    assert_eq!(items[0]["permissions"], 0);
}

#[tokio::test]
async fn the_owner_can_create_a_role_without_holding_manage_roles_themselves() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let role = create_role(&app, &alice_token, server_id, "moderator").await;
    assert_eq!(role["name"], "moderator");
    assert_eq!(role["is_default"], false);
    // New roles start above every existing role — position 1 above the
    // default role's fixed 0.
    assert_eq!(role["position"], 1);
}

#[tokio::test]
async fn a_plain_member_without_manage_roles_cannot_create_a_role() {
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
            &format!("/api/v1/servers/{server_id}/roles"),
            &bob_token,
            json!({ "name": "moderator" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "missing_permission");
}

#[tokio::test]
async fn a_role_with_manage_roles_can_create_further_roles() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let role = create_role(&app, &alice_token, server_id, "moderator").await;
    let role_id = role["id"].as_str().expect("role id present");

    // Grant it the MANAGE_ROLES bit, then assign it to bob.
    let update = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/roles/{role_id}"),
            &alice_token,
            json!({ "permissions": MANAGE_ROLES }),
        ))
        .await
        .expect("update role request succeeds");
    assert_eq!(update.status(), StatusCode::OK);

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

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/roles"),
            &bob_token,
            json!({ "name": "helper" }),
        ))
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_role_cannot_edit_a_role_with_equal_or_higher_position() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    // Two roles: "senior" (position 2, created second so it's higher) and
    // "junior" (position 1) — junior holds MANAGE_ROLES but must not be able
    // to touch senior, which outranks it.
    let junior = create_role(&app, &alice_token, server_id, "junior").await;
    let junior_id = junior["id"].as_str().expect("id present").to_string();
    let senior = create_role(&app, &alice_token, server_id, "senior").await;
    let senior_id = senior["id"].as_str().expect("id present").to_string();
    assert!(
        senior["position"].as_i64() > junior["position"].as_i64(),
        "senior must outrank junior"
    );

    app.clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/roles/{junior_id}"),
            &alice_token,
            json!({ "permissions": MANAGE_ROLES }),
        ))
        .await
        .expect("grant request succeeds");

    app.clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}/roles"),
            &alice_token,
            json!({ "role_ids": [junior_id] }),
        ))
        .await
        .expect("assign request succeeds");

    let response = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/roles/{senior_id}"),
            &bob_token,
            json!({ "name": "renamed" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "insufficient_hierarchy");
}

#[tokio::test]
async fn the_default_role_cannot_be_deleted() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let roles = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/roles"),
            &alice_token,
        ))
        .await
        .expect("list roles succeeds");
    let roles_body = body_json(roles).await;
    let default_role_id = roles_body["items"][0]["id"].as_str().expect("default role id");

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}/roles/{default_role_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "cannot_modify_default_role");
}

#[tokio::test]
async fn reordering_roles_persists_the_new_positions() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let a = create_role(&app, &alice_token, server_id, "a").await;
    let a_id = a["id"].as_str().expect("id present").to_string();
    let b = create_role(&app, &alice_token, server_id, "b").await;
    let b_id = b["id"].as_str().expect("id present").to_string();

    // Reorder so "b" leads "a" — the opposite of creation order.
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/roles/order"),
            &alice_token,
            json!({ "role_ids": [b_id, a_id] }),
        ))
        .await
        .expect("reorder request succeeds");
    assert_eq!(response.status(), StatusCode::OK);

    let list = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/roles"),
            &alice_token,
        ))
        .await
        .expect("list roles succeeds");
    let body = body_json(list).await;
    let items = body["items"].as_array().expect("items array");
    // Ordered by position DESC: "b" (now highest) leads, then "a", then the
    // default role fixed at the bottom.
    assert_eq!(items[0]["name"], "b");
    assert_eq!(items[1]["name"], "a");
    assert_eq!(items[2]["is_default"], true);
}

#[tokio::test]
async fn a_role_id_from_a_different_server_is_not_found() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server_a = create_server(&app, &alice_token, "Server A").await;
    let server_b = create_server(&app, &alice_token, "Server B").await;
    let server_b_id = server_b["id"].as_str().expect("server id present");

    let role_in_a = create_role(&app, &alice_token, server_a["id"].as_str().unwrap(), "role").await;
    let role_id = role_in_a["id"].as_str().expect("role id present");

    let response = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_b_id}/roles/{role_id}"),
            &alice_token,
            json!({ "name": "renamed" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "role_not_found");
}
