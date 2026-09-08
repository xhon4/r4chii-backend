
use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::*;

// Same rationale as crates/api/tests/auth_routes.rs: the register/login
// routes sit behind tower-governor and need a distinct fake peer per call to
// avoid tripping the burst limit under `oneshot`.

/// Creates a server + one text channel as `token`, returning
/// (server_id, invite_code, channel_id).
async fn server_and_channel(app: &axum::Router, token: &str) -> (String, String, String) {
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
    assert_eq!(server_response.status(), StatusCode::CREATED);
    let server = body_json(server_response).await;
    let server_id = server["id"].as_str().expect("server id present").to_string();
    let invite_code = server["invite_code"]
        .as_str()
        .expect("owner sees invite code")
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
    assert_eq!(channel_response.status(), StatusCode::CREATED);
    let channel = body_json(channel_response).await;
    let channel_id = channel["id"]
        .as_str()
        .expect("channel id present")
        .to_string();

    (server_id, invite_code, channel_id)
}

async fn join(app: &axum::Router, token: &str, invite_code: &str) {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            token,
            json!({}),
        ))
        .await
        .expect("join request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
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

#[tokio::test]
async fn sending_a_message_then_listing_it_returns_the_expected_fields() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_server_id, _invite, channel_id) = server_and_channel(&app, &alice_token).await;

    let message = send_message(&app, &alice_token, &channel_id, "hello world").await;
    assert_eq!(message["content"], "hello world");
    assert_eq!(message["channel_id"], channel_id);
    assert!(message["deleted_at"].is_null());

    let list_response = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
        ))
        .await
        .expect("list request succeeds");
    assert_eq!(list_response.status(), StatusCode::OK);
    let body = body_json(list_response).await;
    let items = body["items"].as_array().expect("items is an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], message["id"]);
    // Fewer results than a full default page -> no more to page to.
    assert!(body["next_cursor"].is_null());
}

#[tokio::test]
async fn sending_an_empty_message_returns_400() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_server_id, _invite, channel_id) = server_and_channel(&app, &alice_token).await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
            json!({ "content": "" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn a_non_member_cannot_send_list_edit_or_delete_messages_in_a_channel() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (_server_id, _invite, channel_id) = server_and_channel(&app, &alice_token).await;

    let message = send_message(&app, &alice_token, &channel_id, "alice's message").await;
    let message_id = message["id"].as_str().expect("message id present");

    let send_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &bob_token,
            json!({ "content": "intruder" }),
        ))
        .await
        .expect("request succeeds");
    assert_eq!(send_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(send_response).await["error"]["code"],
        "channel_not_found"
    );

    let list_response = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");
    assert_eq!(list_response.status(), StatusCode::NOT_FOUND);

    let edit_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &bob_token,
            json!({ "content": "edited" }),
        ))
        .await
        .expect("request succeeds");
    assert_eq!(edit_response.status(), StatusCode::NOT_FOUND);

    let delete_response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");
    assert_eq!(delete_response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn editing_someone_elses_message_returns_403() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (_server_id, invite_code, channel_id) = server_and_channel(&app, &alice_token).await;
    join(&app, &bob_token, &invite_code).await;

    let message = send_message(&app, &alice_token, &channel_id, "alice's message").await;
    let message_id = message["id"].as_str().expect("message id present");

    let response = app
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &bob_token,
            json!({ "content": "bob was here" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "not_message_author");
}

#[tokio::test]
async fn deleting_someone_elses_message_returns_403() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (_server_id, invite_code, channel_id) = server_and_channel(&app, &alice_token).await;
    join(&app, &bob_token, &invite_code).await;

    let message = send_message(&app, &alice_token, &channel_id, "alice's message").await;
    let message_id = message["id"].as_str().expect("message id present");

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    // Deleting someone else's message in a SERVER channel now
    // checks MANAGE_MESSAGES before falling back to "not the author" — Bob
    // has neither, so the more specific `missing_permission` is reported
    // (still 403). `not_message_author` stays the outcome for dm/group_dm
    // channels, which have no roles to hold that bit in. See
    // `role_permissions_v2_routes.rs` for the MANAGE_MESSAGES-holder-succeeds
    // path.
    assert_eq!(body["error"]["code"], "missing_permission");
}

#[tokio::test]
async fn editing_or_deleting_a_nonexistent_message_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_server_id, _invite, channel_id) = server_and_channel(&app, &alice_token).await;
    let bogus_message_id = "018f0000-0000-7000-8000-000000000000";

    let edit_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/channels/{channel_id}/messages/{bogus_message_id}"),
            &alice_token,
            json!({ "content": "edited" }),
        ))
        .await
        .expect("request succeeds");
    assert_eq!(edit_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(edit_response).await["error"]["code"],
        "message_not_found"
    );

    let delete_response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/channels/{channel_id}/messages/{bogus_message_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");
    assert_eq!(delete_response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(delete_response).await["error"]["code"],
        "message_not_found"
    );
}

#[tokio::test]
async fn a_soft_deleted_message_stays_in_the_list_with_null_content() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_server_id, _invite, channel_id) = server_and_channel(&app, &alice_token).await;

    let message = send_message(&app, &alice_token, &channel_id, "to be deleted").await;
    let message_id = message["id"].as_str().expect("message id present");

    let delete_response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/channels/{channel_id}/messages/{message_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");
    assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);

    let list_response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");
    let body = body_json(list_response).await;
    let items = body["items"].as_array().expect("items is an array");
    assert_eq!(items.len(), 1, "the soft-deleted message must stay in the list");
    assert_eq!(items[0]["id"], message_id);
    assert!(items[0]["content"].is_null());
    assert!(!items[0]["deleted_at"].is_null());
}

#[tokio::test]
async fn cursor_pagination_pages_backward_with_a_real_next_cursor() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_server_id, _invite, channel_id) = server_and_channel(&app, &alice_token).await;

    let mut sent_ids = Vec::new();
    for i in 0..7 {
        let message = send_message(&app, &alice_token, &channel_id, &format!("message {i}")).await;
        sent_ids.push(message["id"].as_str().expect("id present").to_string());
    }
    let expected_newest_first: Vec<String> = sent_ids.into_iter().rev().collect();

    // First page: 5 of 7 -> full page -> next_cursor must be set.
    let first_response = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/messages?limit=5"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");
    let first_body = body_json(first_response).await;
    let first_items = first_body["items"].as_array().expect("items is an array");
    assert_eq!(first_items.len(), 5);
    let first_ids: Vec<String> = first_items
        .iter()
        .map(|m| m["id"].as_str().expect("id present").to_string())
        .collect();
    assert_eq!(first_ids, expected_newest_first[0..5]);
    let cursor = first_body["next_cursor"]
        .as_str()
        .expect("a full page has a next_cursor")
        .to_string();
    assert_eq!(cursor, first_ids[4]);

    // Second page: remaining 2 -> short page -> next_cursor is null.
    let second_response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/messages?limit=5&before={cursor}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");
    let second_body = body_json(second_response).await;
    let second_items = second_body["items"].as_array().expect("items is an array");
    assert_eq!(second_items.len(), 2);
    let second_ids: Vec<String> = second_items
        .iter()
        .map(|m| m["id"].as_str().expect("id present").to_string())
        .collect();
    assert_eq!(second_ids, expected_newest_first[5..7]);
    assert!(second_body["next_cursor"].is_null());
}

#[tokio::test]
async fn message_routes_require_authentication() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(
            request(Method::GET, "/api/v1/channels/00000000-0000-0000-0000-000000000000/messages")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
