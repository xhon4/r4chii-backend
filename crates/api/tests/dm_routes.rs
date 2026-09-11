
use axum::http::{Method, StatusCode};
use serde_json::json;
use tower::ServiceExt;

mod common;
use common::*;

#[tokio::test]
async fn creating_a_dm_then_creating_it_again_is_idempotent() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let first = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm request succeeds");
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = body_json(first).await;
    assert!(first_body["server_id"].is_null());
    assert_eq!(first_body["kind"], "dm");
    let channel_id = first_body["id"].as_str().expect("channel id present").to_string();

    let second = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm request succeeds");
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = body_json(second).await;
    assert_eq!(second_body["id"], channel_id);
}

#[tokio::test]
async fn starting_a_dm_with_yourself_returns_400() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
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
async fn starting_a_dm_with_a_nonexistent_account_returns_404() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let bogus_account_id = "018f0000-0000-7000-8000-000000000000";

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bogus_account_id }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "account_not_found");
}

#[tokio::test]
async fn a_dm_and_messages_sent_in_it_can_be_listed_by_both_participants() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let dm_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm request succeeds");
    let dm = body_json(dm_response).await;
    let channel_id = dm["id"].as_str().expect("channel id present");

    let send_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
            json!({ "content": "hey bob" }),
        ))
        .await
        .expect("send message request succeeds");
    assert_eq!(send_response.status(), StatusCode::CREATED);

    let bob_list = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &bob_token,
        ))
        .await
        .expect("list request succeeds");
    assert_eq!(bob_list.status(), StatusCode::OK);
    let body = body_json(bob_list).await;
    assert_eq!(body["items"].as_array().expect("items array").len(), 1);

    let list_dms_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/dms", &alice_token))
        .await
        .expect("list dms request succeeds");
    assert_eq!(list_dms_response.status(), StatusCode::OK);
    let dms_body = body_json(list_dms_response).await;
    let items = dms_body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], channel_id);
}

#[tokio::test]
async fn creating_a_group_dm_requires_at_least_two_other_participants() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/group-dms",
            &alice_token,
            json!({ "account_ids": [bob_id] }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn creating_a_group_dm_with_three_accounts_lets_all_of_them_message_in_it() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (carol_id, carol_token) = register_and_login(&app, &mail, "carol@example.com", "carol").await;

    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/group-dms",
            &alice_token,
            json!({ "account_ids": [bob_id, carol_id] }),
        ))
        .await
        .expect("create group dm request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    let group = body_json(response).await;
    assert_eq!(group["kind"], "group_dm");
    let channel_id = group["id"].as_str().expect("channel id present");

    for token in [&bob_token, &carol_token] {
        let list_response = app
            .clone()
            .oneshot(auth_request(
                Method::GET,
                &format!("/api/v1/channels/{channel_id}/messages"),
                token,
            ))
            .await
            .expect("list request succeeds");
        assert_eq!(list_response.status(), StatusCode::OK);
    }
}

#[tokio::test]
async fn create_dm_with_no_token_returns_401() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/dms",
            json!({ "account_id": "018f0000-0000-7000-8000-000000000000" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// A DM has no `name`, so `participants` is what a client labels it with.
// Bob never called POST /dms — his listing is the only place the roster can
// come from.
#[tokio::test]
async fn listing_dms_exposes_the_roster_to_a_participant_who_did_not_create_it() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let dm_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/dms",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("create dm request succeeds");
    let dm = body_json(dm_response).await;
    let channel_id = dm["id"].as_str().expect("channel id present").to_string();

    let created_participants = dm["participants"]
        .as_array()
        .expect("the create response carries the roster");
    assert_eq!(created_participants.len(), 2);

    let list_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/dms", &bob_token))
        .await
        .expect("list dms request succeeds");
    assert_eq!(list_response.status(), StatusCode::OK);
    let body = body_json(list_response).await;

    let listed = &body["items"][0];
    assert_eq!(listed["id"], channel_id.as_str());
    let participants = listed["participants"]
        .as_array()
        .expect("a listed dm carries its roster");
    assert_eq!(participants.len(), 2);
    assert!(participants.iter().any(|id| id == &alice_id));
    assert!(participants.iter().any(|id| id == &bob_id));
}

// `participants` is a DM-only concept — a server channel must not grow one,
// or the field turns into a roster leak on every channel listing.
#[tokio::test]
async fn a_server_channel_listing_carries_no_participants_field() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let server = body_json(
        app.clone()
            .oneshot(auth_json_request(
                Method::POST,
                "/api/v1/servers",
                &alice_token,
                json!({ "name": "Alice's Place" }),
            ))
            .await
            .expect("create server request succeeds"),
    )
    .await;
    let server_id = server["id"].as_str().expect("server id present").to_string();

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("create channel request succeeds");

    let body = body_json(
        app.oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
        ))
        .await
        .expect("list channels request succeeds"),
    )
    .await;

    let items = body["items"].as_array().expect("items array");
    assert!(!items.is_empty());
    for channel in items {
        assert!(channel["participants"].is_null());
    }
}
