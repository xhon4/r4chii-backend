//! HTTP error envelope:
//! `{ "error": { "code", "message", "details" } }`. Intentionally generic —
//! not `auth`-specific — so future slices' domain errors map through the
//! same `ApiError` type instead of each crate inventing its own response
//! shape.

use axum::{
    extract::rejection::{JsonRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "details": self.details,
            }
        });
        (self.status, Json(body)).into_response()
    }
}

impl From<auth::AuthError> for ApiError {
    fn from(err: auth::AuthError) -> Self {
        match err {
            auth::AuthError::EmailTaken => {
                ApiError::new(StatusCode::CONFLICT, "email_taken", err.to_string())
            }
            auth::AuthError::UsernameTaken => {
                ApiError::new(StatusCode::CONFLICT, "username_taken", err.to_string())
            }
            auth::AuthError::InvalidCredentials => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                err.to_string(),
            ),
            auth::AuthError::SessionLimitReached => ApiError::new(
                StatusCode::CONFLICT,
                "session_limit_reached",
                err.to_string(),
            ),
            // Deliberately the same shape whether the session id belongs to
            // someone else or doesn't exist at all.
            auth::AuthError::SessionNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "session_not_found", err.to_string())
            }
            // Deliberately the same shape whether the account id belongs to
            // an account that never existed or one the caller may not see —
            // 404 also for resources the caller may not see, to avoid
            // revealing existence. M0 has no
            // profile-visibility rules yet, so today this only ever fires
            // for a genuinely nonexistent id.
            auth::AuthError::AccountNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "account_not_found", err.to_string())
            }
            auth::AuthError::Unauthenticated => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                err.to_string(),
            ),
            // One shape for wrong, expired, exhausted, and "no such pending
            // registration". Splitting them would tell a guesser which
            // addresses are mid-signup and whether an attempt was close.
            auth::AuthError::InvalidVerificationCode => ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_verification_code",
                err.to_string(),
            ),
            // Distinct from `username_taken` on purpose: this caller proved
            // their address and lost a race, so the client can say so rather
            // than implying they chose a name that was already gone.
            auth::AuthError::UsernameTakenDuringVerification => ApiError::new(
                StatusCode::CONFLICT,
                "username_taken_during_verification",
                err.to_string(),
            ),
            auth::AuthError::Validation(message) => ApiError::new(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                "the request failed validation",
            )
            .with_details(json!({ "message": message })),
            auth::AuthError::MailDelivery(mail_err) => {
                // Log the cause, return a generic 502: the relay's complaint
                // is an operator's problem and can carry infrastructure
                // details the caller has no business seeing.
                //
                // Debug, not Display: `MailError::Delivery` renders as the
                // bare string "delivery failed" and keeps the relay's actual
                // refusal — an SMTP status like 535 — in `#[source]`, which
                // Display never walks. Formatting it with `%` logged a line
                // that named the failure and omitted every fact needed to act
                // on it.
                tracing::error!(error = ?mail_err, "verification email delivery failed");
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "mail_delivery_failed",
                    "could not send the verification email",
                )
            }
            auth::AuthError::Database(db_err) => {
                // Never leak the DB error string in the response body — log
                // it server-side instead.
                tracing::error!(error = %db_err, "database error handling request");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "an internal error occurred",
                )
            }
        }
    }
}

impl From<domain::DomainError> for ApiError {
    fn from(err: domain::DomainError) -> Self {
        match err {
            // Deliberately the same shape whether the server doesn't exist
            // or the caller simply isn't a member — 404 either way, never
            // distinguished.
            domain::DomainError::ServerNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "server_not_found", err.to_string())
            }
            // Also a 404, and deliberately the same code shape as
            // ServerNotFound is fine here too — a bad invite guess must not
            // reveal whether a code was almost right.
            domain::DomainError::InvalidInvite => {
                ApiError::new(StatusCode::NOT_FOUND, "invalid_invite", err.to_string())
            }
            domain::DomainError::ChannelNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "channel_not_found", err.to_string())
            }
            domain::DomainError::MessageNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "message_not_found", err.to_string())
            }
            domain::DomainError::NotMessageAuthor => ApiError::new(
                StatusCode::FORBIDDEN,
                "not_message_author",
                err.to_string(),
            ),
            // Caller-supplied dm/group-dm participant id that doesn't exist —
            // safe to report plainly, unlike the non-leaking 404s above —
            // that rule is about the caller's OWN access to a resource, not
            // input they typed themselves.
            domain::DomainError::AccountNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "account_not_found", err.to_string())
            }
            domain::DomainError::AlreadyMember => {
                ApiError::new(StatusCode::CONFLICT, "already_a_member", err.to_string())
            }
            domain::DomainError::FriendRequestNotFound => ApiError::new(
                StatusCode::NOT_FOUND,
                "friend_request_not_found",
                err.to_string(),
            ),
            domain::DomainError::BlockNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "block_not_found", err.to_string())
            }
            // Deliberately the same shape whether the CALLER placed the
            // block or the target did — a block is directional and this
            // must never let one side learn the other's block state (see
            // `domain::DomainError::Blocked`'s own doc comment).
            domain::DomainError::Blocked => {
                ApiError::new(StatusCode::FORBIDDEN, "blocked", err.to_string())
            }
            domain::DomainError::Validation(message) => ApiError::new(
                StatusCode::BAD_REQUEST,
                "validation_failed",
                "the request failed validation",
            )
            .with_details(json!({ "message": message })),
            // M2.
            domain::DomainError::RoleNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "role_not_found", err.to_string())
            }
            domain::DomainError::MissingPermission => {
                ApiError::new(StatusCode::FORBIDDEN, "missing_permission", err.to_string())
            }
            domain::DomainError::InsufficientHierarchy => ApiError::new(
                StatusCode::FORBIDDEN,
                "insufficient_hierarchy",
                err.to_string(),
            ),
            domain::DomainError::CannotModifyDefaultRole => ApiError::new(
                StatusCode::BAD_REQUEST,
                "cannot_modify_default_role",
                err.to_string(),
            ),
            domain::DomainError::CannotActOnOwner => {
                ApiError::new(StatusCode::FORBIDDEN, "cannot_act_on_owner", err.to_string())
            }
            domain::DomainError::CannotActOnSelf => {
                ApiError::new(StatusCode::BAD_REQUEST, "cannot_act_on_self", err.to_string())
            }
            domain::DomainError::RoleLimitReached => {
                ApiError::new(StatusCode::CONFLICT, "role_limit_reached", err.to_string())
            }
            domain::DomainError::AlreadyBanned => {
                ApiError::new(StatusCode::CONFLICT, "already_banned", err.to_string())
            }
            domain::DomainError::Banned => {
                ApiError::new(StatusCode::FORBIDDEN, "banned_from_server", err.to_string())
            }
            domain::DomainError::OwnerCannotLeave => {
                ApiError::new(StatusCode::BAD_REQUEST, "owner_cannot_leave", err.to_string())
            }
            domain::DomainError::ExportJobNotFound => {
                ApiError::new(StatusCode::NOT_FOUND, "export_job_not_found", err.to_string())
            }
            // Only ever produced inside the worker
            // (`process_next_export_job`), which writes it to
            // `export_job.error` rather than returning an HTTP response —
            // this arm exists only so the match stays exhaustive.
            domain::DomainError::ExportFailed(_) => {
                tracing::error!(error = %err, "export job failed");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "an unexpected error occurred".to_string(),
                )
            }
            domain::DomainError::MemberTimedOut => {
                ApiError::new(StatusCode::FORBIDDEN, "member_timed_out", err.to_string())
            }
            domain::DomainError::MentionNotAllowed => {
                ApiError::new(StatusCode::FORBIDDEN, "mention_not_allowed", err.to_string())
            }
            domain::DomainError::Database(db_err) => {
                // Never leak the DB error string in the response body — log
                // it server-side instead.
                tracing::error!(error = %db_err, "database error handling request");
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "an internal error occurred",
                )
            }
        }
    }
}

/// Malformed/missing JSON body -> 400, in the same envelope shape. This is
/// request *parsing*, not business validation, so it belongs in the api
/// crate rather than auth's `Validation` variant.
impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_request_body", rejection.body_text())
    }
}

/// Malformed query string (e.g. `before` that isn't a valid UUID) -> 400,
/// same envelope shape and same rationale as `JsonRejection` above.
impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_query_string", rejection.body_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_taken_maps_to_409_with_the_right_code() {
        let api_err: ApiError = auth::AuthError::EmailTaken.into();
        assert_eq!(api_err.status(), StatusCode::CONFLICT);
        assert_eq!(api_err.code(), "email_taken");
    }

    #[test]
    fn username_taken_maps_to_409_with_the_right_code() {
        let api_err: ApiError = auth::AuthError::UsernameTaken.into();
        assert_eq!(api_err.status(), StatusCode::CONFLICT);
        assert_eq!(api_err.code(), "username_taken");
    }

    #[test]
    fn invalid_credentials_maps_to_401() {
        let api_err: ApiError = auth::AuthError::InvalidCredentials.into();
        assert_eq!(api_err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(api_err.code(), "invalid_credentials");
    }

    #[test]
    fn session_limit_reached_maps_to_409() {
        let api_err: ApiError = auth::AuthError::SessionLimitReached.into();
        assert_eq!(api_err.status(), StatusCode::CONFLICT);
        assert_eq!(api_err.code(), "session_limit_reached");
    }

    #[test]
    fn session_not_found_maps_to_404() {
        let api_err: ApiError = auth::AuthError::SessionNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "session_not_found");
    }

    #[test]
    fn account_not_found_maps_to_404() {
        let api_err: ApiError = auth::AuthError::AccountNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "account_not_found");
    }

    #[test]
    fn unauthenticated_maps_to_401() {
        let api_err: ApiError = auth::AuthError::Unauthenticated.into();
        assert_eq!(api_err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(api_err.code(), "unauthenticated");
    }

    #[test]
    fn validation_maps_to_400_and_carries_the_message_in_details() {
        let api_err: ApiError = auth::AuthError::Validation("bad email".to_string()).into();
        assert_eq!(api_err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api_err.code(), "validation_failed");
        assert_eq!(api_err.details, Some(json!({ "message": "bad email" })));
    }

    #[test]
    fn database_error_maps_to_500_without_leaking_the_db_error_string() {
        let db_err = sqlx::Error::RowNotFound;
        let message = db_err.to_string();
        let api_err: ApiError = auth::AuthError::Database(db_err).into();

        assert_eq!(api_err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api_err.code(), "internal_error");
        assert_ne!(api_err.message, message, "must not leak the raw DB error text");
    }

    #[test]
    fn server_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::ServerNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "server_not_found");
    }

    #[test]
    fn invalid_invite_maps_to_404() {
        let api_err: ApiError = domain::DomainError::InvalidInvite.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "invalid_invite");
    }

    #[test]
    fn channel_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::ChannelNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "channel_not_found");
    }

    #[test]
    fn message_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::MessageNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "message_not_found");
    }

    #[test]
    fn not_message_author_maps_to_403() {
        let api_err: ApiError = domain::DomainError::NotMessageAuthor.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "not_message_author");
    }

    #[test]
    fn domain_account_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::AccountNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "account_not_found");
    }

    #[test]
    fn already_member_maps_to_409() {
        let api_err: ApiError = domain::DomainError::AlreadyMember.into();
        assert_eq!(api_err.status(), StatusCode::CONFLICT);
        assert_eq!(api_err.code(), "already_a_member");
    }

    #[test]
    fn friend_request_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::FriendRequestNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "friend_request_not_found");
    }

    #[test]
    fn block_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::BlockNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "block_not_found");
    }

    #[test]
    fn blocked_maps_to_403() {
        let api_err: ApiError = domain::DomainError::Blocked.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "blocked");
    }

    #[test]
    fn domain_validation_maps_to_400_and_carries_the_message_in_details() {
        let api_err: ApiError = domain::DomainError::Validation("bad name".to_string()).into();
        assert_eq!(api_err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api_err.code(), "validation_failed");
        assert_eq!(api_err.details, Some(json!({ "message": "bad name" })));
    }

    #[test]
    fn role_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::RoleNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "role_not_found");
    }

    #[test]
    fn missing_permission_maps_to_403() {
        let api_err: ApiError = domain::DomainError::MissingPermission.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "missing_permission");
    }

    #[test]
    fn insufficient_hierarchy_maps_to_403() {
        let api_err: ApiError = domain::DomainError::InsufficientHierarchy.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "insufficient_hierarchy");
    }

    #[test]
    fn cannot_modify_default_role_maps_to_400() {
        let api_err: ApiError = domain::DomainError::CannotModifyDefaultRole.into();
        assert_eq!(api_err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api_err.code(), "cannot_modify_default_role");
    }

    #[test]
    fn cannot_act_on_owner_maps_to_403() {
        let api_err: ApiError = domain::DomainError::CannotActOnOwner.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "cannot_act_on_owner");
    }

    #[test]
    fn cannot_act_on_self_maps_to_400() {
        let api_err: ApiError = domain::DomainError::CannotActOnSelf.into();
        assert_eq!(api_err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api_err.code(), "cannot_act_on_self");
    }

    #[test]
    fn role_limit_reached_maps_to_409() {
        let api_err: ApiError = domain::DomainError::RoleLimitReached.into();
        assert_eq!(api_err.status(), StatusCode::CONFLICT);
        assert_eq!(api_err.code(), "role_limit_reached");
    }

    #[test]
    fn already_banned_maps_to_409() {
        let api_err: ApiError = domain::DomainError::AlreadyBanned.into();
        assert_eq!(api_err.status(), StatusCode::CONFLICT);
        assert_eq!(api_err.code(), "already_banned");
    }

    #[test]
    fn banned_maps_to_403() {
        let api_err: ApiError = domain::DomainError::Banned.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "banned_from_server");
    }

    #[test]
    fn owner_cannot_leave_maps_to_400() {
        let api_err: ApiError = domain::DomainError::OwnerCannotLeave.into();
        assert_eq!(api_err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api_err.code(), "owner_cannot_leave");
    }

    #[test]
    fn export_job_not_found_maps_to_404() {
        let api_err: ApiError = domain::DomainError::ExportJobNotFound.into();
        assert_eq!(api_err.status(), StatusCode::NOT_FOUND);
        assert_eq!(api_err.code(), "export_job_not_found");
    }

    #[test]
    fn export_failed_maps_to_500_without_leaking_the_underlying_message() {
        let api_err: ApiError = domain::DomainError::ExportFailed("s3 timeout".to_string()).into();
        assert_eq!(api_err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api_err.code(), "internal_error");
    }

    #[test]
    fn member_timed_out_maps_to_403() {
        let api_err: ApiError = domain::DomainError::MemberTimedOut.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "member_timed_out");
    }

    #[test]
    fn mention_not_allowed_maps_to_403() {
        let api_err: ApiError = domain::DomainError::MentionNotAllowed.into();
        assert_eq!(api_err.status(), StatusCode::FORBIDDEN);
        assert_eq!(api_err.code(), "mention_not_allowed");
    }

    #[test]
    fn domain_database_error_maps_to_500_without_leaking_the_db_error_string() {
        let db_err = sqlx::Error::RowNotFound;
        let message = db_err.to_string();
        let api_err: ApiError = domain::DomainError::Database(db_err).into();

        assert_eq!(api_err.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api_err.code(), "internal_error");
        assert_ne!(api_err.message, message, "must not leak the raw DB error text");
    }
}
