use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, Method, Request, StatusCode},
};
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::*;

// The register/login routes sit behind tower-governor's PeerIpKeyExtractor,
// which reads `ConnectInfo<SocketAddr>` from the request extensions (set in
// production by `into_make_service_with_connect_info`, absent by default
// under `oneshot`). Each call below is given a distinct fake peer address so
// tests exercise many rapid register/login calls without tripping the
// secure() preset's 2-requests-per-4-seconds burst limit.

fn empty_request(method: Method, uri: &str) -> Request<Body> {
    request(method, uri)
        .body(Body::empty())
        .expect("request builds")
}

/// Pulls the code out of the most recent captured mail.
fn last_code(mail: &mailer::CaptureMailer) -> String {
    mail.last()
        .expect("a verification mail was sent")
        .body
        .split_whitespace()
        .find(|word| word.len() == 8 && word.chars().all(|c| c.is_ascii_digit()))
        .expect("the mail carries an 8-digit code")
        .to_string()
}

fn verify_body(email: &str, code: &str) -> Value {
    json!({ "email": email, "code": code })
}

/// Drives the two-step signup end to end: start, then verify with the mailed
/// code. Returns the account body.
async fn signup(app: &axum::Router, mail: &mailer::CaptureMailer, email: &str, username: &str) -> Value {
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

    let code = last_code(mail);
    let response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/accounts",
            verify_body(email, &code),
        ))
        .await
        .expect("verification request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);

    body_json(response).await
}

#[tokio::test]
async fn starting_a_registration_returns_202_and_creates_no_account() {
    let (app, mail, _container) = test_app().await;

    let response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/registrations",
            register_body("alice@example.com", "alice"),
        ))
        .await
        .expect("request succeeds");

    // 202, not 201: nothing the caller can go and fetch was created yet.
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(mail.sent().len(), 1, "a verification mail went out");

    // Logging in must not work — there is no account to log into.
    let premature = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            login_body("alice@example.com"),
        ))
        .await
        .expect("login request succeeds");
    assert_eq!(premature.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn verifying_returns_201_with_the_new_account() {
    let (app, mail, _container) = test_app().await;

    let body = signup(&app, &mail, "bob@example.com", "bob").await;

    assert_eq!(body["email"], "bob@example.com");
    assert_eq!(body["username"], "bob");
    assert!(body["email_verified_at"].is_string());
    assert!(body.get("password_hash").is_none());
}

#[tokio::test]
async fn a_wrong_code_returns_401_without_saying_why() {
    let (app, mail, _container) = test_app().await;

    app.clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/registrations",
            register_body("carol@example.com", "carol"),
        ))
        .await
        .expect("registration request succeeds");
    let real_code = last_code(&mail);
    let wrong_code = if real_code == "00000000" {
        "11111111"
    } else {
        "00000000"
    };

    let response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/accounts",
            verify_body("carol@example.com", wrong_code),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid_verification_code");

    // An address nobody registered fails identically, so the response cannot
    // be used to discover who is mid-signup.
    let unknown = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/accounts",
            verify_body("nobody@example.com", wrong_code),
        ))
        .await
        .expect("request succeeds");
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    let unknown_body = body_json(unknown).await;
    assert_eq!(unknown_body["error"]["code"], "invalid_verification_code");
}

#[tokio::test]
async fn registering_a_known_address_is_indistinguishable_from_a_fresh_one() {
    let (app, mail, _container) = test_app().await;

    signup(&app, &mail, "dave@example.com", "dave").await;
    mail.clear();

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/registrations",
            register_body("dave@example.com", "dave_two"),
        ))
        .await
        .expect("request succeeds");

    // Same status as a fresh address. Reporting the clash here would turn
    // registration into an oracle for "is this address registered".
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(
        mail.sent().is_empty(),
        "but no mail goes to an address that already has an account"
    );
}

#[tokio::test]
async fn a_taken_username_is_reported_plainly() {
    let (app, mail, _container) = test_app().await;

    signup(&app, &mail, "erin@example.com", "erin").await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/registrations",
            register_body("erin2@example.com", "erin"),
        ))
        .await
        .expect("request succeeds");

    // Usernames are public — they appear in every member list — so saying so
    // leaks nothing, and silence would leave the caller waiting for a mail
    // that is never coming.
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "username_taken");
}

#[tokio::test]
async fn resending_a_code_returns_202_even_for_an_unknown_address() {
    let (app, mail, _container) = test_app().await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/registration-codes",
            json!({ "email": "nobody@example.com" }),
        ))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(
        mail.sent().is_empty(),
        "nothing is sent for an address with no pending registration"
    );
}

#[tokio::test]
async fn login_with_correct_credentials_returns_201_with_cookie_and_token() {
    let (app, mail, _container) = test_app().await;

    signup(&app, &mail, "frank@example.com", "frank").await;

    let login_response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            login_body("frank@example.com"),
        ))
        .await
        .expect("login request succeeds");

    assert_eq!(login_response.status(), StatusCode::CREATED);
    let set_cookie = login_response
        .headers()
        .get(header::SET_COOKIE)
        .expect("Set-Cookie header present")
        .to_str()
        .expect("Set-Cookie header is valid UTF-8")
        .to_string();
    assert!(set_cookie.contains("r4chii_session="));
    assert!(set_cookie.to_ascii_lowercase().contains("httponly"));

    let body = body_json(login_response).await;
    assert!(body["token"].as_str().is_some_and(|t| !t.is_empty()));
}

#[tokio::test]
async fn login_with_wrong_password_returns_401_invalid_credentials() {
    let (app, mail, _container) = test_app().await;

    signup(&app, &mail, "grace@example.com", "grace").await;

    let response = app
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            json!({
                "email": "grace@example.com",
                "password": "totally wrong password",
            }),
        ))
        .await
        .expect("login request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "invalid_credentials");
}

#[tokio::test]
async fn protected_route_with_no_token_returns_401() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(empty_request(Method::GET, "/api/v1/sessions"))
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_route_with_a_valid_token_returns_200_scoped_to_that_account() {
    let (app, mail, _container) = test_app().await;

    signup(&app, &mail, "heidi@example.com", "heidi").await;

    let login_response = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            login_body("heidi@example.com"),
        ))
        .await
        .expect("login request succeeds");
    let login_body = body_json(login_response).await;
    let token = login_body["token"]
        .as_str()
        .expect("token present")
        .to_string();

    let response = app
        .oneshot(
            request(Method::GET, "/api/v1/sessions")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let items = body["items"].as_array().expect("items is an array");
    assert_eq!(items.len(), 1, "heidi should see exactly her own session");
}

#[tokio::test]
async fn deleting_someone_elses_session_returns_404() {
    let (app, mail, _container) = test_app().await;

    signup(&app, &mail, "ivan@example.com", "ivan").await;
    signup(&app, &mail, "judy@example.com", "judy").await;

    let ivan_login = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            login_body("ivan@example.com"),
        ))
        .await
        .expect("login request succeeds");
    let ivan_body = body_json(ivan_login).await;
    let ivan_session_id = ivan_body["id"]
        .as_str()
        .expect("session id present")
        .to_string();

    let judy_login = app
        .clone()
        .oneshot(json_request(
            Method::POST,
            "/api/v1/sessions",
            login_body("judy@example.com"),
        ))
        .await
        .expect("login request succeeds");
    let judy_body = body_json(judy_login).await;
    let judy_token = judy_body["token"]
        .as_str()
        .expect("token present")
        .to_string();

    // judy tries to delete ivan's session by id.
    let response = app
        .oneshot(
            request(Method::DELETE, &format!("/api/v1/sessions/{ivan_session_id}"))
                .header(header::AUTHORIZATION, format!("Bearer {judy_token}"))
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repeated_registration_attempts_from_the_same_peer_are_rate_limited() {
    let (app, _mail, _container) = test_app().await;

    // secure() preset: burst of 2, replenished every 4s. Reuse a single fake
    // peer address (unlike the other tests, which vary it per call
    // specifically to avoid tripping this limit) so the 3rd rapid request is
    // expected to be rejected.
    //
    // This is the per-IP limiter. The per-address gap that stops one inbox
    // being flooded lives in `auth`, because this layer runs before the body
    // is parsed and never sees which address a request names.
    // The body is deliberately invalid. This limiter runs before the body is
    // parsed, so a rejected request still spends a cell, and the quota is
    // reached without paying three password hashes and three inserts — work
    // that can outlast the 4s replenish and hand the third request a fresh
    // cell, which is a property of the harness rather than of the limiter.
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 0);
    let build = || {
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/registrations")
            .extension(ConnectInfo(peer))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .expect("request builds")
    };

    let first = app
        .clone()
        .oneshot(build())
        .await
        .expect("request succeeds");
    let second = app
        .clone()
        .oneshot(build())
        .await
        .expect("request succeeds");
    let third = app.oneshot(build()).await.expect("request succeeds");

    assert_eq!(first.status(), StatusCode::BAD_REQUEST);
    assert_eq!(second.status(), StatusCode::BAD_REQUEST);
    assert_eq!(third.status(), StatusCode::TOO_MANY_REQUESTS);

    let body = body_json(third).await;
    assert_eq!(body["error"]["code"], "rate_limited");
}
