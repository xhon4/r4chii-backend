//! Object storage access.
//!
//! This crate knows about buckets, keys and bytes. It does not know what an
//! avatar is, what a size limit should be, or who is allowed to read
//! anything — that is `domain`'s job. Keeping it that dumb is what makes the
//! storage backend swappable: Garage today, an S3 bucket somewhere else
//! tomorrow, without a line changing above this boundary.

use std::{fmt, time::Duration};

use reqwest::{header::CONTENT_TYPE, redirect::Policy, Client, Url};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use thiserror::Error;

const REQUEST_EXPIRY: Duration = Duration::from_secs(15 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_PRESIGN_DURATION: Duration = Duration::from_secs(1);
const MAX_PRESIGN_DURATION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("missing required environment variable: {0}")]
    MissingConfig(&'static str),
    #[error("invalid storage endpoint configuration")]
    InvalidEndpoint,
    #[error("invalid storage object key")]
    InvalidKey,
    #[error("storage HTTP client configuration is unavailable")]
    HttpClientUnavailable,
    #[error("failed to store object: {0}")]
    Put(String),
    #[error("failed to delete object: {0}")]
    Delete(String),
    #[error("failed to sign a url: {0}")]
    Presign(String),
    #[error("invalid presigning duration: {0}")]
    InvalidDuration(String),
}

#[derive(Clone)]
pub struct StorageConfig {
    /// How the *backend* reaches storage — over the compose network in dev.
    pub endpoint: String,
    /// How a *browser* reaches storage. Not interchangeable with `endpoint`:
    /// SigV4 signs the Host header, so a URL signed against the internal
    /// name is rejected the moment it is fetched from the public one, with a
    /// 403 that reads exactly like a bad access key.
    pub public_endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageConfig")
            .field("endpoint", &"<redacted>")
            .field("public_endpoint", &"<redacted>")
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

impl StorageConfig {
    pub fn from_env() -> Result<Self, StorageError> {
        fn required(key: &'static str) -> Result<String, StorageError> {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or(StorageError::MissingConfig(key))
        }

        Ok(Self {
            endpoint: required("S3_ENDPOINT")?,
            public_endpoint: required("S3_PUBLIC_ENDPOINT")?,
            region: std::env::var("S3_REGION").unwrap_or_else(|_| "garage".to_string()),
            bucket: required("S3_BUCKET")?,
            access_key_id: required("S3_ACCESS_KEY_ID")?,
            secret_access_key: required("S3_SECRET_ACCESS_KEY")?,
        })
    }
}

/// Reads and writes objects.
///
/// Holds two buckets on purpose. `write` talks to the internal endpoint and
/// actually moves bytes; `sign` never makes a request at all — it exists only
/// so presigned URLs carry the public host in their signature. One bucket
/// cannot do both, because the endpoint is baked into what gets signed.
#[derive(Clone)]
pub struct StorageService {
    write: Option<Bucket>,
    sign: Option<Bucket>,
    credentials: Credentials,
    http: Option<Client>,
}

impl fmt::Debug for StorageService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageService")
            .field("write_endpoint_configured", &self.write.is_some())
            .field("sign_endpoint_configured", &self.sign.is_some())
            .finish_non_exhaustive()
    }
}

fn bucket_for(config: &StorageConfig, endpoint: &str) -> Option<Bucket> {
    let endpoint = endpoint.parse::<Url>().ok()?;
    Bucket::new(
        endpoint,
        UrlStyle::Path,
        config.bucket.clone(),
        config.region.clone(),
    )
    .ok()
}

fn validate_presign_duration(expires_in: Duration) -> Result<(), StorageError> {
    if expires_in < MIN_PRESIGN_DURATION || expires_in > MAX_PRESIGN_DURATION {
        return Err(StorageError::InvalidDuration(
            "duration must be between 1 second and 7 days".to_string(),
        ));
    }
    Ok(())
}

impl StorageService {
    pub fn new(config: &StorageConfig) -> Self {
        Self {
            write: bucket_for(config, &config.endpoint),
            sign: bucket_for(config, &config.public_endpoint),
            credentials: Credentials::new(
                config.access_key_id.clone(),
                config.secret_access_key.clone(),
            ),
            http: Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .redirect(Policy::none())
                .build()
                .ok(),
        }
    }

    pub fn from_env() -> Result<Self, StorageError> {
        Ok(Self::new(&StorageConfig::from_env()?))
    }

    fn write_bucket(&self) -> Result<&Bucket, StorageError> {
        self.write.as_ref().ok_or(StorageError::InvalidEndpoint)
    }

    fn http_client(&self) -> Result<&Client, StorageError> {
        self.http
            .as_ref()
            .ok_or(StorageError::HttpClientUnavailable)
    }

    fn sign_bucket(&self) -> Result<&Bucket, StorageError> {
        self.sign.as_ref().ok_or_else(|| {
            StorageError::Presign("invalid storage endpoint configuration".to_string())
        })
    }

    fn signed_put_url(&self, key: &str, content_type: &str) -> Result<Url, StorageError> {
        let bucket = self.write_bucket()?;
        bucket
            .object_url(key)
            .map_err(|_| StorageError::InvalidKey)?;

        let mut action = bucket.put_object(Some(&self.credentials), key);
        action.headers_mut().insert("content-type", content_type);
        Ok(action.sign(REQUEST_EXPIRY))
    }

    fn signed_delete_url(&self, key: &str) -> Result<Url, StorageError> {
        let bucket = self.write_bucket()?;
        bucket
            .object_url(key)
            .map_err(|_| StorageError::InvalidKey)?;
        Ok(bucket
            .delete_object(Some(&self.credentials), key)
            .sign(REQUEST_EXPIRY))
    }

    /// Stores `bytes` at `key`, replacing whatever was there.
    ///
    /// `content_type` is what future readers will be served. Callers must
    /// pass what they actually produced, never a client-supplied header —
    /// by the time bytes reach here they should already have been decoded
    /// and re-encoded, so the caller knows the format for a fact.
    pub async fn put_object(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<(), StorageError> {
        let response = self
            .http_client()?
            .put(self.signed_put_url(key, content_type)?)
            .header(CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await
            .map_err(|_| StorageError::Put("request failed".to_string()))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(StorageError::Put(format!(
                "request returned HTTP {}",
                response.status()
            )))
        }
    }

    pub async fn delete_object(&self, key: &str) -> Result<(), StorageError> {
        let response = self
            .http_client()?
            .delete(self.signed_delete_url(key)?)
            .send()
            .await
            .map_err(|_| StorageError::Delete("request failed".to_string()))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(StorageError::Delete(format!(
                "request returned HTTP {}",
                response.status()
            )))
        }
    }

    /// A time-limited URL a browser can fetch directly.
    ///
    /// The bucket is private, so this is the only way to read an object —
    /// an unsigned request gets a 403. Signing is pure computation: no
    /// request leaves the process, and the key does not have to exist yet.
    pub async fn presigned_get(
        &self,
        key: &str,
        expires_in: Duration,
    ) -> Result<String, StorageError> {
        validate_presign_duration(expires_in)?;
        let bucket = self.sign_bucket()?;
        bucket
            .object_url(key)
            .map_err(|_| StorageError::InvalidKey)?;

        Ok(bucket
            .get_object(Some(&self.credentials), key)
            .sign(expires_in)
            .to_string())
    }
}
