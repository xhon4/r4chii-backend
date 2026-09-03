//! WebSocket hub + event protocol. Depends on: core, domain — never `db`
//! or `auth` directly; the `api` crate resolves auth and hands this crate
//! only what it needs (an `account_id` to register, a `domain::DomainService`
//! to resolve publish recipients through).

mod error;
mod event;
mod hub;

pub use error::RealtimeError;
pub use event::{
    parse_client_frame, ClientFrame, MemberLeaveReason, MessagePayload, PresenceStatus,
    RolePayload, ServerEvent,
};
pub use hub::{ConnectionHandle, Hub};
