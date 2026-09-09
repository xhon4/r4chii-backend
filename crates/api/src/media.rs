//! Avatar and banner upload, and the route that serves what was uploaded.
//!
//! The bytes travel through the backend rather than going straight to storage
//! from the browser: a presigned upload cannot check that what arrives is an
//! image, or that it carries no metadata.
//!
//! The stored value is a path on this API, not a storage URL. The bucket is
//! private, so a storage URL has to be signed and would expire while the
//! column holding it does not. The path is also relative, so it resolves
//! against whichever host served the client and the same row works on any
//! instance.

use axum::{
    extract::{Multipart, Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::time::Duration;

use crate::{dto::AccountResponse, error::ApiError, extract::AuthenticatedUser, AppState};

/// How long a signed storage URL stays valid.
///
/// Kept for any path that still hands a presigned URL directly to a client
/// (e.g. export downloads). Media reads no longer use it — they are proxied
/// same-origin so the signature never leaves the backend.
#[allow(dead_code)]
const MEDIA_URL_TTL: Duration = Duration::from_secs(60 * 60);

/// How long a browser may reuse a media response.
const MEDIA_REDIRECT_CACHE_SECONDS: u64 = 5 * 60;

/// Reads the first file field out of a multipart body.
async fn read_upload(mut multipart: Multipart) -> Result<Vec<u8>, ApiError> {
    let field = multipart
        .next_field()
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_multipart",
                err.body_text(),
            )
        })?
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_multipart",
                "the request carries no file",
            )
        })?;

    field
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|err| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_multipart",
                err.body_text(),
            )
        })
}

fn media_error(err: domain::MediaError) -> ApiError {
    let code = match err {
        domain::MediaError::TooLarge { .. } => "image_too_large",
        domain::MediaError::Empty
        | domain::MediaError::UnknownFormat
        | domain::MediaError::UnsupportedFormat
        | domain::MediaError::Decode(_) => "invalid_image",
        domain::MediaError::Encode(_) => "image_processing_failed",
    };
    let status = match err {
        domain::MediaError::Encode(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError::new(status, code, err.to_string())
}

fn storage_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "storage_unavailable",
        "object storage is not configured on this instance",
    )
}

/// Processes an upload and records where it landed.
///
/// The object key is built here and never read from the request, so a caller
/// has no way to name someone else's key or to overwrite an existing object.
async fn store_upload(
    state: &AppState,
    account_id: uuid::Uuid,
    purpose: domain::ImagePurpose,
    bytes: Vec<u8>,
) -> Result<AccountResponse, ApiError> {
    // The caller's own error is decided before the instance's configuration
    // is consulted, so a malformed upload reads as malformed everywhere.
    let processed = domain::process_image(&bytes, purpose).map_err(media_error)?;

    let storage = state.storage.as_ref().ok_or_else(storage_unavailable)?;

    let folder = match purpose {
        domain::ImagePurpose::Avatar => "avatars",
        domain::ImagePurpose::Banner => "banners",
    };
    let key = format!(
        "{folder}/{account_id}/{}.{}",
        app_core::new_id(),
        processed.extension
    );

    storage
        .put_object(&key, processed.bytes, processed.content_type)
        .await
        .map_err(|err| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "storage_write_failed",
                err.to_string(),
            )
        })?;

    let url = format!("/api/v1/media/{key}");
    let account = match purpose {
        domain::ImagePurpose::Avatar => state.auth.set_avatar_url(account_id, &url).await?,
        domain::ImagePurpose::Banner => state.auth.set_banner_url(account_id, &url).await?,
    };
    let links = state.auth.list_profile_links(account_id).await?;

    Ok(AccountResponse::build(account, links))
}

pub async fn upload_avatar(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    multipart: Multipart,
) -> Result<Json<AccountResponse>, ApiError> {
    let bytes = read_upload(multipart).await?;
    let account = store_upload(
        &state,
        context.account_id,
        domain::ImagePurpose::Avatar,
        bytes,
    )
    .await?;
    Ok(Json(account))
}

pub async fn upload_banner(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    multipart: Multipart,
) -> Result<Json<AccountResponse>, ApiError> {
    let bytes = read_upload(multipart).await?;
    let account = store_upload(
        &state,
        context.account_id,
        domain::ImagePurpose::Banner,
        bytes,
    )
    .await?;
    Ok(Json(account))
}

/// Streams the object through the API (same-origin, no LNA).
///
/// Authenticated like the profiles it serves. Without that, the stored path
/// would hand permanent access to anyone who came across it, which is the
/// property signing the storage URL exists to avoid.
///
/// Previously this issued a 307 redirect to a presigned `S3_PUBLIC_ENDPOINT`
/// URL on the Tailscale tailnet (100.x, considered "private" by Chrome's
/// Local Network Access). Browsers on r4chii.com (public) navigating to
/// ts.net (private) triggered the LNA permission prompt; blocking it left
/// avatars blank. Proxying through the backend keeps the fetch same-origin
/// (`https://r4chii.com/api/v1/media/*`) and no LNA check applies.
pub async fn get_media(
    State(state): State<AppState>,
    _caller: AuthenticatedUser,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    // `..` in a key would resolve outside the prefix the uploader was given.
    if key
        .split('/')
        .any(|segment| segment == ".." || segment.is_empty())
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_media_key",
            "malformed key",
        ));
    }

    let storage = state.storage.as_ref().ok_or_else(storage_unavailable)?;

    let (bytes, content_type) = storage.get_object(&key).await.map_err(|err| match err {
        storage::StorageError::NotFound => ApiError::new(
            StatusCode::NOT_FOUND,
            "media_not_found",
            "media not found",
        ),
        storage::StorageError::InvalidKey => ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_media_key",
            "malformed key",
        ),
        other => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "storage_read_failed",
            other.to_string(),
        ),
    })?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CACHE_CONTROL,
                format!("private, max-age={MEDIA_REDIRECT_CACHE_SECONDS}"),
            ),
        ],
        bytes,
    )
        .into_response())
}
