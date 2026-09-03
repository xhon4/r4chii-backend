//! The `AuthContext` extractor: resolves either the `Authorization: Bearer`
//! header or the `r4chii_session` cookie into the same `app_core::AuthContext`
//! — one session model, two transports.

use axum::{
    extract::{FromRef, FromRequestParts},
    http::{request::Parts, StatusCode},
};
use axum_extra::{
    extract::cookie::CookieJar,
    headers::authorization::{Authorization, Bearer},
    TypedHeader,
};

use crate::{error::ApiError, AppState};

/// Name of the session cookie set by the login handler.
pub const SESSION_COOKIE_NAME: &str = "r4chii_session";

/// Thin wrapper around `app_core::AuthContext`. A local newtype is required
/// here — orphan rules forbid implementing the foreign `FromRequestParts`
/// trait directly for the foreign `app_core::AuthContext` type.
#[derive(Debug, Clone, Copy)]
pub struct AuthenticatedUser(pub app_core::AuthContext);

impl<S> FromRequestParts<S> for AuthenticatedUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        let Some(token) = resolve_token(parts, state).await else {
            return Err(unauthenticated());
        };

        let context = app_state.auth.verify_session(&token).await?;
        Ok(AuthenticatedUser(context))
    }
}

/// Pulls a session token out of the request: `Authorization: Bearer` first,
/// then the session cookie — one session model, two transports. Shared by
/// [`AuthenticatedUser`] (which hard-fails 401 when this comes back `None`)
/// and the gateway upgrade handler, which must NOT hard-fail — a
/// Bearer-only client can't set custom headers on a browser WebSocket
/// handshake, so the fallback is to upgrade anyway and wait for an
/// `identify` frame when no token is found here.
pub(crate) async fn resolve_token<S>(parts: &mut Parts, state: &S) -> Option<String>
where
    S: Send + Sync,
{
    let bearer_token = TypedHeader::<Authorization<Bearer>>::from_request_parts(parts, state)
        .await
        .ok()
        .map(|TypedHeader(auth)| auth.token().to_string());

    match bearer_token {
        Some(token) => Some(token),
        None => CookieJar::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|jar| jar.get(SESSION_COOKIE_NAME).map(|c| c.value().to_string())),
    }
}

/// Extracts a token via [`resolve_token`] without hard-failing when absent —
/// used only by the gateway upgrade handler's handshake fallback.
pub(crate) struct ResolvedToken(pub Option<String>);

impl<S> FromRequestParts<S> for ResolvedToken
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(ResolvedToken(resolve_token(parts, state).await))
    }
}

fn unauthenticated() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "authentication required",
    )
}
