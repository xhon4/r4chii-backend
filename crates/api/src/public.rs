//! Public read path: server-rendered, no login, no client JS.
//! Merged into the same `axum::Router`/binary as the JSON API and the SPA's
//! static assets (a deliberate one-binary shape), under `/archive/*`
//! so it can never collide with the SPA's own client-side routes (which
//! live at `/`) or the JSON API (`/api/v1/*`). `/robots.txt` is registered
//! at the true site root, per that file's own hard convention — nested
//! under `/archive/` would make it invisible to a crawler.

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;

use crate::AppState;

/// A thread's canonical URL id is its bare `channel.id` (a UUIDv7, always
/// 36 characters as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`) with an
/// optional `-{slug}` suffix. The slug is cosmetic — slugs can
/// drift, the id always resolves — this only ever reads the leading 36
/// characters and ignores whatever follows.
fn parse_thread_id(thread_ref: &str) -> Option<app_core::Uuid> {
    let id_part = thread_ref.get(..36)?;
    app_core::Uuid::parse_str(id_part).ok()
}

fn format_timestamp(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

struct PublicMessageView {
    author_display_name: String,
    created_at_iso: String,
    created_at_display: String,
    content: String,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadTemplate {
    title: String,
    canonical_url: String,
    thread_id: String,
    messages: Vec<PublicMessageView>,
    next_cursor: Option<String>,
}

#[derive(Template)]
#[template(path = "not_found.html")]
struct NotFoundTemplate;

/// Renders `not_found.html` at 404 — the one response shape for "deleted",
/// "de-listed", "never existed", or "not actually a thread" — these must
/// be indistinguishable to an anonymous caller. This project has
/// no thread-delete feature yet (no `deleted_at`/tombstone concept on
/// `channel` at all), so the intended 410-for-deleted / 404-for-de-listed
/// split has nothing to switch on today — deferred until a
/// delete-thread action exists to actually produce that first case.
fn not_found() -> Response {
    let body = NotFoundTemplate.render().unwrap_or_else(|_| "Not found".to_string());
    (StatusCode::NOT_FOUND, Html(body)).into_response()
}

#[derive(Debug, Deserialize)]
struct ThreadPageQuery {
    after: Option<app_core::Uuid>,
}

async fn thread_page(
    State(state): State<AppState>,
    Path(thread_ref): Path<String>,
    Query(query): Query<ThreadPageQuery>,
) -> Response {
    let Some(thread_id) = parse_thread_id(&thread_ref) else {
        return not_found();
    };

    let Ok((thread, messages)) = state.domain.get_public_thread(thread_id, query.after).await else {
        return not_found();
    };

    let title = thread.title.unwrap_or_else(|| "Untitled thread".to_string());
    let slug = thread.slug.unwrap_or_default();
    let next_cursor = messages.last().map(|m| m.id.to_string());

    let template = ThreadTemplate {
        canonical_url: format!("/archive/t/{thread_id}-{slug}"),
        thread_id: thread_id.to_string(),
        title,
        messages: messages
            .into_iter()
            .map(|m| PublicMessageView {
                author_display_name: m.author_display_name,
                created_at_iso: m.created_at.to_rfc3339(),
                created_at_display: format_timestamp(m.created_at),
                content: m.content,
            })
            .collect(),
        next_cursor,
    };

    match template.render() {
        Ok(body) => (StatusCode::OK, Html(body)).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "template render failed").into_response(),
    }
}

/// Self-adapting to whatever host actually served the request (works
/// unmodified on `r4chii.com` or any self-hosted instance, per
/// this project's self-hosting goal) rather than a hardcoded domain —
/// there is no base-URL config to read here (`app_core::Config` has none).
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("https://{host}")
}

async fn sitemap_xml(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let threads = state.domain.list_public_threads().await.unwrap_or_default();
    let base = base_url(&headers);

    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push_str(r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#);
    for thread in threads {
        xml.push_str("<url><loc>");
        xml.push_str(&format!("{base}/archive/t/{}-{}", thread.id, thread.slug));
        xml.push_str("</loc><lastmod>");
        xml.push_str(&thread.created_at.to_rfc3339());
        xml.push_str("</lastmod></url>");
    }
    xml.push_str("</urlset>");

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

async fn robots_txt() -> Response {
    let body = "User-agent: *\n\
                Allow: /archive/\n\
                Disallow: /archive/*?*\n\
                Sitemap: /archive/sitemap.xml\n";

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/archive/t/{thread_ref}", get(thread_page))
        .route("/archive/sitemap.xml", get(sitemap_xml))
        .route("/robots.txt", get(robots_txt))
}
