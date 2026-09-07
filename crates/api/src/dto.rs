//! Request/response DTOs — the JSON shapes at the HTTP boundary. Kept
//! separate from `auth`'s domain types (`AccountSummary`, `SessionSummary`)
//! `api` owns "routing, extractors, request/response DTOs, error mapping";
//! domain types carry no serde knowledge.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub username: String,
    pub password: String,
    pub display_name: String,
}

impl From<RegisterRequest> for auth::RegisterInput {
    fn from(req: RegisterRequest) -> Self {
        Self {
            email: req.email,
            username: req.username,
            password: req.password,
            display_name: req.display_name,
        }
    }
}

/// Body of `POST /registration-codes` — asks for a fresh code on an existing
/// pending registration.
#[derive(Debug, Deserialize)]
pub struct ResendCodeRequest {
    pub email: String,
}

/// Body of `POST /accounts` — the address and the code that proves it. This is
/// what actually creates an account.
#[derive(Debug, Deserialize)]
pub struct VerifyRegistrationRequest {
    pub email: String,
    pub code: String,
}

impl From<VerifyRegistrationRequest> for auth::VerifyRegistrationInput {
    fn from(req: VerifyRegistrationRequest) -> Self {
        Self {
            email: req.email,
            code: req.code,
        }
    }
}

/// The "self" account shape: what a caller sees for their own profile.
/// Includes `email` — never returned for another account's profile, see
/// `ProfileResponse`.
#[derive(Debug, Serialize)]
pub struct AccountResponse {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub bio: Option<String>,
    pub banner_url: Option<String>,
    pub accent_color: Option<String>,
    pub pronouns: Option<String>,
    /// The global "member since". Already on the public shape; carried here
    /// too so a client rendering its own profile does not need a second
    /// request to fill in the same field.
    pub created_at: DateTime<Utc>,
    /// `null` only for accounts predating required email verification. The client uses it to
    /// prompt those accounts to verify from settings; it is never a reason to
    /// block a request. Self shape only — whether someone else has verified is
    /// nobody's business.
    pub email_verified_at: Option<DateTime<Utc>>,
}

impl From<auth::AccountSummary> for AccountResponse {
    fn from(account: auth::AccountSummary) -> Self {
        Self {
            id: account.id,
            username: account.username,
            email: account.email,
            display_name: account.display_name,
            avatar_url: account.avatar_url,
            bio: account.bio,
            banner_url: account.banner_url,
            accent_color: account.accent_color,
            pronouns: account.pronouns,
            created_at: account.created_at,
            email_verified_at: account.email_verified_at,
        }
    }
}

/// Query string of `GET /accounts/{id}` — `?server_id=` opts into the
/// per-server nickname/roles block, when the caller and the profile owner
/// share that server (see `ProfileServerContextResponse`).
#[derive(Debug, Deserialize)]
pub struct ProfileQuery {
    #[serde(default)]
    pub server_id: Option<Uuid>,
}

/// The "someone else's profile" shape: what a caller sees for *any* other
/// account, self included via `GET /accounts/me` mapping separately to
/// `AccountResponse`. Built ONLY by [`ProfileResponse::build`], from a
/// `domain::ProfileVisibilityDecision` — there is no `From<ProfileContext>`
/// or `From<db::profile::ProfileRow>` on purpose, because either would let a
/// caller construct this response from raw profile data without running it
/// through the privacy decision first.
#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    id: Uuid,
    username: String,
    display_name: String,
    avatar_url: Option<String>,
    banner_url: Option<String>,
    accent_color: Option<String>,
    bio: Option<String>,
    pronouns: Option<String>,
    links: Vec<ProfileLinkResponse>,
    created_at: DateTime<Utc>,
    presence: ProfilePresenceResponse,
    custom_status: Option<ProfileCustomStatusResponse>,
    /// Always `null` in M1 — no rich activity payload exists yet (B2).
    activity: Option<()>,
    /// Always empty in M1 — no badges exist yet (B7).
    badges: Vec<()>,
    relationship: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_context: Option<ProfileServerContextResponse>,
    flags: ProfileFlagsResponse,
}

#[derive(Debug, Serialize)]
pub struct ProfileLinkResponse {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct ProfilePresenceResponse {
    /// The persisted manual status (`online`/`idle`/`dnd`/`invisible`), or
    /// the fixed literal `"offline"` when the decision forces presence
    /// hidden — never read from the profile row in that case.
    pub status: String,
    pub online: bool,
}

#[derive(Debug, Serialize)]
pub struct ProfileCustomStatusResponse {
    pub text: String,
    pub emoji: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ProfileServerContextResponse {
    pub server_id: Uuid,
    pub nickname: Option<String>,
    pub roles: Vec<ProfileServerRoleResponse>,
    pub joined_at: DateTime<Utc>,
}

/// Deliberately not the full `RoleResponse` (permissions/position have no
/// reason to appear in a profile popout) — the M1 contract asks for exactly
/// `id`/`name`/`color`.
#[derive(Debug, Serialize)]
pub struct ProfileServerRoleResponse {
    pub id: Uuid,
    pub name: String,
    pub color: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProfileFlagsResponse {
    pub deleted: bool,
    /// Always `false` — there is no system-account concept yet.
    pub system: bool,
}

impl ProfileResponse {
    /// The only constructor. Takes the already-decided
    /// `domain::ProfileVisibilityDecision` and maps each field strictly
    /// according to its exposure — never the raw `ctx.profile` value
    /// directly. `online` is the realtime hub's answer for this account;
    /// passing a real value when `decision.presence` is `ForcedOffline` is
    /// harmless because that branch never reads it.
    pub(crate) fn build(
        ctx: domain::ProfileContext,
        decision: domain::ProfileVisibilityDecision,
        online: bool,
        requested_server_id: Option<Uuid>,
    ) -> Self {
        let domain::ProfileContext {
            profile,
            server_context,
            ..
        } = ctx;

        let display_name = match decision.identity {
            domain::ProfileIdentityExposure::Tombstone => "Deleted User".to_string(),
            domain::ProfileIdentityExposure::Visible => profile.display_name,
        };

        let (avatar_url, banner_url, accent_color) = match decision.media {
            domain::ProfileMediaExposure::Actual => {
                (profile.avatar_url, profile.banner_url, profile.accent_color)
            }
            domain::ProfileMediaExposure::Default => (None, None, None),
        };

        let (bio, pronouns, links) = match decision.bio_pronouns_and_links {
            domain::ProfileFieldExposure::Visible => (
                profile.bio,
                profile.pronouns,
                profile
                    .links
                    .into_iter()
                    .map(|link| ProfileLinkResponse {
                        label: link.label,
                        url: link.url,
                    })
                    .collect(),
            ),
            domain::ProfileFieldExposure::Hidden => (None, None, Vec::new()),
        };

        let presence = match decision.presence_for_status(&profile.status) {
            domain::ProfilePresenceExposure::Real => ProfilePresenceResponse {
                status: profile.status,
                online,
            },
            domain::ProfilePresenceExposure::ForcedOffline => ProfilePresenceResponse {
                status: "offline".to_string(),
                online: false,
            },
        };

        let custom_status = match decision.custom_status {
            domain::ProfileFieldExposure::Visible => {
                profile
                    .custom_status
                    .map(|text| ProfileCustomStatusResponse {
                        text,
                        emoji: profile.custom_emoji,
                        expires_at: profile.custom_expires_at,
                    })
            }
            domain::ProfileFieldExposure::Hidden => None,
        };

        let relationship = match decision.relationship {
            domain::ProfileRelationshipExposure::SelfView => "self",
            domain::ProfileRelationshipExposure::Friend => "friend",
            domain::ProfileRelationshipExposure::NoRelationship => "none",
            domain::ProfileRelationshipExposure::Blocked => "blocked",
            // `Minimum` and `None` both collapse to "none" on purpose: the
            // domain decision already refuses to disclose an inbound block
            // or its direction (see `profile_visibility.rs`), and leaking
            // that distinction in the relationship string would undo it.
            domain::ProfileRelationshipExposure::Minimum => "none",
            domain::ProfileRelationshipExposure::None => "none",
        };

        let server_context = match (decision.server_context, requested_server_id) {
            (domain::ProfileFieldExposure::Visible, Some(server_id)) => {
                server_context.map(|ctx| ProfileServerContextResponse {
                    server_id,
                    nickname: ctx.nickname,
                    roles: ctx
                        .roles
                        .into_iter()
                        .map(|role| ProfileServerRoleResponse {
                            id: role.id,
                            name: role.name,
                            color: role.color,
                        })
                        .collect(),
                    joined_at: ctx.joined_at,
                })
            }
            _ => None,
        };

        Self {
            id: profile.id,
            username: profile.username,
            display_name,
            avatar_url,
            banner_url,
            accent_color,
            bio,
            pronouns,
            links,
            created_at: profile.created_at,
            presence,
            custom_status,
            activity: None,
            badges: Vec::new(),
            relationship,
            server_context,
            flags: ProfileFlagsResponse {
                deleted: decision.identity == domain::ProfileIdentityExposure::Tombstone,
                system: false,
            },
        }
    }
}

#[cfg(test)]
mod profile_response_tests {
    use super::ProfileResponse;
    use chrono::Utc;

    /// A profile with every masked field populated (non-null custom status,
    /// a server context), so a mapper that ignores the decision and reads
    /// the raw row anyway is caught red-handed instead of coincidentally
    /// passing because the field happened to be empty.
    fn populated_context() -> domain::ProfileContext {
        domain::ProfileContext {
            profile: db::profile::ProfileRow {
                id: app_core::new_id(),
                username: "alice".to_string(),
                display_name: "Alice".to_string(),
                avatar_url: Some("https://example.com/a.png".to_string()),
                bio: Some("hi".to_string()),
                banner_url: Some("https://example.com/b.png".to_string()),
                accent_color: Some("#4A90E2".to_string()),
                pronouns: Some("she/her".to_string()),
                created_at: Utc::now(),
                status: "dnd".to_string(),
                custom_status: Some("in a meeting".to_string()),
                custom_emoji: Some("📞".to_string()),
                custom_expires_at: None,
                theme: "{}".to_string(),
                vis_bio: "public".to_string(),
                vis_communities: "public".to_string(),
                vis_friends: "public".to_string(),
                deleted_at: None,
                links: vec![db::profile::ProfileLinkRow {
                    id: app_core::new_id(),
                    label: "github".to_string(),
                    url: "https://github.com/alice".to_string(),
                    position: 0,
                }],
            },
            relationship: domain::ProfileViewerRelationship::Friend,
            caller_blocked_owner: false,
            owner_blocked_caller: false,
            server_context: Some(db::profile::ServerContextRow {
                nickname: Some("ally".to_string()),
                joined_at: Utc::now(),
                roles: vec![db::server_role::ServerRoleRow {
                    id: app_core::new_id(),
                    server_id: app_core::new_id(),
                    name: "admin".to_string(),
                    color: Some("#FF0000".to_string()),
                    permissions: 0,
                    position: 0,
                    is_default: false,
                    created_at: Utc::now(),
                    mentionable: false,
                }],
            }),
            has_shared_server_context: true,
        }
    }

    fn visible_decision() -> domain::ProfileVisibilityDecision {
        domain::decide_profile_visibility(domain::ProfileVisibilityInput {
            relationship: domain::ProfileViewerRelationship::Friend,
            vis_bio: domain::ProfileVisibility::Public,
            vis_communities: domain::ProfileVisibility::Public,
            vis_friends: domain::ProfileVisibility::Public,
            caller_blocked_owner: false,
            owner_blocked_caller: false,
            has_shared_server_context: true,
            is_deleted: false,
        })
    }

    #[test]
    fn an_inbound_block_masks_custom_status_and_server_context_even_with_real_data() {
        let ctx = populated_context();
        let decision = domain::decide_profile_visibility(domain::ProfileVisibilityInput {
            relationship: domain::ProfileViewerRelationship::Friend,
            vis_bio: domain::ProfileVisibility::Public,
            vis_communities: domain::ProfileVisibility::Public,
            vis_friends: domain::ProfileVisibility::Public,
            caller_blocked_owner: false,
            owner_blocked_caller: true,
            has_shared_server_context: true,
            is_deleted: false,
        });

        // `online: true` on purpose — a real live connection — to prove the
        // mapper does not leak it once the decision forces presence hidden.
        let response = ProfileResponse::build(ctx, decision, true, Some(app_core::new_id()));

        assert_eq!(response.presence.status, "offline");
        assert!(!response.presence.online);
        assert!(response.custom_status.is_none());
        assert!(response.server_context.is_none());
        assert_eq!(response.relationship, "none");
    }

    #[test]
    fn a_deleted_account_masks_custom_status_and_server_context_even_with_real_data() {
        let ctx = populated_context();
        let decision = domain::decide_profile_visibility(domain::ProfileVisibilityInput {
            relationship: domain::ProfileViewerRelationship::Friend,
            vis_bio: domain::ProfileVisibility::Public,
            vis_communities: domain::ProfileVisibility::Public,
            vis_friends: domain::ProfileVisibility::Public,
            caller_blocked_owner: false,
            owner_blocked_caller: false,
            has_shared_server_context: true,
            is_deleted: true,
        });

        let response = ProfileResponse::build(ctx, decision, true, Some(app_core::new_id()));

        assert_eq!(response.display_name, "Deleted User");
        assert_eq!(response.presence.status, "offline");
        assert!(!response.presence.online);
        assert!(response.custom_status.is_none());
        assert!(response.server_context.is_none());
        assert!(response.avatar_url.is_none());
        assert_eq!(response.relationship, "none");
        assert!(response.flags.deleted);
    }

    #[test]
    fn a_visible_decision_carries_the_real_custom_status_and_server_context_through() {
        let ctx = populated_context();
        let server_id = app_core::new_id();

        let response = ProfileResponse::build(ctx, visible_decision(), true, Some(server_id));

        let custom_status = response.custom_status.expect("custom status is visible");
        assert_eq!(custom_status.text, "in a meeting");
        let server_context = response.server_context.expect("server context is visible");
        assert_eq!(server_context.server_id, server_id);
        assert_eq!(server_context.nickname.as_deref(), Some("ally"));
        assert!(response.presence.online);
    }
}

/// Turns an absent key and an explicit `null` into different values.
///
/// Serde collapses both into `None` for a plain `Option<T>`, which is fine
/// while every field is "set it or leave it". It stops being fine once a
/// field is nullable and clearable: `{"bio": null}` means *remove my bio*,
/// and `{}` means *do not touch my bio*, and the API has to tell them apart.
///
/// With `#[serde(default, deserialize_with = "double_option")]`:
/// absent -> `None`, `null` -> `Some(None)`, value -> `Some(Some(v))`.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// `PATCH /accounts/me` body. Every field is optional; `#[serde(default)]`
/// makes an omitted key deserialize to `None` instead of a parse error, so a
/// client can send just `{ "display_name": "..." }` and leave the rest
/// untouched.
///
/// `username` and `display_name` are NOT NULL columns and stay plain
/// `Option`. The nullable profile fields use `double_option` so they can be
/// cleared — see its doc comment.
#[derive(Debug, Deserialize)]
pub struct UpdateAccountRequest {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub avatar_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub bio: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub banner_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub accent_color: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub pronouns: Option<Option<String>>,
}

impl From<UpdateAccountRequest> for auth::UpdateAccountInput {
    fn from(req: UpdateAccountRequest) -> Self {
        Self {
            username: req.username,
            display_name: req.display_name,
            avatar_url: req.avatar_url,
            bio: req.bio,
            banner_url: req.banner_url,
            accent_color: req.accent_color,
            pronouns: req.pronouns,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

impl From<LoginRequest> for auth::LoginInput {
    fn from(req: LoginRequest) -> Self {
        Self {
            email: req.email,
            password: req.password,
        }
    }
}

/// Never carries `token_hash` — only what a client needs to display/manage
/// its own sessions.
#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl From<auth::SessionSummary> for SessionResponse {
    fn from(session: auth::SessionSummary) -> Self {
        Self {
            id: session.id,
            created_at: session.created_at,
            last_used_at: session.last_used_at,
            expires_at: session.expires_at,
        }
    }
}

/// The one time the raw token is ever available in plaintext — the login
/// response body. Desktop/other clients read `token` from the body; web
/// clients get the same session via the `r4chii_session` cookie set
/// alongside it — one session model, two transports.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    #[serde(flatten)]
    pub session: SessionResponse,
    pub token: String,
}

/// Collection envelope. `next_cursor` is
/// always `null` here: a max-4-item session list has no real pagination
/// need, but the shape stays consistent with the rest of the API.
#[derive(Debug, Serialize)]
pub struct SessionListResponse {
    pub items: Vec<SessionResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateServerRequest {
    pub name: String,
    #[serde(default)]
    pub visibility: Option<String>,
}

impl From<CreateServerRequest> for domain::CreateServerInput {
    fn from(req: CreateServerRequest) -> Self {
        Self {
            name: req.name,
            visibility: req.visibility,
        }
    }
}

/// `invite_code` is `None` unless the caller viewing this response is that
/// server's owner — decided in `domain::DomainService`, not here (a
/// business rule, not a response-shaping concern).
#[derive(Debug, Serialize)]
pub struct ServerResponse {
    pub id: Uuid,
    pub name: String,
    pub icon_url: Option<String>,
    pub visibility: String,
    pub owner_account_id: Uuid,
    pub invite_code: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<domain::ServerSummary> for ServerResponse {
    fn from(server: domain::ServerSummary) -> Self {
        Self {
            id: server.id,
            name: server.name,
            icon_url: server.icon_url,
            visibility: server.visibility,
            owner_account_id: server.owner_account_id,
            invite_code: server.invite_code,
            created_at: server.created_at,
        }
    }
}

/// Collection envelope. `next_cursor`
/// stays `null`: no real pagination need at M0 scale (a handful of
/// friends' servers).
#[derive(Debug, Serialize)]
pub struct ServerListResponse {
    pub items: Vec<ServerResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    /// Optional so every pre-voice client keeps working — omitted means
    /// `text`. `domain`'s `validate_channel_kind` is what narrows this to
    /// `text`/`voice`; this layer only parses it.
    #[serde(default)]
    pub kind: Option<String>,
}

impl From<CreateChannelRequest> for domain::CreateChannelInput {
    fn from(req: CreateChannelRequest) -> Self {
        Self {
            name: req.name,
            kind: req.kind,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ChannelResponse {
    pub id: Uuid,
    /// `null` for `dm`/`group_dm` channels.
    pub server_id: Option<Uuid>,
    pub kind: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    /// `null` = inherit the server's visibility unchanged.
    pub visibility: Option<String>,
    /// The parent text channel a `thread` carries. `null` for
    /// every other kind.
    pub parent_channel_id: Option<Uuid>,
    /// The message a thread was spawned from, if any. `null` for a
    /// standalone thread or any non-thread kind.
    pub root_message_id: Option<Uuid>,
    /// A thread's display title. `null` for every non-thread kind (which use
    /// `name` instead).
    pub title: Option<String>,
    /// URL-safe, unique per `parent_channel_id`. `null` for every non-thread
    /// kind.
    pub slug: Option<String>,
    /// RAW value of this channel's own column — always `false` for
    /// a `thread` (it inherits its parent's restriction rather than carrying
    /// its own; the client resolves a thread's effective restriction from
    /// its parent channel, same as everywhere else in this API).
    pub restricted: bool,
}

impl From<domain::ChannelSummary> for ChannelResponse {
    fn from(channel: domain::ChannelSummary) -> Self {
        Self {
            id: channel.id,
            server_id: channel.server_id,
            kind: channel.kind,
            name: channel.name,
            created_at: channel.created_at,
            visibility: channel.visibility,
            parent_channel_id: channel.parent_channel_id,
            root_message_id: channel.root_message_id,
            title: channel.title,
            slug: channel.slug,
            restricted: channel.restricted,
        }
    }
}

/// `PATCH .../channels/{id}/restricted` body.
#[derive(Debug, Deserialize)]
pub struct UpdateChannelRestrictedRequest {
    pub restricted: bool,
}

/// `PUT .../channels/{id}/permissions/{role_id}` body — full
/// replace, not a bit-flip (matches this codebase's existing convention for
/// permission bodies).
#[derive(Debug, Deserialize)]
pub struct SetChannelRolePermissionRequest {
    pub permissions: i64,
}

/// One row of `GET .../channels/{id}/permissions`.
#[derive(Debug, Serialize)]
pub struct ChannelRolePermissionResponse {
    pub role_id: Uuid,
    pub permissions: i64,
}

#[derive(Debug, Serialize)]
pub struct ChannelRolePermissionListResponse {
    pub items: Vec<ChannelRolePermissionResponse>,
}

/// `root_message_id` omitted or `null` means a standalone thread
/// (a new top-level topic in the channel) rather than a reply thread spawned
/// from an existing message.
#[derive(Debug, Deserialize)]
pub struct CreateThreadRequest {
    pub title: String,
    #[serde(default)]
    pub root_message_id: Option<Uuid>,
}

impl From<CreateThreadRequest> for domain::CreateThreadInput {
    fn from(req: CreateThreadRequest) -> Self {
        Self {
            title: req.title,
            root_message_id: req.root_message_id,
        }
    }
}

/// `visibility` is required (not `#[serde(default)]`) so a caller
/// must be explicit — either a value to narrow to, or `null` to clear the
/// override back to "inherit the server's".
#[derive(Debug, Deserialize)]
pub struct UpdateChannelVisibilityRequest {
    pub visibility: Option<String>,
}

/// Flips `server.visibility` outright, owner-only.
#[derive(Debug, Deserialize)]
pub struct UpdateServerVisibilityRequest {
    pub visibility: String,
}

/// Collection envelope. `next_cursor`
/// stays `null`: no real pagination need at M0 scale.
#[derive(Debug, Serialize)]
pub struct ChannelListResponse {
    pub items: Vec<ChannelResponse>,
    pub next_cursor: Option<String>,
}

/// One member of a server: their public profile plus the role they hold
/// there. Shares `ProfileResponse`'s privacy stance — no `email`,
/// because this is someone else's profile, not the caller's own.
#[derive(Debug, Serialize)]
pub struct ServerMemberResponse {
    pub account_id: Uuid,
    pub username: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    /// `owner` | `member` (the `membership.role` CHECK constraint).
    pub role: String,
    pub joined_at: DateTime<Utc>,
    /// `online` | `offline`, the same two states `presence.update` carries
    /// Not stored anywhere — read from the
    /// realtime hub's live connection registry at request time, so the
    /// client's first render is already correct before any socket event
    /// arrives and the events only have to carry deltas from there.
    pub status: realtime::PresenceStatus,
    /// M2: the member's EXPLICITLY assigned role ids — never includes the
    /// server's default role, which every member holds implicitly.
    pub role_ids: Vec<Uuid>,
    /// `null` means the account's own `display_name` renders
    /// instead.
    pub nickname: Option<String>,
    /// `null`, or a future timestamp while timed out.
    pub timeout_until: Option<DateTime<Utc>>,
}

impl ServerMemberResponse {
    /// Presence deliberately does not come from `domain::ServerMemberSummary`
    /// — it is not part of the membership row, it is live socket state owned
    /// by `realtime` — so this replaces the plain `From` impl the other DTOs
    /// use, forcing every caller to say where the status came from.
    pub fn new(member: domain::ServerMemberSummary, status: realtime::PresenceStatus) -> Self {
        Self {
            account_id: member.account_id,
            username: member.username,
            display_name: member.display_name,
            avatar_url: member.avatar_url,
            role: member.role,
            joined_at: member.joined_at,
            status,
            role_ids: member.role_ids,
            nickname: member.nickname,
            timeout_until: member.timeout_until,
        }
    }
}

/// Collection envelope. `next_cursor`
/// stays `null`: no real pagination need at M0 scale (a handful of friends
/// per server).
#[derive(Debug, Serialize)]
pub struct ServerMemberListResponse {
    pub items: Vec<ServerMemberResponse>,
    pub next_cursor: Option<String>,
}

/// A `server_role` row (M2).
#[derive(Debug, Serialize)]
pub struct RoleResponse {
    pub id: Uuid,
    pub server_id: Uuid,
    pub name: String,
    pub color: Option<String>,
    pub permissions: i64,
    pub position: i32,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    /// When `true`, ANY member may `@`-mention this role.
    pub mentionable: bool,
}

impl From<domain::RoleSummary> for RoleResponse {
    fn from(role: domain::RoleSummary) -> Self {
        Self {
            id: role.id,
            server_id: role.server_id,
            name: role.name,
            color: role.color,
            permissions: role.permissions,
            position: role.position,
            is_default: role.is_default,
            created_at: role.created_at,
            mentionable: role.mentionable,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RoleListResponse {
    pub items: Vec<RoleResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRoleRequest {
    pub name: String,
}

impl From<CreateRoleRequest> for domain::CreateRoleInput {
    fn from(req: CreateRoleRequest) -> Self {
        Self { name: req.name }
    }
}

/// Every field optional — omitted (or `null`) means "leave it as it is" (the
/// service layer reads the existing row for anything left out). No path to
/// clear a color back to "no override" yet; add one if that's ever actually
/// needed rather than modeling it speculatively now.
#[derive(Debug, Deserialize)]
pub struct UpdateRoleRequest {
    pub name: Option<String>,
    pub color: Option<String>,
    pub permissions: Option<i64>,
    /// `None`/omitted leaves the existing value alone.
    #[serde(default)]
    pub mentionable: Option<bool>,
}

impl From<UpdateRoleRequest> for domain::UpdateRoleInput {
    fn from(req: UpdateRoleRequest) -> Self {
        Self {
            name: req.name,
            color: req.color,
            permissions: req.permissions,
            mentionable: req.mentionable,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReorderRolesRequest {
    pub role_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct SetMemberRolesRequest {
    pub role_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct MemberRolesResponse {
    pub role_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct CreateBanRequest {
    pub account_id: Uuid,
    pub reason: Option<String>,
}

/// A `server_ban` row (M2). Deliberately no `id` — `account_id` is already
/// the natural key a client acts on (unban takes it, not the ban row's own
/// id), same id-semantics choice `FriendshipResponse`/`BlockResponse` made.
#[derive(Debug, Serialize)]
pub struct BanResponse {
    pub account_id: Uuid,
    pub banned_by: Uuid,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<domain::BanSummary> for BanResponse {
    fn from(ban: domain::BanSummary) -> Self {
        Self {
            account_id: ban.account_id,
            banned_by: ban.banned_by,
            reason: ban.reason,
            created_at: ban.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BanListResponse {
    pub items: Vec<BanResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
}

impl From<SendMessageRequest> for domain::SendMessageInput {
    fn from(req: SendMessageRequest) -> Self {
        Self {
            content: req.content,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct EditMessageRequest {
    pub content: String,
}

impl From<EditMessageRequest> for domain::EditMessageInput {
    fn from(req: EditMessageRequest) -> Self {
        Self {
            content: req.content,
        }
    }
}

/// `download_url` is `null` until `status == "done"`.
#[derive(Debug, Serialize)]
pub struct ExportJobResponse {
    pub id: Uuid,
    pub server_id: Uuid,
    pub status: String,
    pub download_url: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl From<domain::ExportJobSummary> for ExportJobResponse {
    fn from(job: domain::ExportJobSummary) -> Self {
        Self {
            id: job.id,
            server_id: job.server_id,
            status: job.status,
            download_url: job.download_url,
            error: job.error,
            created_at: job.created_at,
            completed_at: job.completed_at,
        }
    }
}

/// `?limit=&before=` — the frozen cursor
/// pagination (`?limit=50&before=<cursor>`). Both optional; `domain`'s own
/// default/cap apply when `limit` is omitted or over the cap.
#[derive(Debug, Deserialize)]
pub struct MessageListQuery {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub before: Option<Uuid>,
}

/// `before` is the same opaque cursor every other list endpoint
/// uses; the date-range filters are deliberately named `since`/`until`
/// rather than `after`/`before` so they can never collide with that cursor
/// param's own name.
#[derive(Debug, Deserialize)]
pub struct MessageSearchQuery {
    pub q: String,
    #[serde(default)]
    pub author: Option<Uuid>,
    #[serde(default)]
    pub channel: Option<Uuid>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub before: Option<Uuid>,
}

impl From<MessageSearchQuery> for domain::SearchInput {
    fn from(query: MessageSearchQuery) -> Self {
        Self {
            query: query.q,
            author_account_id: query.author,
            channel_id: query.channel,
            created_after: query.since,
            created_before: query.until,
            limit: query.limit,
            before: query.before,
        }
    }
}

/// `content` is `None` for a soft-deleted message
/// — the row stays, its content doesn't; `deleted_at` is present so a
/// client can render a "message deleted" placeholder.
#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_account_id: Uuid,
    pub content: Option<String>,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// `null` = not pinned.
    pub pinned_at: Option<DateTime<Utc>>,
}

impl From<domain::MessageSummary> for MessageResponse {
    fn from(message: domain::MessageSummary) -> Self {
        Self {
            id: message.id,
            channel_id: message.channel_id,
            author_account_id: message.author_account_id,
            content: message.content,
            created_at: message.created_at,
            edited_at: message.edited_at,
            deleted_at: message.deleted_at,
            pinned_at: message.pinned_at,
        }
    }
}

/// `POST/DELETE .../members/{account_id}/timeout` body — `until`
/// must be a future RFC 3339 timestamp; `domain` rejects a past one via the
/// same validation the endpoint's own doc comment describes.
#[derive(Debug, Deserialize)]
pub struct TimeoutRequest {
    pub until: DateTime<Utc>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl From<TimeoutRequest> for domain::TimeoutInput {
    fn from(req: TimeoutRequest) -> Self {
        Self {
            until: req.until,
            reason: req.reason,
        }
    }
}

/// `PATCH .../members/{account_id}/nickname` body. `nickname: null`
/// clears it back to "no override".
#[derive(Debug, Deserialize)]
pub struct UpdateNicknameRequest {
    pub nickname: Option<String>,
}

/// Collection envelope. Unlike the other
/// list responses in this file, `next_cursor` here is real: the last item's
/// id when a full page came back (there may be more to page to), `null`
/// when the page came back short (nothing older is left).
#[derive(Debug, Serialize)]
pub struct MessageListResponse {
    pub items: Vec<MessageResponse>,
    pub next_cursor: Option<String>,
}

/// `POST /dms` body (ROADMAP slice 6): the other participant of the 1:1 dm
/// to get-or-create.
#[derive(Debug, Deserialize)]
pub struct CreateDmRequest {
    pub account_id: Uuid,
}

/// `POST /group-dms` body. The caller is always added as a member alongside
/// these — not part of the request.
#[derive(Debug, Deserialize)]
pub struct CreateGroupDmRequest {
    pub account_ids: Vec<Uuid>,
}

impl From<CreateGroupDmRequest> for domain::CreateGroupDmInput {
    fn from(req: CreateGroupDmRequest) -> Self {
        Self {
            account_ids: req.account_ids,
        }
    }
}

/// `POST /friends` body (ROADMAP slice 7): the account to send a friend
/// request to, or accept one from.
#[derive(Debug, Deserialize)]
pub struct SendFriendRequestRequest {
    pub account_id: Uuid,
}

/// A friendship from the caller's point of view — `account_id` is always
/// the OTHER party (`domain::FriendshipSummary`'s own shape).
#[derive(Debug, Serialize)]
pub struct FriendshipResponse {
    pub id: Uuid,
    pub account_id: Uuid,
    pub status: String,
    pub requested_by: Uuid,
    pub created_at: DateTime<Utc>,
}

impl From<domain::FriendshipSummary> for FriendshipResponse {
    fn from(friendship: domain::FriendshipSummary) -> Self {
        Self {
            id: friendship.id,
            account_id: friendship.account_id,
            status: friendship.status,
            requested_by: friendship.requested_by,
            created_at: friendship.created_at,
        }
    }
}

/// Collection envelope. `next_cursor`
/// stays `null`: no real pagination need at M0 scale (a handful of
/// friends).
#[derive(Debug, Serialize)]
pub struct FriendshipListResponse {
    pub items: Vec<FriendshipResponse>,
    pub next_cursor: Option<String>,
}

/// `POST /blocks` body.
#[derive(Debug, Deserialize)]
pub struct CreateBlockRequest {
    pub account_id: Uuid,
}

/// `account_id` is the blocked account — `blocker_account_id` is always the
/// caller, so it's never in the response (`domain::BlockSummary`'s shape).
#[derive(Debug, Serialize)]
pub struct BlockResponse {
    pub id: Uuid,
    pub account_id: Uuid,
    pub created_at: DateTime<Utc>,
}

impl From<domain::BlockSummary> for BlockResponse {
    fn from(block: domain::BlockSummary) -> Self {
        Self {
            id: block.id,
            account_id: block.account_id,
            created_at: block.created_at,
        }
    }
}

/// Collection envelope. `next_cursor`
/// stays `null`: no real pagination need at M0 scale.
#[derive(Debug, Serialize)]
pub struct BlockListResponse {
    pub items: Vec<BlockResponse>,
    pub next_cursor: Option<String>,
}
