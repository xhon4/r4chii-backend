//! `GET /api/v1/gateway` end-to-end coverage.
//! Unlike the other `crates/api/tests/*.rs` files, exercising a real
//! WebSocket handshake needs an actual TCP listener — `tower::ServiceExt::oneshot`
//! can drive ordinary HTTP requests against the router in-memory, but not a
//! WS upgrade. So register/login/setup still goes through `oneshot` (cheap,
//! matches every other test file's pattern), and only the gateway itself is
//! served over a real loopback `TcpListener`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::{
    body::Body,
    http::{header, Method, Request},
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{client::ClientRequestBuilder, Message as WsMessage};
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
    };

    (api::router(state), mail, container)
}

/// Serves `app` on an ephemeral loopback port and returns its address. The
/// server task runs for the lifetime of the test process (there's no
/// explicit shutdown) — fine for a short-lived test binary.
async fn spawn_server(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener binds");
    let addr = listener.local_addr().expect("listener has a local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("server runs");
    });
    addr
}

static NEXT_IP_OCTETS: AtomicU32 = AtomicU32::new(1);

fn next_peer_addr() -> SocketAddr {
    use std::net::{IpAddr, Ipv4Addr};
    let n = NEXT_IP_OCTETS.fetch_add(1, Ordering::Relaxed);
    let ip = Ipv4Addr::new(10, (n >> 16) as u8, (n >> 8) as u8, n as u8);
    SocketAddr::new(IpAddr::V4(ip), 0)
}

fn json_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(axum::extract::ConnectInfo(next_peer_addr()))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds")
}

fn auth_json_request(method: Method, uri: &str, token: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(axum::extract::ConnectInfo(next_peer_addr()))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
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
            json!({
                "email": email,
                "username": username,
                "password": "correct horse battery staple",
                "display_name": "Test User",
            }),
        ))
        .await
        .expect("registration request succeeds");
    assert_eq!(start.status(), http::StatusCode::ACCEPTED);

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
    assert_eq!(verify_response.status(), http::StatusCode::CREATED);
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
            json!({ "email": email, "password": "correct horse battery staple" }),
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

async fn server_and_channel(app: &axum::Router, token: &str) -> String {
    let server_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/servers",
            token,
            json!({ "name": "Alice's Place" }),
        ))
        .await
        .expect("create server request succeeds");
    let server = body_json(server_response).await;
    let server_id = server["id"].as_str().expect("server id present").to_string();

    let channel_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("create channel request succeeds");
    let channel = body_json(channel_response).await;
    channel["id"].as_str().expect("channel id present").to_string()
}

/// Bare `Authorization: Bearer` request with no body, for the GET/POST
/// endpoints that take none.
fn auth_request(method: Method, uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(axum::extract::ConnectInfo(next_peer_addr()))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builds")
}

async fn next_ws_message(
    stream: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin),
) -> WsMessage {
    timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("receives a frame before the bounded wait elapses")
        .expect("stream is not closed")
        .expect("frame reads without a transport error")
}

#[tokio::test]
async fn an_invalid_bearer_token_at_handshake_closes_with_4001_immediately() {
    let (app, _mail, _container) = test_app().await;
    let addr = spawn_server(app).await;

    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway").parse().expect("uri parses");
    let builder =
        ClientRequestBuilder::new(uri).with_header("Authorization", "Bearer not-a-real-token");

    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("handshake upgrades even though the token is invalid");

    let message = next_ws_message(&mut ws).await;
    match message {
        WsMessage::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 4001),
        other => panic!("expected a close frame with code 4001, got {other:?}"),
    }
}

#[tokio::test]
async fn a_bad_identify_frame_closes_with_4001() {
    let (app, _mail, _container) = test_app().await;
    let addr = spawn_server(app).await;

    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway").parse().expect("uri parses");
    let (mut ws, _response) = tokio_tungstenite::connect_async(uri.to_string())
        .await
        .expect("handshake upgrades with no token present at all");

    ws.send(WsMessage::Text(
        json!({ "type": "identify", "data": { "token": "not-a-real-token" } })
            .to_string()
            .into(),
    ))
    .await
    .expect("send succeeds");

    let message = next_ws_message(&mut ws).await;
    match message {
        WsMessage::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 4001),
        other => panic!("expected a close frame with code 4001, got {other:?}"),
    }
}

#[tokio::test]
async fn cookie_authenticated_connection_receives_a_ready_event_with_its_channel_ids() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let channel_id = server_and_channel(&app, &alice_token).await;

    let addr = spawn_server(app).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway").parse().expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={alice_token}"));

    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("cookie-authenticated handshake succeeds");

    let message = next_ws_message(&mut ws).await;
    let text = match message {
        WsMessage::Text(text) => text,
        other => panic!("expected a text frame, got {other:?}"),
    };
    let value: Value = serde_json::from_str(text.as_str()).expect("ready event is valid JSON");

    assert_eq!(value["type"], "ready");
    assert_eq!(value["data"]["account_id"], alice_id);
    let channel_ids = value["data"]["channel_ids"]
        .as_array()
        .expect("channel_ids is an array");
    assert!(
        channel_ids.iter().any(|id| id == &Value::String(channel_id.clone())),
        "ready event must list the channel the account can receive events for"
    );
}

#[tokio::test]
async fn ping_gets_a_pong_reply() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let addr = spawn_server(app).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway").parse().expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={alice_token}"));
    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("handshake succeeds");

    // First frame is always `ready` — consume it before pinging.
    let _ready = next_ws_message(&mut ws).await;

    ws.send(WsMessage::Text(json!({ "type": "ping" }).to_string().into()))
        .await
        .expect("send succeeds");

    let message = next_ws_message(&mut ws).await;
    let text = match message {
        WsMessage::Text(text) => text,
        other => panic!("expected a text frame, got {other:?}"),
    };
    let value: Value = serde_json::from_str(text.as_str()).expect("pong event is valid JSON");
    assert_eq!(value["type"], "pong");
}

#[tokio::test]
async fn a_message_sent_over_http_is_delivered_live_to_a_connected_gateway_client() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let channel_id = server_and_channel(&app, &alice_token).await;

    let addr = spawn_server(app.clone()).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway").parse().expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={alice_token}"));
    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("handshake succeeds");

    // Consume `ready` before sending, then send via the HTTP API (the
    // socket is a delivery channel, never a write path).
    let _ready = next_ws_message(&mut ws).await;

    let send_response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
            json!({ "content": "hello over the wire" }),
        ))
        .await
        .expect("send request succeeds");
    let sent_message = body_json(send_response).await;

    let message = next_ws_message(&mut ws).await;
    let text = match message {
        WsMessage::Text(text) => text,
        other => panic!("expected a text frame, got {other:?}"),
    };
    let value: Value = serde_json::from_str(text.as_str()).expect("event is valid JSON");

    assert_eq!(value["type"], "message.create");
    assert_eq!(value["data"]["message"]["id"], sent_message["id"]);
    assert_eq!(value["data"]["message"]["content"], "hello over the wire");
}

/// `ServerMemberResponse.status` is the REST snapshot of the same live
/// registry `presence.update` publishes deltas from — it has to be right
/// before any socket event arrives, which is exactly what the client's
/// initial render depends on.
#[tokio::test]
async fn member_list_status_reflects_who_actually_holds_a_gateway_socket() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/servers",
            &alice_token,
            json!({ "name": "Alice's Place" }),
        ))
        .await
        .expect("create server request succeeds");
    let server = body_json(server_response).await;
    let server_id = server["id"].as_str().expect("server id present").to_string();
    let invite_code = server["invite_code"]
        .as_str()
        .expect("the owner sees the invite code")
        .to_string();

    let join = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
        ))
        .await
        .expect("join request succeeds");
    assert_eq!(join.status(), http::StatusCode::CREATED);

    let members_uri = format!("/api/v1/servers/{server_id}/members");

    // Nobody is connected yet: everyone reads as offline.
    let before = body_json(
        app.clone()
            .oneshot(auth_request(Method::GET, &members_uri, &alice_token))
            .await
            .expect("members request succeeds"),
    )
    .await;
    for member in before["items"].as_array().expect("items array") {
        assert_eq!(
            member["status"], "offline",
            "no gateway socket exists yet, so no member can be online"
        );
    }

    let addr = spawn_server(app.clone()).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway").parse().expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={alice_token}"));
    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("handshake succeeds");

    // `ready` is only sent after the connection is registered in the hub, so
    // waiting for it removes the race between the socket connecting and the
    // HTTP request below reading the registry.
    let _ready = next_ws_message(&mut ws).await;

    let after = body_json(
        app.clone()
            .oneshot(auth_request(Method::GET, &members_uri, &alice_token))
            .await
            .expect("members request succeeds"),
    )
    .await;
    let items = after["items"].as_array().expect("items array");

    let alice = items
        .iter()
        .find(|m| m["account_id"] == alice_id)
        .expect("alice listed");
    assert_eq!(
        alice["status"], "online",
        "the account holding a live gateway socket must read as online"
    );

    let bob = items
        .iter()
        .find(|m| m["account_id"] == bob_id)
        .expect("bob listed");
    assert_eq!(
        bob["status"], "offline",
        "a member who never connected must not be dragged online by someone else's socket"
    );
}
