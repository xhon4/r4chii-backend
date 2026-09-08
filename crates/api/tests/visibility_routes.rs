//! Visibility model HTTP surface — `PATCH .../visibility` for
//! servers and channels. Same harness as `role_routes.rs`.

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::*;

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

#[tokio::test]
async fn the_owner_can_narrow_a_channel_below_a_public_server() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place", Some("public")).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "staff").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let response = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/visibility"),
            &alice_token,
            json!({ "visibility": "private" }),
        ))
        .await
        .expect("update channel visibility request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["visibility"], "private");
}

#[tokio::test]
async fn a_channel_visibility_broader_than_its_server_is_rejected() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    // Default visibility is private.
    let server = create_server(&app, &alice_token, "Alice's Place", None).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let response = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/visibility"),
            &alice_token,
            json!({ "visibility": "public" }),
        ))
        .await
        .expect("update channel visibility request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn a_plain_member_without_manage_visibility_cannot_update_channel_visibility() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place", Some("public")).await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let response = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/channels/{channel_id}/visibility"),
            &bob_token,
            json!({ "visibility": "private" }),
        ))
        .await
        .expect("update channel visibility request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "missing_permission");
}

#[tokio::test]
async fn only_the_owner_can_flip_server_visibility() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place", None).await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let denied = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/visibility"),
            &bob_token,
            json!({ "visibility": "public" }),
        ))
        .await
        .expect("update server visibility request succeeds");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let allowed = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/visibility"),
            &alice_token,
            json!({ "visibility": "public" }),
        ))
        .await
        .expect("update server visibility request succeeds");
    assert_eq!(allowed.status(), StatusCode::OK);
    let body = body_json(allowed).await;
    assert_eq!(body["visibility"], "public");
}
