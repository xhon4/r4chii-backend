//! Threads HTTP surface — `POST`/`GET .../channels/{id}/threads`.
//! Same harness as `role_routes.rs`.

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
async fn a_member_can_create_and_list_threads_under_a_channel() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let create_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/threads"),
            &alice_token,
            json!({ "title": "How do I configure X?" }),
        ))
        .await
        .expect("create thread request succeeds");
    assert_eq!(create_response.status(), StatusCode::CREATED);
    let thread = body_json(create_response).await;
    assert_eq!(thread["kind"], "thread");
    assert_eq!(thread["parent_channel_id"], channel_id);
    assert_eq!(thread["title"], "How do I configure X?");

    let list_response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/threads"),
            &alice_token,
        ))
        .await
        .expect("list threads request succeeds");
    assert_eq!(list_response.status(), StatusCode::OK);
    let body = body_json(list_response).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], thread["id"]);
}

#[tokio::test]
async fn a_non_member_cannot_create_a_thread_and_gets_a_non_leaking_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/threads"),
            &bob_token,
            json!({ "title": "intruder" }),
        ))
        .await
        .expect("create thread request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "channel_not_found");
}

#[tokio::test]
async fn messages_can_be_sent_inside_a_thread_over_http() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");

    let thread_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/threads"),
            &alice_token,
            json!({ "title": "a topic" }),
        ))
        .await
        .expect("create thread request succeeds");
    let thread = body_json(thread_response).await;
    let thread_id = thread["id"].as_str().expect("thread id present");

    let message_response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{thread_id}/messages"),
            &alice_token,
            json!({ "content": "first reply" }),
        ))
        .await
        .expect("send message request succeeds");
    assert_eq!(message_response.status(), StatusCode::CREATED);
    let message = body_json(message_response).await;
    assert_eq!(message["channel_id"], thread_id);
}

