//! End-to-end coverage of the M0 success criteria: two
//! accounts, over the real HTTP + WebSocket surface (never calling
//! `DomainService` directly), can register, log in, friend each other, DM,
//! create a server, invite a friend into it, chat live in a text channel,
//! edit/delete their own messages, block/unblock, and manage sessions.
//!
//! This is the "hardening" slice's e2e round-trip test — it deliberately
//! re-exercises behavior already unit/integration-tested elsewhere, because
//! its job is proving the REST + WS surface composes end to end, not
//! re-proving any single piece of domain logic in isolation.

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{client::ClientRequestBuilder, Message as WsMessage};
use tower::ServiceExt;

mod common;
use common::*;

/// Serves `app` on an ephemeral loopback port for the WS handshake, which
/// (unlike ordinary HTTP requests via `oneshot`) needs a real TCP listener.
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

async fn next_ws_message(
    stream: &mut (impl StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>>
              + Unpin),
) -> Value {
    let message = timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("receives a frame before the bounded wait elapses")
        .expect("stream is not closed")
        .expect("frame reads without a transport error");
    match message {
        WsMessage::Text(text) => {
            serde_json::from_str(text.as_str()).expect("frame is valid JSON")
        }
        other => panic!("expected a text frame, got {other:?}"),
    }
}

async fn connect_gateway(
    addr: SocketAddr,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let uri: http::Uri = format!("ws://{addr}/api/v1/gateway").parse().expect("uri parses");
    let builder =
        ClientRequestBuilder::new(uri).with_header("Authorization", format!("Bearer {token}"));
    let (ws, _response) = tokio_tungstenite::connect_async(builder)
        .await
        .expect("authenticated handshake succeeds");
    ws
}

#[tokio::test]
async fn m0_success_criteria_round_trip_works_end_to_end() {
    let (app, mail, _container) = test_app().await;

    // --- register + log in ---
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let addr = spawn_server(app.clone()).await;
    let mut alice_ws = connect_gateway(addr, &alice_token).await;
    let mut bob_ws = connect_gateway(addr, &bob_token).await;
    let alice_ready = next_ws_message(&mut alice_ws).await;
    let bob_ready = next_ws_message(&mut bob_ws).await;
    assert_eq!(alice_ready["type"], "ready");
    assert_eq!(bob_ready["type"], "ready");

    // --- friends: request then accept ("add each other as friends") ---
    let request_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("friend request succeeds");
    assert_eq!(request_response.status(), StatusCode::CREATED);
    let pending = body_json(request_response).await;
    assert_eq!(pending["status"], "pending");

    let accept_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("friend accept succeeds");
    assert_eq!(accept_response.status(), StatusCode::CREATED);
    let accepted = body_json(accept_response).await;
    assert_eq!(accepted["status"], "accepted");

    let alice_friends = body_json(
        app.clone()
            .oneshot(auth_request(Method::GET, "/api/v1/friends", &alice_token))
            .await
            .expect("list friendships succeeds"),
    )
    .await;
    assert_eq!(alice_friends["items"].as_array().unwrap().len(), 1);
    assert_eq!(alice_friends["items"][0]["status"], "accepted");

    // --- DM, delivered live over the socket ("DM") ---
    let dm_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm succeeds");
    assert_eq!(dm_response.status(), StatusCode::CREATED);
    let dm_channel = body_json(dm_response).await;
    let dm_channel_id = dm_channel["id"].as_str().expect("dm channel id present").to_string();

    let dm_send_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{dm_channel_id}/messages"),
            &alice_token,
            json!({ "content": "hey bob" }),
        ))
        .await
        .expect("dm send succeeds");
    assert_eq!(dm_send_response.status(), StatusCode::CREATED);
    let dm_message = body_json(dm_send_response).await;

    // Fan-out goes to every channel member including the sender itself —
    // no sender exclusion — drain alice's own echo too, not just bob's.
    let alice_dm_echo = next_ws_message(&mut alice_ws).await;
    assert_eq!(alice_dm_echo["type"], "message.create");
    let bob_dm_event = next_ws_message(&mut bob_ws).await;
    assert_eq!(bob_dm_event["type"], "message.create");
    assert_eq!(bob_dm_event["data"]["message"]["id"], dm_message["id"]);
    assert_eq!(bob_dm_event["data"]["message"]["content"], "hey bob");

    // --- server + invite ("create a server, invite others") ---
    let server_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/servers",
            &alice_token,
            json!({ "name": "Alice's Place" }),
        ))
        .await
        .expect("create server succeeds");
    assert_eq!(server_response.status(), StatusCode::CREATED);
    let server = body_json(server_response).await;
    let server_id = server["id"].as_str().expect("server id present").to_string();
    let invite_code = server["invite_code"]
        .as_str()
        .expect("owner sees the invite code")
        .to_string();

    let channel_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("create channel succeeds");
    assert_eq!(channel_response.status(), StatusCode::CREATED);
    let channel = body_json(channel_response).await;
    let channel_id = channel["id"].as_str().expect("channel id present").to_string();

    // Creating a channel fans a `channel.create` event out to the server's members,
    // the owner included. Drain it so the reads below start at the message events.
    let alice_channel_created = next_ws_message(&mut alice_ws).await;
    assert_eq!(alice_channel_created["type"], "channel.create");
    assert_eq!(alice_channel_created["data"]["channel"]["id"], channel_id);

    let join_response = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
        ))
        .await
        .expect("join via invite succeeds");
    assert_eq!(join_response.status(), StatusCode::CREATED);

    // --- chat live in the text channel ("messages appearing live") ---
    let send_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
            json!({ "content": "welcome to the server" }),
        ))
        .await
        .expect("channel send succeeds");
    assert_eq!(send_response.status(), StatusCode::CREATED);
    let sent_message = body_json(send_response).await;
    let message_id = sent_message["id"].as_str().expect("message id present").to_string();

    let alice_channel_echo = next_ws_message(&mut alice_ws).await;
    assert_eq!(alice_channel_echo["type"], "message.create");
    let bob_channel_event = next_ws_message(&mut bob_ws).await;
    assert_eq!(bob_channel_event["type"], "message.create");
    assert_eq!(bob_channel_event["data"]["message"]["id"], message_id);
    assert_eq!(
        bob_channel_event["data"]["message"]["content"],
        "welcome to the server"
    );

    // --- edit/delete own messages, both delivered live ---
    let edit_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &alice_token,
            json!({ "content": "welcome to the server!" }),
        ))
        .await
        .expect("edit succeeds");
    assert_eq!(edit_response.status(), StatusCode::OK);
    let edited = body_json(edit_response).await;
    assert_eq!(edited["content"], "welcome to the server!");
    assert!(edited["edited_at"].is_string());

    let alice_edit_echo = next_ws_message(&mut alice_ws).await;
    assert_eq!(alice_edit_echo["type"], "message.update");
    let bob_edit_event = next_ws_message(&mut bob_ws).await;
    assert_eq!(bob_edit_event["type"], "message.update");
    assert_eq!(
        bob_edit_event["data"]["message"]["content"],
        "welcome to the server!"
    );

    let delete_response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &alice_token,
        ))
        .await
        .expect("delete succeeds");
    assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);

    let alice_delete_echo = next_ws_message(&mut alice_ws).await;
    assert_eq!(alice_delete_echo["type"], "message.delete");
    let bob_delete_event = next_ws_message(&mut bob_ws).await;
    assert_eq!(bob_delete_event["type"], "message.delete");
    assert_eq!(bob_delete_event["data"]["message_id"], message_id);

    let messages_after_delete = body_json(
        app.clone()
            .oneshot(auth_request(
                Method::GET,
                &format!("/api/v1/channels/{channel_id}/messages"),
                &bob_token,
            ))
            .await
            .expect("list messages succeeds"),
    )
    .await;
    let deleted_entry = messages_after_delete["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == message_id)
        .expect("deleted message still has a row");
    assert!(deleted_entry["content"].is_null(), "deleted content is hidden");

    // --- block prevents further DMs, unblock restores it ---
    let block_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("block succeeds");
    assert_eq!(block_response.status(), StatusCode::CREATED);

    let blocked_send = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{dm_channel_id}/messages"),
            &bob_token,
            json!({ "content": "can you still see this" }),
        ))
        .await
        .expect("blocked send request completes");
    assert_eq!(blocked_send.status(), StatusCode::FORBIDDEN);
    let blocked_body = body_json(blocked_send).await;
    assert_eq!(blocked_body["error"]["code"], "blocked");

    let unblock_response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/blocks/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("unblock succeeds");
    assert_eq!(unblock_response.status(), StatusCode::NO_CONTENT);

    let unblocked_send = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{dm_channel_id}/messages"),
            &bob_token,
            json!({ "content": "back to normal" }),
        ))
        .await
        .expect("post-unblock send succeeds");
    assert_eq!(unblocked_send.status(), StatusCode::CREATED);

    // Fans out to both DM participants, including bob's own echo.
    let alice_dm_event = next_ws_message(&mut alice_ws).await;
    assert_eq!(alice_dm_event["type"], "message.create");
    assert_eq!(alice_dm_event["data"]["message"]["content"], "back to normal");
    let bob_dm_echo = next_ws_message(&mut bob_ws).await;
    assert_eq!(bob_dm_echo["type"], "message.create");

    // --- session management (list, revoke one, logout-all) ---
    // Identify alice's original session id *before* creating a second one,
    // so we revoke that specific session below rather than guessing at list
    // order — revoking the wrong one would invalidate `alice_second_token`
    // and break the logout-all check that follows.
    let original_sessions = body_json(
        app.clone()
            .oneshot(auth_request(Method::GET, "/api/v1/sessions", &alice_token))
            .await
            .expect("list sessions succeeds"),
    )
    .await;
    let original_session_items = original_sessions["items"].as_array().expect("items is an array");
    assert_eq!(original_session_items.len(), 1, "only the original session exists so far");
    let original_session_id = original_session_items[0]["id"]
        .as_str()
        .expect("session id present")
        .to_string();

    let second_login = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({ "email": "alice@example.com", "password": "correct horse battery staple" }),
        ))
        .await
        .expect("second login succeeds");
    assert_eq!(second_login.status(), StatusCode::CREATED);
    let alice_second_token = body_json(second_login).await["token"]
        .as_str()
        .expect("second token present")
        .to_string();

    let sessions = body_json(
        app.clone()
            .oneshot(auth_request(Method::GET, "/api/v1/sessions", &alice_token))
            .await
            .expect("list sessions succeeds"),
    )
    .await;
    assert_eq!(
        sessions["items"].as_array().unwrap().len(),
        2,
        "alice has two active sessions (original + the one just logged in)"
    );

    let revoke_response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/sessions/{original_session_id}"),
            &alice_token,
        ))
        .await
        .expect("revoke succeeds");
    assert_eq!(revoke_response.status(), StatusCode::NO_CONTENT);

    let sessions_after_revoke = body_json(
        app.clone()
            .oneshot(auth_request(Method::GET, "/api/v1/sessions", &alice_second_token))
            .await
            .expect("list sessions succeeds"),
    )
    .await;
    assert_eq!(
        sessions_after_revoke["items"].as_array().unwrap().len(),
        1,
        "exactly the revoked session is gone, the second one is unaffected"
    );

    let logout_all_response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            "/api/v1/sessions",
            &alice_second_token,
        ))
        .await
        .expect("logout-all succeeds");
    assert_eq!(logout_all_response.status(), StatusCode::NO_CONTENT);

    let post_logout_all = app
        .clone()
        .oneshot(auth_request(Method::GET, "/api/v1/sessions", &alice_second_token))
        .await
        .expect("request completes");
    assert_eq!(
        post_logout_all.status(),
        StatusCode::UNAUTHORIZED,
        "the token used to call logout-all was itself revoked by it"
    );
}
