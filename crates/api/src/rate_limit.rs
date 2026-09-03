//! Maps tower-governor's `GovernorError` onto the same
//! `{ "error": { "code", "message", "details" } }` envelope as every other
//! API error — every error, no exceptions.
//! Without this, a rate-limited request would get tower-governor's own
//! plain-text response instead of our JSON shape.

use axum::{
    body::Body,
    http::{header, Response, StatusCode},
};
use serde_json::json;
use tower_governor::GovernorError;

pub(crate) fn error_response(error: GovernorError) -> Response<Body> {
    let (status, code, message, details, extra_headers) = match error {
        GovernorError::TooManyRequests { wait_time, headers } => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many requests, please slow down".to_string(),
            Some(json!({ "retry_after_seconds": wait_time })),
            headers,
        ),
        GovernorError::UnableToExtractKey => {
            // Only reachable if the server forgot to wire
            // into_make_service_with_connect_info — a server misconfiguration,
            // not something the caller can fix, hence 500.
            tracing::error!("rate limiter could not extract a peer key from the request");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "an internal error occurred".to_string(),
                None,
                None,
            )
        }
        GovernorError::Other { code, msg, headers } => {
            tracing::error!(status = %code, message = ?msg, "rate limiter error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "an internal error occurred".to_string(),
                None,
                headers,
            )
        }
    };

    let body = json!({
        "error": {
            "code": code,
            "message": message,
            "details": details,
        }
    });

    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    if let Some(extra) = extra_headers {
        response.headers_mut().extend(extra);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn too_many_requests_maps_to_429_with_the_shared_envelope() {
        let response = error_response(GovernorError::TooManyRequests {
            wait_time: 3,
            headers: None,
        });

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        let bytes = response.into_body().collect().await.expect("body collects").to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");

        assert_eq!(body["error"]["code"], "rate_limited");
        assert_eq!(body["error"]["details"]["retry_after_seconds"], 3);
    }

    #[tokio::test]
    async fn unable_to_extract_key_maps_to_500_without_leaking_internals() {
        let response = error_response(GovernorError::UnableToExtractKey);

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let bytes = response.into_body().collect().await.expect("body collects").to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");

        assert_eq!(body["error"]["code"], "internal_error");
    }
}
