//! Object storage access.
//!
//! This crate knows about buckets, keys and bytes. It does not know what an
//! avatar is, what a size limit should be, or who is allowed to read
//! anything — that is `domain`'s job. Keeping it that dumb is what makes the
//! storage backend swappable: Garage today, an S3 bucket somewhere else
//! tomorrow, without a line changing above this boundary.

use std::time::Duration;

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("missing required environment variable: {0}")]
    MissingConfig(&'static str),
    #[error("failed to store object: {0}")]
    Put(String),
    #[error("failed to delete object: {0}")]
    Delete(String),
    #[error("failed to sign a url: {0}")]
    Presign(String),
    #[error("invalid presigning duration: {0}")]
    InvalidDuration(String),
}

#[derive(Debug, Clone)]
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
            // Garage has no real regions, but SigV4 signs whichever string is
            // used, so both sides must agree on it.
            region: std::env::var("S3_REGION").unwrap_or_else(|_| "garage".to_string()),
            bucket: required("S3_BUCKET")?,
            access_key_id: required("S3_ACCESS_KEY_ID")?,
            secret_access_key: required("S3_SECRET_ACCESS_KEY")?,
        })
    }
}

/// Reads and writes objects.
///
/// Holds two clients on purpose. `write` talks to the internal endpoint and
/// actually moves bytes; `sign` never makes a request at all — it exists only
/// so presigned URLs carry the public host in their signature. One client
/// cannot do both, because the endpoint is baked into what gets signed.
#[derive(Debug, Clone)]
pub struct StorageService {
    write: Client,
    sign: Client,
    bucket: String,
}

fn client_for(config: &StorageConfig, endpoint: &str) -> Client {
    let credentials = Credentials::new(
        config.access_key_id.clone(),
        config.secret_access_key.clone(),
        None,
        None,
        "r4chii-static",
    );

    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(config.region.clone()))
        .endpoint_url(endpoint)
        .credentials_provider(credentials)
        // Garage serves path-style out of the box. Virtual-host style would
        // need a wildcard DNS entry per bucket, which nothing here has.
        .force_path_style(true)
        .build();

    Client::from_conf(s3_config)
}

impl StorageService {
    pub fn new(config: &StorageConfig) -> Self {
        Self {
            write: client_for(config, &config.endpoint),
            sign: client_for(config, &config.public_endpoint),
            bucket: config.bucket.clone(),
        }
    }

    pub fn from_env() -> Result<Self, StorageError> {
        Ok(Self::new(&StorageConfig::from_env()?))
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
        self.write
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|e| StorageError::Put(format!("{e:?}")))?;
        Ok(())
    }

    pub async fn delete_object(&self, key: &str) -> Result<(), StorageError> {
        self.write
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::Delete(format!("{e:?}")))?;
        Ok(())
    }

    /// A time-limited URL a browser can fetch directly.
    ///
    /// The bucket is private, so this is the only way to read an object —
    /// an unsigned request gets a 403. Signing is pure computation: no
    /// request leaves the process, and the key does not have to exist yet.
    /// It is `async` regardless because the SDK's credential provider is,
    /// and every caller here is already in async context.
    pub async fn presigned_get(
        &self,
        key: &str,
        expires_in: Duration,
    ) -> Result<String, StorageError> {
        let presigning = PresigningConfig::expires_in(expires_in)
            .map_err(|e| StorageError::InvalidDuration(e.to_string()))?;

        let request = self
            .sign
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presigning)
            .await
            .map_err(|e| StorageError::Presign(format!("{e:?}")))?;

        Ok(request.uri().to_string())
    }
}
