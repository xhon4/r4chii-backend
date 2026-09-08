
use axum::http::{Method, StatusCode};
use serde_json::json;
use tower::ServiceExt;

mod common;
use common::*;

#[tokio::test]
async fn blocking_an_account_returns_201() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["account_id"], bob_id);
}

#[tokio::test]
async fn blocking_the_same_account_twice_returns_200_the_second_time() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("first block succeeds");

    let second = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("second block succeeds");

    assert_eq!(second.status(), StatusCode::OK);
}

#[tokio::test]
async fn listing_blocks_shows_only_the_callers_own_blocks() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("block succeeds");
    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("block succeeds");

    let response = app
        .oneshot(auth_request(Method::GET, "/api/v1/blocks", &alice_token))
        .await
        .expect("list request succeeds");
    let body = body_json(response).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["account_id"], bob_id);
}

#[tokio::test]
async fn unblocking_returns_204_and_it_no_longer_lists() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("block succeeds");

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

    let list_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/blocks", &alice_token))
        .await
        .expect("list request succeeds");
    let body = body_json(list_response).await;
    assert!(body["items"].as_array().expect("items array").is_empty());
}

#[tokio::test]
async fn unblocking_a_nonexistent_block_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/blocks/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("unblock request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "block_not_found");
}

#[tokio::test]
async fn a_block_prevents_creating_a_dm() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("block succeeds");

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("dm request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "blocked");
}

#[tokio::test]
async fn create_block_with_no_token_returns_401() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/blocks",
            json!({ "account_id": "018f0000-0000-7000-8000-000000000000" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
