
use axum::http::{Method, StatusCode};
use serde_json::json;
use tower::ServiceExt;

mod common;
use common::*;

#[tokio::test]
async fn sending_a_friend_request_returns_201_pending() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    assert_eq!(body["status"], "pending");
    assert_eq!(body["account_id"], bob_id);
}

#[tokio::test]
async fn a_reciprocal_request_accepts_and_returns_201() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    let accept_response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("accept succeeds");

    assert_eq!(accept_response.status(), StatusCode::CREATED);
    let body = body_json(accept_response).await;
    assert_eq!(body["status"], "accepted");
}

#[tokio::test]
async fn sending_a_friend_request_to_yourself_returns_400() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn listing_friendships_shows_both_pending_and_accepted() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (carol_id, _carol_token) = register_and_login(&app, &mail, "carol@example.com", "carol").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");
    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("accept succeeds");
    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": carol_id }),
        ))
        .await
        .expect("request succeeds");

    let list_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/friends", &alice_token))
        .await
        .expect("list request succeeds");
    assert_eq!(list_response.status(), StatusCode::OK);
    let body = body_json(list_response).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn removing_a_friendship_returns_204_and_it_no_longer_lists() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    let remove_response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/friends/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("remove request succeeds");
    assert_eq!(remove_response.status(), StatusCode::NO_CONTENT);

    let list_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/friends", &alice_token))
        .await
        .expect("list request succeeds");
    let body = body_json(list_response).await;
    assert!(body["items"].as_array().expect("items array").is_empty());
}

#[tokio::test]
async fn removing_a_nonexistent_friendship_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/friends/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("remove request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "friend_request_not_found");
}

#[tokio::test]
async fn a_friend_request_between_a_blocked_pair_returns_403() {
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
        .expect("block request succeeds");

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "blocked");
}

#[tokio::test]
async fn send_friend_request_with_no_token_returns_401() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/friends",
            json!({ "account_id": "018f0000-0000-7000-8000-000000000000" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
