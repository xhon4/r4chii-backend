//! Servers, channels, memberships, messages, dms, friendships, and blocks —
//! services + authorization logic. Depends on: core, db,
//! auth (see crates/domain/Cargo.toml).

mod error;
mod media;
mod invite;
pub mod channel_permissions;
pub mod permissions;
mod profile_visibility;
mod service;
mod types;
mod validation;

pub use error::DomainError;
pub use media::{process_image, ImagePurpose, MediaError, ProcessedImage};
pub use profile_visibility::{
    decide_profile_visibility, ProfileFieldExposure, ProfileIdentityExposure, ProfileMediaExposure,
    ProfilePresenceExposure, ProfileRelationshipExposure, ProfileViewerRelationship,
    ProfileVisibility, ProfileVisibilityDecision, ProfileVisibilityInput,
};
pub use service::{DomainService, ReadAccess, DEFAULT_MESSAGE_LIMIT, MAX_MESSAGE_LIMIT};
pub use types::{
    BanSummary, BlockSummary, ChannelSummary, CreateChannelInput, CreateGroupDmInput,
    CreateRoleInput, CreateServerInput, CreateThreadInput, EditMessageInput, ExportJobSummary,
    FriendshipSummary, MessagePagination, MessageSummary, PublicMessageSummary, RoleSummary,
    SearchInput, SendMessageInput, ServerMemberSummary, ServerSummary, SitemapThread,
    TimeoutInput, UpdateRoleInput,
};
