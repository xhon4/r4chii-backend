//! HTTP surface for timeout, pin/unpin, nickname management, and
//! invite-code visibility/regeneration. Domain-level coverage (hierarchy,
//! mentions, DM-vs-server-channel message deletion) lives in
//! `crates/domain/tests/role_permissions_v2_service.rs` — this file only
//! confirms routing, status codes, and the realtime side-effects reach the
//! HTTP layer. Same harness as `role_routes.rs`.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::*;

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

async fn create_channel(app: &axum::Router, token: &str, server_id: &str) -> Value {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("create channel request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

async fn send_message(app: &axum::Router, token: &str, channel_id: &str, content: &str) -> Value {
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
    body_json(response).await
}

/// Creates a role, grants it exactly `bits`, and assigns it to `target_id` —
/// `owner_token` bypasses `MANAGE_ROLES` via ownership, same shape
/// `role_routes.rs`'s own `a_role_with_manage_roles_can_create_further_roles`
/// test already establishes.
async fn grant_role(
    app: &axum::Router,
    owner_token: &str,
    server_id: &str,
    target_id: &str,
    name: &str,
    bits: i64,
) -> String {
    let role = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/roles"),
            owner_token,
            json!({ "name": name }),
        ))
        .await
        .expect("create role request succeeds");
    assert_eq!(role.status(), StatusCode::CREATED);
    let role_id = body_json(role).await["id"]
        .as_str()
        .expect("role id present")
        .to_string();

    let update = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/roles/{role_id}"),
            owner_token,
            json!({ "permissions": bits }),
        ))
        .await
        .expect("update role request succeeds");
    assert_eq!(update.status(), StatusCode::OK);

    let assign = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/members/{target_id}/roles"),
            owner_token,
            json!({ "role_ids": [role_id] }),
        ))
        .await
        .expect("assign role request succeeds");
    assert_eq!(assign.status(), StatusCode::OK);

    role_id
}

const TIMEOUT_MEMBERS: i64 = 1 << 9;
const MANAGE_MESSAGES: i64 = 1 << 10;
const PIN_MESSAGES: i64 = 1 << 11;
const MANAGE_INVITES: i64 = 1 << 12;

// ---- timeout ----

#[tokio::test]
async fn timing_out_then_clearing_a_member_returns_204() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let until = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}/timeout"),
            &alice_token,
            json!({ "until": until }),
        ))
        .await
        .expect("timeout request succeeds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let clear = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}/timeout"),
            &alice_token,
        ))
        .await
        .expect("clear timeout request succeeds");
    assert_eq!(clear.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_plain_member_without_timeout_members_gets_403() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (carol_id, carol_token) =
        register_and_login(&app, &mail, "carol@example.com", "carol").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;
    join_server(&app, &carol_token, invite_code).await;
    let _ = carol_id;

    let until = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}/timeout"),
            &carol_token,
            json!({ "until": until }),
        ))
        .await
        .expect("timeout request succeeds");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ---- pin/unpin ----

#[tokio::test]
async fn pinning_a_message_with_pin_messages_returns_the_pinned_message_and_lists_it() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;
    grant_role(
        &app,
        &alice_token,
        server_id,
        &bob_id,
        "pinner",
        PIN_MESSAGES,
    )
    .await;

    let channel = create_channel(&app, &alice_token, server_id).await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let message = send_message(&app, &alice_token, channel_id, "important").await;
    let message_id = message["id"].as_str().expect("message id present");

    let pin = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}/pin"),
            &bob_token,
        ))
        .await
        .expect("pin request succeeds");
    assert_eq!(pin.status(), StatusCode::OK);
    let pinned_body = body_json(pin).await;
    assert!(!pinned_body["pinned_at"].is_null());

    let pins = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/pins"),
            &alice_token,
        ))
        .await
        .expect("list pins request succeeds");
    assert_eq!(pins.status(), StatusCode::OK);
    let pins_body = body_json(pins).await;
    assert_eq!(pins_body["items"].as_array().expect("items array").len(), 1);
}

#[tokio::test]
async fn a_plain_member_without_pin_messages_gets_403() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let channel = create_channel(&app, &alice_token, server_id).await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let message = send_message(&app, &alice_token, channel_id, "important").await;
    let message_id = message["id"].as_str().expect("message id present");

    let pin = app
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}/pin"),
            &bob_token,
        ))
        .await
        .expect("pin request succeeds");
    assert_eq!(pin.status(), StatusCode::FORBIDDEN);
}

// ---- manage messages (delete someone else's, over HTTP) ----

#[tokio::test]
async fn manage_messages_lets_a_moderator_delete_someone_elses_message_over_http() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;
    grant_role(
        &app,
        &alice_token,
        server_id,
        &bob_id,
        "mod",
        MANAGE_MESSAGES,
    )
    .await;

    let channel = create_channel(&app, &alice_token, server_id).await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let message = send_message(&app, &alice_token, channel_id, "hi").await;
    let message_id = message["id"].as_str().expect("message id present");

    let delete = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &bob_token,
        ))
        .await
        .expect("delete request succeeds");
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
}

// ---- nicknames ----

#[tokio::test]
async fn a_member_can_set_their_own_nickname_but_not_someone_elses_without_the_bit() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    let bob_membership = join_server(&app, &bob_token, invite_code).await;
    let _ = bob_membership;

    let own = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}/nickname"),
            &bob_token,
            json!({ "nickname": "Bobby" }),
        ))
        .await
        .expect("self nickname request succeeds");
    assert_eq!(own.status(), StatusCode::NO_CONTENT);

    let (alice_id, _) = (
        server["owner_account_id"]
            .as_str()
            .expect("owner id present"),
        (),
    );
    let other = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/members/{alice_id}/nickname"),
            &bob_token,
            json!({ "nickname": "Not Allowed" }),
        ))
        .await
        .expect("other nickname request succeeds");
    assert_eq!(other.status(), StatusCode::FORBIDDEN);
}

// ---- invites ----

#[tokio::test]
async fn regenerating_the_invite_code_requires_manage_invites_and_returns_a_fresh_code() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let denied = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/invite-code/regenerate"),
            &bob_token,
        ))
        .await
        .expect("regenerate request succeeds");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    grant_role(
        &app,
        &alice_token,
        server_id,
        &bob_id,
        "inviter",
        MANAGE_INVITES,
    )
    .await;

    let allowed = app
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/invite-code/regenerate"),
            &bob_token,
        ))
        .await
        .expect("regenerate request succeeds");
    assert_eq!(allowed.status(), StatusCode::OK);
    let body = body_json(allowed).await;
    let new_code = body["invite_code"]
        .as_str()
        .expect("fresh invite code present");
    assert_ne!(new_code, invite_code);
}

#[tokio::test]
async fn member_list_redacts_timeout_reason_except_for_target_owner_and_admin() {
    let (app, mail, _container) = test_app().await;
    let (_owner_id, owner_token) =
        register_and_login(&app, &mail, "reason_owner@example.com", "reason_owner").await;
    let (issuer_id, issuer_token) =
        register_and_login(&app, &mail, "reason_issuer@example.com", "reason_issuer").await;
    let (target_id, target_token) =
        register_and_login(&app, &mail, "reason_target@example.com", "reason_target").await;
    let (_ordinary_id, ordinary_token) = register_and_login(
        &app,
        &mail,
        "reason_ordinary@example.com",
        "reason_ordinary",
    )
    .await;
    let (admin_id, admin_token) =
        register_and_login(&app, &mail, "reason_admin@example.com", "reason_admin").await;
    let server = create_server(&app, &owner_token, "Reason Server").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite = server["invite_code"].as_str().expect("invite code present");
    for token in [&issuer_token, &target_token, &ordinary_token, &admin_token] {
        join_server(&app, token, invite).await;
    }
    grant_role(
        &app,
        &owner_token,
        server_id,
        &issuer_id,
        "Timeout Issuer",
        TIMEOUT_MEMBERS,
    )
    .await;
    grant_role(&app, &owner_token, server_id, &admin_id, "Administrator", 1).await;

    let until = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let timeout = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/members/{target_id}/timeout"),
            &issuer_token,
            json!({ "until": until, "reason": "repeated spam" }),
        ))
        .await
        .expect("timeout request succeeds");
    assert_eq!(timeout.status(), StatusCode::NO_CONTENT);

    for (token, expected_reason) in [
        (&owner_token, Some("repeated spam")),
        (&issuer_token, None),
        (&target_token, Some("repeated spam")),
        (&ordinary_token, None),
        (&admin_token, Some("repeated spam")),
    ] {
        let response = app
            .clone()
            .oneshot(auth_request(
                Method::GET,
                &format!("/api/v1/servers/{server_id}/members"),
                token,
            ))
            .await
            .expect("member list request succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let member = body["items"]
            .as_array()
            .expect("members array")
            .iter()
            .find(|member| member["account_id"] == target_id)
            .expect("target member is listed");
        assert_eq!(member["timeout_reason"].as_str(), expected_reason);
    }
}
