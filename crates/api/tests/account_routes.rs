use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};
use tower::ServiceExt;
use uuid::Uuid;

async fn test_app() -> (
    axum::Router,
    mailer::CaptureMailer,
    realtime::Hub,
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        // postgres:16, the tag production runs (docker-compose.yml).
        // The crate default is 11-alpine: five majors and a different
        // libc away from the database this schema is deployed on.
        .with_tag("16")
        .start()
        .await
        .expect("postgres container starts");

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = db::build_pool(&database_url)
        .await
        .expect("pool connects");
    db::run_migrations(&pool).await.expect("migrations run");

    let mail = mailer::CaptureMailer::new();
    let domain = domain::DomainService::new(pool.clone());
    let state = api::AppState {
        auth: auth::AuthService::new(pool, std::sync::Arc::new(mail.clone())),
        domain: domain.clone(),
        realtime: realtime::Hub::new(domain),
    };
    let hub = state.realtime.clone();

    (api::router(state), mail, hub, container)
}

// Same rationale as crates/api/tests/auth_routes.rs: the register/login
// routes sit behind tower-governor and need a distinct fake peer per call to
// avoid tripping the burst limit under `oneshot`.
static NEXT_IP_OCTETS: AtomicU32 = AtomicU32::new(1);

fn next_peer_addr() -> SocketAddr {
    let n = NEXT_IP_OCTETS.fetch_add(1, Ordering::Relaxed);
    let ip = Ipv4Addr::new(10, (n >> 16) as u8, (n >> 8) as u8, n as u8);
    SocketAddr::new(IpAddr::V4(ip), 0)
}

fn request(method: Method, uri: &str) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(next_peer_addr()))
}

fn json_request(method: Method, uri: &str, body: Value) -> Request<Body> {
    request(method, uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds")
}

fn auth_json_request(method: Method, uri: &str, token: &str, body: Value) -> Request<Body> {
    request(method, uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("request builds")
}

fn auth_request(method: Method, uri: &str, token: &str) -> Request<Body> {
    request(method, uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request builds")
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("body is valid JSON")
}

fn register_body(email: &str, username: &str) -> Value {
    json!({
        "email": email,
        "username": username,
        "password": "correct horse battery staple",
        "display_name": "Test User",
    })
}

fn login_body(email: &str) -> Value {
    json!({
        "email": email,
        "password": "correct horse battery staple",
    })
}

/// Registers, verifies, and logs in, returning (account_id, bearer_token).
///
/// Goes through the real HTTP flow rather than creating the account directly,
/// so a break in registration surfaces here too instead of only in
/// auth_routes.rs.
async fn register_and_login(
    app: &axum::Router,
    mail: &mailer::CaptureMailer,
    email: &str,
    username: &str,
) -> (String, String) {
    let start = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/registrations",
            register_body(email, username),
        ))
        .await
        .expect("registration request succeeds");
    assert_eq!(start.status(), StatusCode::ACCEPTED);

    let code = mail
        .last()
        .expect("a verification mail was sent")
        .body
        .split_whitespace()
        .find(|word| word.len() == 8 && word.chars().all(|c| c.is_ascii_digit()))
        .expect("the mail carries an 8-digit code")
        .to_string();

    let verify_response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/accounts",
            json!({ "email": email, "code": code }),
        ))
        .await
        .expect("verification request succeeds");
    assert_eq!(verify_response.status(), StatusCode::CREATED);
    let account_body = body_json(verify_response).await;
    let account_id = account_body["id"]
        .as_str()
        .expect("account id present")
        .to_string();

    let login_response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            login_body(email),
        ))
        .await
        .expect("login request succeeds");
    let login_body = body_json(login_response).await;
    let token = login_body["token"]
        .as_str()
        .expect("token present")
        .to_string();

    (account_id, token)
}

async fn create_server(app: &axum::Router, token: &str) -> Value {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/servers",
            token,
            json!({ "name": "Alice's Place" }),
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
        .expect("join server request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn viewing_another_accounts_public_profile_never_includes_email() {
    let (app, mail, _hub, _container) = test_app().await;

    let (alice_id, _alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{alice_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["username"], "alice");
    assert!(
        body.get("email").is_none(),
        "public profile must never include email"
    );
}

/// Direct connection to the same test container's database, for setup that
/// has no HTTP path yet (there is no delete-account endpoint in M1 — only
/// the `deleted_at` column A2 added).
async fn direct_pool(
    container: &testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
) -> db::PgPool {
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    db::build_pool(&database_url).await.expect("pool connects")
}

#[tokio::test]
async fn get_account_for_yourself_by_id_reports_self_relationship() {
    let (app, mail, _hub, _container) = test_app().await;
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{alice_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["relationship"], "self");
}

#[tokio::test]
async fn get_account_with_no_relationship_reports_none() {
    let (app, mail, _hub, _container) = test_app().await;
    let (alice_id, _alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (_bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{alice_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    let body = body_json(response).await;
    assert_eq!(body["relationship"], "none");
}

#[tokio::test]
async fn get_account_after_a_reciprocal_friend_request_reports_friend() {
    let (app, mail, _hub, _container) = test_app().await;
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
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
    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/friends",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("reciprocal request accepts");

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{alice_id}"),
            &bob_token,
        ))
        .await
        .expect("request succeeds");

    let body = body_json(response).await;
    assert_eq!(body["relationship"], "friend");
}

#[tokio::test]
async fn get_account_you_blocked_reports_blocked_relationship() {
    let (app, mail, _hub, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
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
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");

    let body = body_json(response).await;
    assert_eq!(body["relationship"], "blocked");
}

/// The sharpest case: bob blocked alice (inbound from alice's point of
/// view) AND bob has a real live gateway connection registered. The
/// response must still show alice a fully masked, offline profile — proof
/// that the mapping layer honors `decide_profile_visibility`'s
/// `ForcedOffline`/`Hidden` verdict rather than reading the real presence
/// and profile data it has on hand.
#[tokio::test]
async fn an_inbound_block_withholds_only_its_own_existence() {
    let (app, mail, hub, _container) = test_app().await;
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token).await;
    let server_id = server["id"].as_str().expect("server id");
    join_server(
        &app,
        &bob_token,
        server["invite_code"].as_str().expect("invite code"),
    )
    .await;

    let avatar_update = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &bob_token,
            json!({ "avatar_url": "https://example.com/bob.png" }),
        ))
        .await
        .expect("avatar update succeeds");
    assert_eq!(avatar_update.status(), StatusCode::OK);

    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("block request succeeds");

    let bob_uuid: Uuid = bob_id.parse().expect("bob id is a uuid");
    let (_handle, _receiver) = hub.register(bob_uuid).await;

    let response = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{bob_id}?server_id={server_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(
        body["relationship"], "none",
        "the response must never name an inbound block"
    );
    assert_eq!(
        body["avatar_url"], "https://example.com/bob.png",
        "public identity is not redacted to conceal a block"
    );
    assert_eq!(body["presence"]["online"], true);
    assert!(
        body.get("server_context").is_some(),
        "shared membership is already visible in the member list"
    );
}

#[tokio::test]
async fn a_deleted_account_is_returned_as_a_tombstone_not_a_404() {
    let (app, mail, hub, container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let bob_uuid: Uuid = bob_id.parse().expect("bob id is a uuid");
    let (_handle, _receiver) = hub.register(bob_uuid).await;

    let pool = direct_pool(&container).await;
    sqlx::query("UPDATE account SET deleted_at = now() WHERE id = $1")
        .bind(bob_uuid)
        .execute(&pool)
        .await
        .expect("deleted_at updates");

    let response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a deleted account is a tombstone, not a 404 — it must not break message history"
    );
    let body = body_json(response).await;
    assert_eq!(body["display_name"], "Deleted User");
    assert_eq!(body["username"], "bob", "username survives so /users/:username still resolves");
    assert_eq!(body["relationship"], "none");
    assert_eq!(body["presence"]["status"], "offline");
    assert_eq!(body["presence"]["online"], false);
    assert!(body["custom_status"].is_null());
    assert!(body.get("server_context").is_none());
    assert_eq!(body["flags"]["deleted"], true);
}

#[tokio::test]
async fn server_context_requires_shared_membership_and_rejects_invalid_server_ids() {
    let (app, mail, _hub, _container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;
    let (carol_id, carol_token) =
        register_and_login(&app, &mail, "carol@example.com", "carol").await;
    let server = create_server(&app, &alice_token).await;
    let server_id = server["id"].as_str().expect("server id");
    join_server(
        &app,
        &bob_token,
        server["invite_code"].as_str().expect("invite code"),
    )
    .await;

    let cases = [
        (
            format!("/api/v1/accounts/{bob_id}?server_id={server_id}"),
            &carol_token,
        ),
        (
            format!("/api/v1/accounts/{carol_id}?server_id={server_id}"),
            &alice_token,
        ),
        (
            format!("/api/v1/accounts/{bob_id}?server_id={}", Uuid::now_v7()),
            &alice_token,
        ),
    ];
    for (uri, token) in cases {
        let response = app
            .clone()
            .oneshot(auth_request(Method::GET, &uri, token))
            .await
            .expect("profile request succeeds");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_json(response).await.get("server_context").is_none());
    }

    let invalid = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{bob_id}?server_id=abc"),
            &alice_token,
        ))
        .await
        .expect("invalid profile request succeeds");
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
}

/// A live connection AND an `invisible` preference at once: the account is
/// genuinely online, so anything that reports it as such is reading presence
/// without folding the preference in.
#[tokio::test]
async fn an_invisible_account_reads_as_offline_to_someone_else() {
    let (app, mail, hub, container) = test_app().await;
    let (_alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, _bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let bob_uuid: Uuid = bob_id.parse().expect("bob id is a uuid");
    let pool = direct_pool(&container).await;
    sqlx::query("UPDATE account SET status = 'invisible' WHERE id = $1")
        .bind(bob_uuid)
        .execute(&pool)
        .await
        .expect("profile status updates");
    let (_handle, _receiver) = hub.register(bob_uuid).await;

    let profile = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("profile request succeeds");
    assert_eq!(profile.status(), StatusCode::OK);
    let profile = body_json(profile).await;
    assert_eq!(profile["presence"]["status"], "offline");
    assert_eq!(
        profile["presence"]["online"], false,
        "a live connection must not surface through the invisible preference"
    );
}

/// The other half of the rule: `invisible` hides you from third parties, and
/// you are not a third party to yourself. Reporting your own state back as
/// offline would leave the setting invisible to the person who set it.
#[tokio::test]
async fn an_invisible_account_still_sees_its_own_status() {
    let (app, mail, hub, container) = test_app().await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let bob_uuid: Uuid = bob_id.parse().expect("bob id is a uuid");
    let pool = direct_pool(&container).await;
    sqlx::query("UPDATE account SET status = 'invisible' WHERE id = $1")
        .bind(bob_uuid)
        .execute(&pool)
        .await
        .expect("profile status updates");
    let (_handle, _receiver) = hub.register(bob_uuid).await;

    let profile = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{bob_id}"),
            &bob_token,
        ))
        .await
        .expect("profile request succeeds");

    assert_eq!(profile.status(), StatusCode::OK);
    let profile = body_json(profile).await;
    assert_eq!(profile["relationship"], "self");
    assert_eq!(profile["presence"]["status"], "invisible");
    assert_eq!(profile["presence"]["online"], true);
}

#[tokio::test]
async fn get_own_account_returns_the_self_shape_including_email() {
    let (app, mail, _hub, _container) = test_app().await;

    let (_carol_id, carol_token) =
        register_and_login(&app, &mail, "carol@example.com", "carol").await;

    let response = app
        .oneshot(auth_request(Method::GET, "/api/v1/accounts/me", &carol_token))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["username"], "carol");
    assert_eq!(body["email"], "carol@example.com");
}

#[tokio::test]
async fn get_account_for_a_nonexistent_id_returns_404() {
    let (app, mail, _hub, _container) = test_app().await;

    let (_dave_id, dave_token) = register_and_login(&app, &mail, "dave@example.com", "dave").await;

    let response = app
        .oneshot(auth_request(
            Method::GET,
            "/api/v1/accounts/018f3b2a-0000-7000-8000-000000000000",
            &dave_token,
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "account_not_found");
}

#[tokio::test]
async fn patch_accounts_me_updates_only_the_calling_account() {
    let (app, mail, _hub, _container) = test_app().await;

    let (_erin_id, erin_token) = register_and_login(&app, &mail, "erin@example.com", "erin").await;
    let (frank_id, frank_token) = register_and_login(&app, &mail, "frank@example.com", "frank").await;

    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &erin_token,
            json!({ "display_name": "Erin Updated" }),
        ))
        .await
        .expect("patch request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["display_name"], "Erin Updated");
    assert_eq!(body["username"], "erin");

    // frank's own profile is untouched.
    let frank_response = app
        .clone()
        .oneshot(auth_request(Method::GET, "/api/v1/accounts/me", &frank_token))
        .await
        .expect("request succeeds");
    let frank_body = body_json(frank_response).await;
    assert_eq!(frank_body["display_name"], "Test User");

    // frank's public profile (fetched via the id another account would use)
    // is also untouched.
    let frank_public_response = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{frank_id}"),
            &frank_token,
        ))
        .await
        .expect("request succeeds");
    let frank_public_body = body_json(frank_public_response).await;
    assert_eq!(frank_public_body["display_name"], "Test User");
}

#[tokio::test]
async fn patch_accounts_me_updating_one_field_leaves_the_others_unchanged() {
    let (app, mail, _hub, _container) = test_app().await;

    let (_grace_id, grace_token) =
        register_and_login(&app, &mail, "grace@example.com", "grace").await;

    let response = app
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &grace_token,
            json!({ "display_name": "Grace Updated" }),
        ))
        .await
        .expect("patch request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["display_name"], "Grace Updated");
    assert_eq!(body["username"], "grace");
    assert!(body["avatar_url"].is_null());
}

#[tokio::test]
async fn patch_accounts_me_to_a_taken_username_returns_409_and_leaves_the_caller_unmodified() {
    let (app, mail, _hub, _container) = test_app().await;

    let (_heidi_id, _heidi_token) =
        register_and_login(&app, &mail, "heidi@example.com", "heidi").await;
    let (_ivan_id, ivan_token) = register_and_login(&app, &mail, "ivan@example.com", "ivan").await;

    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &ivan_token,
            json!({ "username": "heidi" }),
        ))
        .await
        .expect("patch request succeeds");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "username_taken");

    // ivan's own row must be untouched by the failed update.
    let ivan_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/accounts/me", &ivan_token))
        .await
        .expect("request succeeds");
    let ivan_body = body_json(ivan_response).await;
    assert_eq!(ivan_body["username"], "ivan");
}

#[tokio::test]
async fn get_accounts_id_with_no_token_returns_401() {
    let (app, mail, _hub, _container) = test_app().await;

    let (someone_id, _token) = register_and_login(&app, &mail, "jack@example.com", "jack").await;

    let response = app
        .oneshot(request(
            Method::GET,
            &format!("/api/v1/accounts/{someone_id}"),
        )
        .body(Body::empty())
        .expect("request builds"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn patch_accounts_me_with_no_token_returns_401() {
    let (app, _mail, _hub, _container) = test_app().await;

    let response = app
        .oneshot(json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            json!({ "display_name": "Nope" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// The single behaviour `double_option` in `dto.rs` exists for, exercised
/// through real JSON rather than a hand-built `UpdateAccountInput`.
///
/// A plain `Option<String>` would pass every other test in this file and
/// still fail here: serde collapses an absent key and an explicit `null`
/// into the same `None`, so a bio could be set and never removed.
#[tokio::test]
async fn patch_accounts_me_tells_an_absent_field_apart_from_an_explicit_null() {
    let (app, mail, _hub, _container) = test_app().await;

    let (_heidi_id, heidi_token) = register_and_login(&app, &mail, "heidi@example.com", "heidi").await;

    // Seed the two nullable fields this test then treats differently.
    let seed_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &heidi_token,
            json!({ "bio": "escribo de noche", "pronouns": "she/her" }),
        ))
        .await
        .expect("patch request succeeds");
    assert_eq!(seed_response.status(), StatusCode::OK);
    let seeded = body_json(seed_response).await;
    assert_eq!(seeded["bio"], "escribo de noche");
    assert_eq!(seeded["pronouns"], "she/her");

    // Absent keys: both fields survive a patch that never mentions them.
    let absent_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &heidi_token,
            json!({ "display_name": "Heidi" }),
        ))
        .await
        .expect("patch request succeeds");
    assert_eq!(absent_response.status(), StatusCode::OK);
    let after_absent = body_json(absent_response).await;
    assert_eq!(after_absent["display_name"], "Heidi");
    assert_eq!(after_absent["bio"], "escribo de noche");
    assert_eq!(after_absent["pronouns"], "she/her");

    // Explicit null clears `bio` only — `pronouns` is absent this time and
    // must not be dragged along.
    let null_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &heidi_token,
            json!({ "bio": null }),
        ))
        .await
        .expect("patch request succeeds");
    assert_eq!(null_response.status(), StatusCode::OK);
    let after_null = body_json(null_response).await;
    assert!(after_null["bio"].is_null());
    assert_eq!(after_null["pronouns"], "she/her");

    // And it is the stored row that changed, not just the response body.
    let reread_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/accounts/me", &heidi_token))
        .await
        .expect("request succeeds");
    let reread = body_json(reread_response).await;
    assert!(reread["bio"].is_null());
    assert_eq!(reread["pronouns"], "she/her");
}

#[tokio::test]
async fn patch_accounts_me_rejects_an_invalid_accent_color_and_writes_nothing() {
    let (app, mail, _hub, _container) = test_app().await;

    let (_ivan_id, ivan_token) = register_and_login(&app, &mail, "ivan@example.com", "ivan").await;

    let seed_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &ivan_token,
            json!({ "accent_color": "#AABBCC" }),
        ))
        .await
        .expect("patch request succeeds");
    assert_eq!(seed_response.status(), StatusCode::OK);

    // `red` is a colour, but not the `#RRGGBB` shape the column accepts.
    // The service rejects it before writing, so the earlier value stands.
    let rejected_response = app
        .clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &ivan_token,
            json!({ "accent_color": "red", "display_name": "Ivan Updated" }),
        ))
        .await
        .expect("patch request succeeds");
    assert_eq!(rejected_response.status(), StatusCode::BAD_REQUEST);

    let reread_response = app
        .oneshot(auth_request(Method::GET, "/api/v1/accounts/me", &ivan_token))
        .await
        .expect("request succeeds");
    let reread = body_json(reread_response).await;
    assert_eq!(reread["accent_color"], "#AABBCC");
    // The valid field that travelled alongside the invalid one is not
    // partially applied either.
    assert_eq!(reread["display_name"], "Test User");
}

/// The profile and the member list of a shared server must agree about an
/// account that blocked the caller. They disagreed while the profile redacted
/// media and presence to conceal an inbound block: the member list showed the
/// real values, so diffing the two responses announced the block that the
/// redaction existed to hide.
///
/// Blocking is an interaction boundary, not a secrecy boundary — the profile
/// no longer redacts readable data for it, and this test is what keeps the two
/// surfaces from drifting apart again.
#[tokio::test]
async fn the_profile_and_member_list_agree_about_an_account_that_blocked_you() {
    let (app, mail, _hub, _container) = test_app().await;
    let (alice_id, alice_token) =
        register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let (bob_id, bob_token) = register_and_login(&app, &mail, "bob@example.com", "bob").await;

    let server = create_server(&app, &alice_token).await;
    let server_id = server["id"].as_str().expect("server id");
    join_server(
        &app,
        &bob_token,
        server["invite_code"].as_str().expect("invite code"),
    )
    .await;

    app.clone()
        .oneshot(auth_json_request(
            Method::PATCH,
            "/api/v1/accounts/me",
            &bob_token,
            json!({ "avatar_url": "https://example.com/bob.png" }),
        ))
        .await
        .expect("avatar update succeeds");
    app.clone()
        .oneshot(auth_json_request(
            Method::POST,
            "/api/v1/blocks",
            &bob_token,
            json!({ "account_id": alice_id }),
        ))
        .await
        .expect("block request succeeds");

    let profile = app
        .clone()
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/accounts/{bob_id}"),
            &alice_token,
        ))
        .await
        .expect("profile request succeeds");
    let profile = body_json(profile).await;

    let members = app
        .oneshot(auth_request(
            Method::GET,
            &format!("/api/v1/servers/{server_id}/members"),
            &alice_token,
        ))
        .await
        .expect("member list request succeeds");
    let members = body_json(members).await;
    let member = members["items"]
        .as_array()
        .expect("member list")
        .iter()
        .find(|member| member["account_id"] == bob_id)
        .expect("bob member")
        .clone();

    assert_eq!(
        profile["avatar_url"], member["avatar_url"],
        "a difference between the two surfaces is what discloses the block"
    );
    assert_eq!(profile["avatar_url"], "https://example.com/bob.png");
    assert_eq!(profile["display_name"], member["display_name"]);
    assert_eq!(
        profile["relationship"], "none",
        "agreeing on the data must not extend to naming the block"
    );
}

