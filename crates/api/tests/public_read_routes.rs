//! Public read path HTTP surface — `/archive/t/{id}`,
//! `/archive/sitemap.xml`, `/robots.txt`. Same harness as `role_routes.rs`,
//! plus a few unauthenticated requests (no `Authorization` header).

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

fn plain_request(method: Method, uri: &str) -> Request<Body> {
    request(method, uri).body(Body::empty()).expect("request builds")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.expect("body collects").to_bytes();
    serde_json::from_slice(&bytes).expect("body is valid JSON")
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.expect("body collects").to_bytes();
    String::from_utf8(bytes.to_vec()).expect("body is valid utf-8")
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

async fn create_server(app: &axum::Router, token: &str, name: &str, visibility: Option<&str>) -> Value {
    let mut body = json!({ "name": name });
    if let Some(visibility) = visibility {
        body["visibility"] = json!(visibility);
    }
    let response = app
        .clone()
        .oneshot(auth_json_request(Method::POST, "/api/v1/servers", token, body))
        .await
        .expect("create server request succeeds");
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

async fn create_thread(app: &axum::Router, token: &str, channel_id: &str, title: &str) -> Value {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/threads"),
            token,
            json!({ "title": title }),
        ))
        .await
        .expect("create thread request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

async fn send_message(app: &axum::Router, token: &str, channel_id: &str, content: &str) {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            token,
            json!({ "content": content }),
        ))
        .await
        .expect("send message request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn a_public_threads_archive_page_renders_with_no_auth() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Public Place", Some("public")).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let thread = create_thread(&app, &alice_token, channel_id, "How do I configure X?").await;
    let thread_id = thread["id"].as_str().expect("thread id present");
    send_message(&app, &alice_token, thread_id, "does anyone know?").await;

    let response = app
        .oneshot(plain_request(Method::GET, &format!("/archive/t/{thread_id}-how-do-i-configure-x")))
        .await
        .expect("archive page request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains("How do I configure X?"));
    assert!(body.contains("does anyone know?"));
    assert!(body.contains("Test User"));
}

#[tokio::test]
async fn a_private_threads_archive_page_returns_404_with_no_auth() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Private Place", None).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let thread = create_thread(&app, &alice_token, channel_id, "secret topic").await;
    let thread_id = thread["id"].as_str().expect("thread id present");

    let response = app
        .oneshot(plain_request(Method::GET, &format!("/archive/t/{thread_id}-secret-topic")))
        .await
        .expect("archive page request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_malformed_thread_ref_returns_404_not_a_500() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(plain_request(Method::GET, "/archive/t/not-a-real-uuid"))
        .await
        .expect("archive page request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_sitemap_lists_a_public_thread_and_robots_txt_points_at_it() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Public Place", Some("public")).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let thread = create_thread(&app, &alice_token, channel_id, "a public topic").await;
    let thread_id = thread["id"].as_str().expect("thread id present");

    let sitemap_response = app
        .clone()
        .oneshot(plain_request(Method::GET, "/archive/sitemap.xml"))
        .await
        .expect("sitemap request succeeds");
    assert_eq!(sitemap_response.status(), StatusCode::OK);
    let sitemap_body = body_text(sitemap_response).await;
    assert!(sitemap_body.contains(thread_id));

    let robots_response = app
        .oneshot(plain_request(Method::GET, "/robots.txt"))
        .await
        .expect("robots.txt request succeeds");
    assert_eq!(robots_response.status(), StatusCode::OK);
    let robots_body = body_text(robots_response).await;
    assert!(robots_body.contains("/archive/sitemap.xml"));
    assert!(robots_body.contains("Allow: /archive/"));
}
