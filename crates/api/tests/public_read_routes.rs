//! Public read path HTTP surface — `/archive/t/{id}`,
//! `/archive/sitemap.xml`, `/robots.txt`. Same harness as `role_routes.rs`,
//! plus a few unauthenticated requests (no `Authorization` header).

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

mod common;
use common::*;

fn plain_request(method: Method, uri: &str) -> Request<Body> {
    request(method, uri).body(Body::empty()).expect("request builds")
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.expect("body collects").to_bytes();
    String::from_utf8(bytes.to_vec()).expect("body is valid utf-8")
}

async fn create_server(app: &axum::Router, token: &str, name: &str, visibility: Option<&str>) -> Value {
    let mut body = json!({ "name": name });
    if let Some(visibility) = visibility {
        body["visibility"] = json!(visibility);
    }
    let response = app
        .clone()
        .oneshot(auth_json_request(Method::POST, "/api/v1/servers", token, body))
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

async fn create_thread(app: &axum::Router, token: &str, channel_id: &str, title: &str) -> Value {
    let response = app
        .clone()
        .oneshot(auth_json_request(
            Method::POST,
            &format!("/api/v1/channels/{channel_id}/threads"),
            token,
            json!({ "title": title }),
        ))
        .await
        .expect("create thread request succeeds");
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await
}

async fn send_message(app: &axum::Router, token: &str, channel_id: &str, content: &str) {
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
}

#[tokio::test]
async fn a_public_threads_archive_page_renders_with_no_auth() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Public Place", Some("public")).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let thread = create_thread(&app, &alice_token, channel_id, "How do I configure X?").await;
    let thread_id = thread["id"].as_str().expect("thread id present");
    send_message(&app, &alice_token, thread_id, "does anyone know?").await;

    let response = app
        .oneshot(plain_request(Method::GET, &format!("/archive/t/{thread_id}-how-do-i-configure-x")))
        .await
        .expect("archive page request succeeds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(body.contains("How do I configure X?"));
    assert!(body.contains("does anyone know?"));
    assert!(body.contains("Test User"));
}

#[tokio::test]
async fn a_private_threads_archive_page_returns_404_with_no_auth() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Private Place", None).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let thread = create_thread(&app, &alice_token, channel_id, "secret topic").await;
    let thread_id = thread["id"].as_str().expect("thread id present");

    let response = app
        .oneshot(plain_request(Method::GET, &format!("/archive/t/{thread_id}-secret-topic")))
        .await
        .expect("archive page request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_malformed_thread_ref_returns_404_not_a_500() {
    let (app, _mail, _container) = test_app().await;

    let response = app
        .oneshot(plain_request(Method::GET, "/archive/t/not-a-real-uuid"))
        .await
        .expect("archive page request succeeds");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_sitemap_lists_a_public_thread_and_robots_txt_points_at_it() {
    let (app, mail, _container) = test_app().await;
    let (_alice_id, alice_token) = register_and_login(&app, &mail, "alice@example.com", "alice").await;
    let server = create_server(&app, &alice_token, "Public Place", Some("public")).await;
    let server_id = server["id"].as_str().expect("server id present");
    let channel = create_channel(&app, &alice_token, server_id, "general").await;
    let channel_id = channel["id"].as_str().expect("channel id present");
    let thread = create_thread(&app, &alice_token, channel_id, "a public topic").await;
    let thread_id = thread["id"].as_str().expect("thread id present");

    let sitemap_response = app
        .clone()
        .oneshot(plain_request(Method::GET, "/archive/sitemap.xml"))
        .await
        .expect("sitemap request succeeds");
    assert_eq!(sitemap_response.status(), StatusCode::OK);
    let sitemap_body = body_text(sitemap_response).await;
    assert!(sitemap_body.contains(thread_id));

    let robots_response = app
        .oneshot(plain_request(Method::GET, "/robots.txt"))
        .await
        .expect("robots.txt request succeeds");
    assert_eq!(robots_response.status(), StatusCode::OK);
    let robots_body = body_text(robots_response).await;
    assert!(robots_body.contains("/archive/sitemap.xml"));
    assert!(robots_body.contains("Allow: /archive/"));
}
