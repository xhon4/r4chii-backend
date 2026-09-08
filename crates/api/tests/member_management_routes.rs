//! M2: kick, ban, leave, delete-server.
//! Same harness as `server_routes.rs`/`role_routes.rs`.

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

#[tokio::test]
async fn a_member_can_leave_a_server_they_are_not_the_owner_of() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let response = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/leave"),
            &bob_token,
        ))
        .await
        .expect("leave request succeeds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Bob is really gone: he can rejoin fresh via the invite rather than
    // hitting `already_a_member`.
    let rejoin = app
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
        ))
        .await
        .expect("rejoin request succeeds");
    assert_eq!(rejoin.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn the_owner_cannot_leave_their_own_server() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    let response = app
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/leave"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "owner_cannot_leave");
}

#[tokio::test]
async fn the_owner_can_kick_a_plain_member() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("kick request succeeds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Bob really lost access: the server now 404s for him.
    let get_server = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");
    assert_eq!(get_server.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_plain_member_without_kick_cannot_kick_anyone() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (_carol_id, carol_token) = register_and_login(&app, &mail, "carol@example.com", "carol").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;
    join_server(&app, &carol_token, invite_code).await;

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}/members/{bob_id}"),
            &carol_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "missing_permission");
}

#[tokio::test]
async fn nobody_can_kick_the_owner() {
    let (app, mail, _container) = test_app().await;
    let (alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");

    // Alice holds no KICK bit and is targeting herself, so this should fail
    // on `missing_permission` before it ever reaches the owner check — a
    // more useful assertion is a member WITH kick trying to kick the owner.
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let roles = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/roles"),
            &alice_token,
        ))
        .await
        .expect("list roles succeeds");
    let roles_body = body_json(roles).await;
    let default_role_id = roles_body["items"][0]["id"].as_str().expect("default role id");

    const KICK: i64 = 1 << 3;
    app.clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            &format!("/api/v1/servers/{server_id}/roles/{default_role_id}"),
            &alice_token,
            json!({ "permissions": KICK }),
        ))
        .await
        .expect("grant request succeeds");

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}/members/{alice_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "cannot_act_on_owner");
    let _ = bob_id;
}

#[tokio::test]
async fn banning_a_member_prevents_rejoining_and_lists_them_as_banned() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let ban = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/bans"),
            &alice_token,
            json!({ "account_id": bob_id, "reason": "spamming" }),
        ))
        .await
        .expect("ban request succeeds");
    assert_eq!(ban.status(), StatusCode::CREATED);
    let ban_body = body_json(ban).await;
    assert_eq!(ban_body["account_id"], bob_id);
    assert_eq!(ban_body["reason"], "spamming");

    let rejoin = app
        .clone()
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
        ))
        .await
        .expect("rejoin attempt succeeds");
    assert_eq!(rejoin.status(), StatusCode::FORBIDDEN);
    let rejoin_body = body_json(rejoin).await;
    assert_eq!(rejoin_body["error"]["code"], "banned_from_server");

    let bans = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/bans"),
            &alice_token,
        ))
        .await
        .expect("list bans succeeds");
    let bans_body = body_json(bans).await;
    assert_eq!(bans_body["items"].as_array().expect("items array").len(), 1);

    // Unban, then the invite works again.
    let unban = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}/bans/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("unban request succeeds");
    assert_eq!(unban.status(), StatusCode::NO_CONTENT);

    let rejoin_again = app
        .oneshot(auth_request(
            Method::POST,
            &format!("/api/v1/invites/{invite_code}/memberships"),
            &bob_token,
        ))
        .await
        .expect("second rejoin attempt succeeds");
    assert_eq!(rejoin_again.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn banning_twice_returns_409() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/bans"),
            &alice_token,
            json!({ "account_id": bob_id, "reason": null }),
        ))
        .await
        .expect("first ban succeeds");

    // Rejoin isn't possible after a ban, so there's nothing to kick a second
    // time — banning again should still cleanly report "already banned"
    // rather than an account-not-found from the (already-removed) member
    // lookup.
    let response = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/bans"),
            &alice_token,
            json!({ "account_id": bob_id, "reason": null }),
        ))
        .await
        .expect("second ban attempt succeeds");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "already_banned");
}

#[tokio::test]
async fn deleting_a_server_cascades_and_removes_it_for_everyone() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let channel = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/servers/{server_id}/channels"),
            &alice_token,
            json!({ "name": "general" }),
        ))
        .await
        .expect("create channel succeeds");
    let channel_body = body_json(channel).await;
    let channel_id = channel_body["id"].as_str().expect("channel id present").to_string();

    let response = app
        .clone()
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}"),
            &alice_token,
        ))
        .await
        .expect("delete server request succeeds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Gone for the owner too — not just 403, genuinely 404.
    let get_server = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");
    assert_eq!(get_server.status(), StatusCode::NOT_FOUND);

    // The channel really cascaded — sending to it now 404s rather than
    // orphaning the row.
    let send = app
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/messages"),
            &alice_token,
            json!({ "content": "hello?" }),
        ))
        .await
        .expect("request succeeds");
    assert_eq!(send.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_non_owner_cannot_delete_the_server() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let server = create_server(&app, &alice_token, "Alice's Place").await;
    let server_id = server["id"].as_str().expect("server id present");
    let invite_code = server["invite_code"].as_str().expect("invite code present");
    join_server(&app, &bob_token, invite_code).await;

    let response = app
        .oneshot(auth_request(
            Method::DELETE,
            &format!("/api/v1/servers/{server_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "missing_permission");
}
