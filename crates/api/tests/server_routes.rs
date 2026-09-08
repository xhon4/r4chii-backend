
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

/// Creates a server as `token`, returning the parsed response body.
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

#[tokio::test]
async fn creating_a_server_makes_the_creator_the_owner_with_an_invite_code() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;

    assert_eq!(server["name"], "Alice's Place");
    assert_eq!(server["visibility"], "private");
    assert!(server["invite_code"].is_string());
}

#[tokio::test]
async fn creating_a_server_with_an_empty_name_returns_400() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/servers",
            &alice_token,
            json!({ "name": "" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn a_non_member_getting_a_server_gets_404_not_403() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "server_not_found");
}

#[tokio::test]
async fn a_non_member_cannot_create_a_channel_in_a_server_they_are_not_in() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            &bob_token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "server_not_found");
}

#[tokio::test]
async fn a_non_member_cannot_list_channels_in_a_server_they_are_not_in() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/channels"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "server_not_found");
}

#[tokio::test]
async fn list_servers_for_one_account_never_includes_a_server_only_another_account_is_in() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    create_server(&app, &alice_token, "Alice's Place").await;

    let response = app
        .oneshot(auth_request(Method::GET, "/api/v1/servers", &bob_token))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["items"].as_array().expect("items is an array").len(),
        0
    );
}

#[tokio::test]
async fn joining_with_an_invalid_invite_code_returns_404() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/invites/does-not-exist/memberships",
            &bob_token,
            json!({}),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid_invite");
}

#[tokio::test]
async fn joining_a_server_already_a_member_of_returns_409() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let invite_code = server["invite_code"]
        .as_str()
        .expect("owner sees invite code");

    let first_join = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
            json!({}),
        ))
        .await
        .expect("request succeeds");
    assert_eq!(first_join.status(), StatusCode::CREATED);

    let second_join = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
            json!({}),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(second_join.status(), StatusCode::CONFLICT);
    let body = body_json(second_join).await;
    assert_eq!(body["error"]["code"], "already_a_member");
}

#[tokio::test]
async fn invite_code_is_present_for_the_owner_and_absent_for_a_plain_member() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"]
        .as_str()
        .expect("owner sees invite code")
        .to_string();

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
            json!({}),
        ))
        .await
        .expect("bob joins");

    let owner_view = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");
    let owner_body = body_json(owner_view).await;
    assert!(owner_body["invite_code"].is_string());

    let member_view = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");
    let member_body = body_json(member_view).await;
    assert!(member_body["invite_code"].is_null());
}

#[tokio::test]
async fn creating_a_channel_then_listing_channels_returns_the_expected_fields() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let create_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(create_response.status(), StatusCode::CREATED);
    let channel = body_json(create_response).await;
    assert_eq!(channel["name"], "general");
    assert_eq!(channel["kind"], "text");
    assert_eq!(channel["server_id"], server_id);

    let list_response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(list_response.status(), StatusCode::OK);
    let body = body_json(list_response).await;
    let items = body["items"].as_array().expect("items is an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "general");
    assert_eq!(items[0]["id"], channel["id"]);
}

#[tokio::test]
async fn servers_and_channels_routes_require_authentication() {
    let (app, _mail, _hub, _container) = test_app_with_hub().await;

    let response = app
        .oneshot(
            request(Method::GET, "/api/v1/servers")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_members_returns_members_with_roles_and_no_email() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id");
    let invite_code = server["invite_code"].as_str().expect("owner sees code");

    let join = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
        ))
        .await
        .expect("join request succeeds");
    assert_eq!(join.status(), StatusCode::CREATED);

    let response = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/members"),
            &alice_token,
        ))
        .await
        .expect("members request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;

    assert!(body["next_cursor"].is_null());
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);

    let owner = items
        .iter()
        .find(|m| m["account_id"] == alice_id)
        .expect("alice listed");
    assert_eq!(owner["role"], "owner");
    assert_eq!(owner["username"], "alice");
    assert!(
        owner.get("email").is_none(),
        "a member list must never carry another account's email"
    );

    let member = items
        .iter()
        .find(|m| m["account_id"] == bob_id)
        .expect("bob listed");
    assert_eq!(member["role"], "member");
}

#[tokio::test]
async fn list_members_by_a_non_member_returns_404_server_not_found() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_mallory_id, mallory_token) =
        register_and_login(&app, &mail, "mallory@example.com", "mallory").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id");

    let response = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/members"),
            &mallory_token,
        ))
        .await
        .expect("members request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "server_not_found");
}

#[tokio::test]
async fn list_members_without_a_session_is_unauthenticated() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id");

    let response = app
        .clone()
        .oneshot(
            request(
                Method::GET,
                &format!("/api/v1/servers/{server_id}/members"),
            )
            .body(Body::empty())
            .expect("request builds"),
        )
        .await
        .expect("members request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_channel_accepts_an_explicit_voice_kind_and_defaults_to_text() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id");

    // Omitted `kind` still works — every pre-voice client sends exactly this.
    let default_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("create channel succeeds");
    assert_eq!(default_response.status(), StatusCode::CREATED);
    assert_eq!(body_json(default_response).await["kind"], "text");

    let voice_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
            json!({ "name": "General Voice", "kind": "voice" }),
        ))
        .await
        .expect("create channel succeeds");
    assert_eq!(voice_response.status(), StatusCode::CREATED);
    let voice = body_json(voice_response).await;
    assert_eq!(voice["kind"], "voice");
    assert_eq!(voice["server_id"], server_id);
}

#[tokio::test]
async fn create_channel_rejects_an_unsupported_kind_with_400() {
    let (app, mail, _hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id");

    for kind in ["dm", "group_dm", "video"] {
        let response = app
            .clone()
            .oneshot(auth_json_request(
                Method::POST,
                &format!("/api/v1/servers/{server_id}/channels"),
                &alice_token,
                json!({ "name": "nope", "kind": kind }),
            ))
            .await
            .expect("create channel request succeeds");

        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "kind {kind:?} must be rejected"
        );
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "validation_failed");
    }
}

#[tokio::test]
async fn a_blocked_members_status_reports_offline_to_the_blocker_regardless_of_real_presence() {
    // The blocked member holds a live hub connection, so the masked and
    // unmasked answers differ.
    let (app, mail, hub, _container) = test_app_with_hub().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id");
    let invite_code = server["invite_code"].as_str().expect("owner sees code");

    app.clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
        ))
        .await
        .expect("join succeeds");

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &alice_token,
            json!({ "account_id": bob_id }),
        ))
        .await
        .expect("block succeeds");

    let bob_uuid: uuid::Uuid = bob_id.parse().expect("bob id is a uuid");
    let (_handle, _receiver) = hub.register(bob_uuid).await;

    // The mask is directional: bob reads offline to alice, not the reverse.
    let control = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/members"),
            &bob_token,
        ))
        .await
        .expect("members request succeeds");
    let control = body_json(control).await;
    let bob_to_himself = control["items"]
        .as_array()
        .expect("items array")
        .iter()
        .find(|m| m["account_id"] == bob_id)
        .expect("bob listed");
    assert_eq!(
        bob_to_himself["status"], "online",
        "the live connection is real — the blocker's view is what masks it"
    );

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/members"),
            &alice_token,
        ))
        .await
        .expect("members request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let items = body["items"].as_array().expect("items array");
    let bob = items
        .iter()
        .find(|m| m["account_id"] == bob_id)
        .expect("bob listed");
    assert_eq!(bob["status"], "offline");
}
