//! Full export HTTP surface — `POST`/`GET .../servers/{id}/exports`.
//! Same harness as `role_routes.rs`. Only covers job creation/polling — the
//! worker itself needs real S3-compatible storage, which this sandbox does
//! not have (see `crates/domain/tests/export_service.rs`'s own note).

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

async fn join_server(app: &axum::Router, token: &str, invite_code: &str) {
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
}

#[tokio::test]
async fn the_owner_can_request_and_poll_an_export() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let create_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/exports"),
            &alice_token,
            json!({}),
        ))
        .await
        .expect("request export succeeds");
    assert_eq!(create_response.status(), StatusCode::ACCEPTED);
    let job = body_json(create_response).await;
    assert_eq!(job["status"], "pending");
    assert_eq!(job["download_url"], Value::Null);
    let job_id = job["id"].as_str().expect("job id present");

    let poll_response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/exports/{job_id}"),
            &alice_token,
        ))
        .await
        .expect("poll export succeeds");
    assert_eq!(poll_response.status(), StatusCode::OK);
    let polled = body_json(poll_response).await;
    assert_eq!(polled["id"], job_id);
    assert_eq!(polled["status"], "pending");
}

#[tokio::test]
async fn a_plain_member_without_admin_cannot_request_an_export() {
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
            &format!("/api/v1/servers/{server_id}/exports"),
            &bob_token,
            json!({}),
        ))
        .await
        .expect("request export succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "missing_permission");
}

#[tokio::test]
async fn polling_a_nonexistent_export_job_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let fake_job_id = app_core::new_id();

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/exports/{fake_job_id}"),
            &alice_token,
        ))
        .await
        .expect("poll export succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "export_job_not_found");
}
