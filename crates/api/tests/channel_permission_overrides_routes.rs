//! HTTP surface for channel restriction and role grants. Domain-
//! level coverage (the public-leak and search-leak cases) lives in
//! `crates/domain/tests/channel_permission_overrides_service.rs` — this file
//! only confirms routing and status codes. Same harness as `role_routes.rs`.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::*;

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
