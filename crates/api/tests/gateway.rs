//! `GET /api/v1/gateway` end-to-end coverage.
//! Unlike the other `crates/api/tests/*.rs` files, exercising a real
//! WebSocket handshake needs an actual TCP listener — `tower::ServiceExt::oneshot`
//! can drive ordinary HTTP requests against the router in-memory, but not a
//! WS upgrade. So register/login/setup still goes through `oneshot` (cheap,
//! matches every other test file's pattern), and only the gateway itself is
//! served over a real loopback `TcpListener`.

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::Method;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{client::ClientRequestBuilder, Message as WsMessage};
use tower::ServiceExt;

mod common;
use common::*;

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
    let server_id = server["id"]
        .as_str()
        .expect("server id present")
        .to_string();

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
    channel["id"]
        .as_str()
        .expect("channel id present")
        .to_string()
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

    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
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

    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
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
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let channel_id = server_and_channel(&app, &alice_token).await;

    let addr = spawn_server(app).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
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
    let channel_ids = value
        .get("data")
        .and_then(|data| data.get("channel_ids"))
        .and_then(Value::as_array)
        .expect("channel_ids is an array");
    assert!(
        channel_ids
            .iter()
            .any(|id| id == &Value::String(channel_id.clone())),
        "ready event must list the channel the account can receive events for"
    );
}

#[tokio::test]
async fn ping_gets_a_pong_reply() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let addr = spawn_server(app).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={alice_token}"));
    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("handshake succeeds");

    // First frame is always `ready` — consume it before pinging.
    let _ready = next_ws_message(&mut ws).await;

    ws.send(WsMessage::Text(
        json!({ "type": "ping" }).to_string().into(),
    ))
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
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let channel_id = server_and_channel(&app, &alice_token).await;

    let addr = spawn_server(app.clone()).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
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
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
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
    let server_id = server["id"]
        .as_str()
        .expect("server id present")
        .to_string();
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
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
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

#[tokio::test]
async fn ready_omits_a_restricted_channel_after_its_only_role_is_revoked() {
    let (app, mail, _container) = test_app().await;
    let (_owner_id, owner_token) =
        register_and_login(&app, &mail, "owner_ready@example.com", "owner_ready").await;
    let (member_id, member_token) =
        register_and_login(&app, &mail, "member_ready@example.com", "member_ready").await;

    let server = body_json(
        app.clone()
            .oneshot(auth_json_request(
                Method::POST,
                "/api/v1/servers",
                &owner_token,
                json!({ "name": "Ready Access" }),
            ))
            .await
            .expect("create server request succeeds"),
    )
    .await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    let join = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &member_token,
        ))
        .await
        .expect("join request succeeds");
    assert_eq!(join.status(), http::StatusCode::CREATED);

    let channel = body_json(
        app.clone()
            .oneshot(auth_json_request(
                Method::POST,
                &format!("/api/v1/servers/{server_id}/channels"),
                &owner_token,
                json!({ "name": "staff-only" }),
            ))
            .await
            .expect("create channel request succeeds"),
    )
    .await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let restrict = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/restricted"),
            &owner_token,
            json!({ "restricted": true }),
        ))
        .await
        .expect("restrict request succeeds");
    assert_eq!(restrict.status(), http::StatusCode::OK);

    let role = body_json(
        app.clone()
            .oneshot(auth_json_request(
                Method::POST,
                &format!("/api/v1/servers/{server_id}/roles"),
                &owner_token,
                json!({ "name": "Staff" }),
            ))
            .await
            .expect("create role request succeeds"),
    )
    .await;
    let role_id = role["id"].as_str().expect("role id present");
    let assign = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/members/{member_id}/roles"),
            &owner_token,
            json!({ "role_ids": [role_id] }),
        ))
        .await
        .expect("assign role request succeeds");
    assert_eq!(assign.status(), http::StatusCode::OK);
    let grant = app
        .clone()
        .oneshot(auth_json_request(
            Method::PUT,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/permissions/{role_id}"),
            &owner_token,
            json!({ "permissions": 1 }),
        ))
        .await
        .expect("grant request succeeds");
    assert_eq!(grant.status(), http::StatusCode::NO_CONTENT);

    let addr = spawn_server(app.clone()).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={member_token}"));
    let (mut granted_ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("granted member connects");
    let granted_ready = match next_ws_message(&mut granted_ws).await {
        WsMessage::Text(text) => {
            serde_json::from_str::<Value>(text.as_str()).expect("ready is JSON")
        }
        other => panic!("expected ready text frame, got {other:?}"),
    };
    assert!(
        granted_ready["data"]["channel_ids"]
            .as_array()
            .expect("channel_ids is an array")
            .iter()
            .any(|id| id == channel_id),
        "the granted role is present before revocation"
    );
    granted_ws.close(None).await.expect("close succeeds");

    let revoke = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/members/{member_id}/roles"),
            &owner_token,
            json!({ "role_ids": [] }),
        ))
        .await
        .expect("revoke role request succeeds");
    assert_eq!(revoke.status(), http::StatusCode::OK);

    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={member_token}"));
    let (mut revoked_ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("revoked member connects");
    let revoked_ready = match next_ws_message(&mut revoked_ws).await {
        WsMessage::Text(text) => {
            serde_json::from_str::<Value>(text.as_str()).expect("ready is JSON")
        }
        other => panic!("expected ready text frame, got {other:?}"),
    };
    assert!(
        !revoked_ready["data"]["channel_ids"]
            .as_array()
            .expect("channel_ids is an array")
            .iter()
            .any(|id| id == channel_id),
        "the revoked role must remove the restricted channel from ready"
    );
}

/// The reported bug: bob accepts alice's request and alice's screen keeps
/// showing it as pending until she reloads. This drives the real HTTP
/// handlers, so it covers the wiring the hub's own tests cannot.
#[tokio::test]
async fn accepting_a_friend_request_reaches_the_requester_over_the_socket() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let request_app = app.clone();
    let addr = spawn_server(app).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={alice_token}"));

    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("handshake succeeds");
    let _ready = next_ws_message(&mut ws).await;

    // Alice asks first, from her own HTTP request rather than her socket.
    request_app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("friend request succeeds");

    let pending = next_ws_message(&mut ws).await;
    let pending: Value = match pending {
        WsMessage::Text(text) => serde_json::from_str(text.as_str()).expect("valid JSON"),
        other => panic!("expected a text frame, got {other:?}"),
    };
    assert_eq!(pending["type"], "friendship.update");
    assert_eq!(pending["data"]["friendship"]["status"], "pending");
    // Alice's own projection names bob, never herself.
    assert_eq!(pending["data"]["friendship"]["account_id"], bob_id);

    // Bob accepts. Alice never issued this request and must still be told.
    request_app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("friend accept succeeds");

    let accepted = next_ws_message(&mut ws).await;
    let accepted: Value = match accepted {
        WsMessage::Text(text) => serde_json::from_str(text.as_str()).expect("valid JSON"),
        other => panic!("expected a text frame, got {other:?}"),
    };
    assert_eq!(accepted["type"], "friendship.update");
    assert_eq!(accepted["data"]["friendship"]["status"], "accepted");
    assert_eq!(accepted["data"]["friendship"]["account_id"], bob_id);
}

/// Unfriending is the same row deletion as declining or cancelling, so it is
/// the same frame — and the other side must not keep a friend who is gone.
#[tokio::test]
async fn unfriending_reaches_the_other_party_over_the_socket() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let request_app = app.clone();
    for (token, other) in [(&alice_token, &bob_id), (&bob_token, &alice_id)] {
        request_app
            .clone()
            .oneshot(auth_json_request(
                Method::POST,
                "/api/v1/friends",
                token,
                json!({ "account_id": other }),
            ))
            .await
            .expect("friendship is established");
    }

    let addr = spawn_server(app).await;
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway")
        .parse()
        .expect("uri parses");
    let builder = ClientRequestBuilder::new(uri)
        .with_header("Cookie", format!("r4chii_session={alice_token}"));

    let (mut ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("handshake succeeds");
    let _ready = next_ws_message(&mut ws).await;

    request_app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/friends/{alice_id}"),
            &bob_token,
        ))
        .await
        .expect("unfriend succeeds");

    let removed = next_ws_message(&mut ws).await;
    let removed: Value = match removed {
        WsMessage::Text(text) => serde_json::from_str(text.as_str()).expect("valid JSON"),
        other => panic!("expected a text frame, got {other:?}"),
    };
    assert_eq!(removed["type"], "friendship.remove");
    assert_eq!(removed["data"]["account_id"], bob_id);
}
