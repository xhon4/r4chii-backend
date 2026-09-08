use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn healthz_reports_ok_when_database_is_reachable() {
    let test_db = test_support::test_db().await;
    let pool = test_db.pool();

    let app = server::build_router(
        pool,
        std::sync::Arc::new(mailer::CaptureMailer::new()),
        None,
        None,
    );

    let response = app
        .oneshot(
            Request::get("/healthz")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::OK);

    let body = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();

    assert_eq!(body.as_ref(), br#"{"status":"ok"}"#);
}
