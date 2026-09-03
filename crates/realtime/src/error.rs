use thiserror::Error;

/// Realtime errors. No HTTP/WebSocket-close-code knowledge lives here — the
/// `api` crate maps this the same way it maps `domain::DomainError`.
#[derive(Debug, Error)]
pub enum RealtimeError {
    /// `Hub::publish_*` resolves recipients via
    /// `domain::DomainService::authorized_account_ids`, which can fail the
    /// same ways any other domain query can.
    #[error("domain error resolving publish recipients: {0}")]
    Domain(#[from] domain::DomainError),
}
