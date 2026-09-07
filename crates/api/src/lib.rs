mod dto;
mod error;
mod extract;
mod gateway;
mod handlers;
mod media;
mod public;
mod rate_limit;
mod voice;

use axum::{
    extract::{DefaultBodyLimit, FromRef},
    routing::{delete, get, patch, post, put},
    Router,
};
use tower_governor::{
    governor::{GovernorConfig, GovernorConfigBuilder},
    key_extractor::PeerIpKeyExtractor,
    GovernorLayer,
};

pub use extract::{AuthenticatedUser, SESSION_COOKIE_NAME};

/// Shared application state handed to every route. `auth` (slice 2),
/// `domain` (slice 4), and `realtime` (slice 5) are the services wired up so
/// far.
#[derive(Clone)]
pub struct AppState {
    pub auth: auth::AuthService,
    pub domain: domain::DomainService,
    pub realtime: realtime::Hub,
    /// Absent when S3 is not configured. Uploads answer with a clean error
    /// rather than the process refusing to start, matching how the export
    /// worker treats the same absence.
    pub storage: Option<storage::StorageService>,
}

impl FromRef<AppState> for auth::AuthService {
    fn from_ref(state: &AppState) -> Self {
        state.auth.clone()
    }
}

impl FromRef<AppState> for domain::DomainService {
    fn from_ref(state: &AppState) -> Self {
        state.domain.clone()
    }
}

impl FromRef<AppState> for realtime::Hub {
    fn from_ref(state: &AppState) -> Self {
        state.realtime.clone()
    }
}

pub fn router(state: AppState) -> Router {
    // Registration, verification, and login — every unauthenticated entry
    // point (OAuth/2FA/recovery codes are still M2+).
    // `secure()` = 2 requests / 4s replenish, tuned against brute-force login
    // and registration abuse.
    //
    // This limiter is per-IP and runs before the body is parsed, so it cannot
    // see which address a request names. The per-address gap that stops the
    // issue endpoints being a spam cannon lives in `auth` instead, where the
    // email is actually known.
    //
    // Requires `axum::serve` to be called with
    // `into_make_service_with_connect_info::<SocketAddr>()` so the governor
    // can see the real peer IP (wired in crates/server/src/lib.rs).
    let rate_limited_config: GovernorConfig<PeerIpKeyExtractor, governor::middleware::NoOpMiddleware<governor::clock::QuantaInstant>> =
        GovernorConfig::secure();

    let rate_limited = Router::new()
        .route("/api/v1/registrations", post(handlers::start_registration))
        .route(
            "/api/v1/registration-codes",
            post(handlers::resend_registration_code),
        )
        .route("/api/v1/accounts", post(handlers::register))
        .route("/api/v1/sessions", post(handlers::login))
        .layer(GovernorLayer::new(rate_limited_config).error_handler(rate_limit::error_response))
        .with_state(state.clone());

    // Kick/ban/delete-server (M2): real
    // moderator use is bursty (banning a handful of accounts back to back is
    // normal), so this is far looser than the auth group's brute-force-tuned
    // limit — just enough to blunt a scripting mistake, not to withstand an
    // attack.
    let moderation_config: GovernorConfig<
        PeerIpKeyExtractor,
        governor::middleware::NoOpMiddleware<governor::clock::QuantaInstant>,
    > = GovernorConfigBuilder::default()
        .per_second(10)
        .burst_size(5)
        .finish()
        .expect("moderation rate limit config is valid (non-zero period and burst)");

    // Editing your own profile is a write, and a person doing it saves a few
    // times in a row while adjusting things — hence a burst with a slow
    // replenish, rather than the auth group's brute-force tuning.
    let profile_write_config: GovernorConfig<
        PeerIpKeyExtractor,
        governor::middleware::NoOpMiddleware<governor::clock::QuantaInstant>,
    > = GovernorConfigBuilder::default()
        .per_second(3)
        .burst_size(5)
        .finish()
        .expect("profile write rate limit config is valid (non-zero period and burst)");

    let profile_writes = Router::new()
        .route("/api/v1/accounts/me", patch(handlers::update_own_account))
        // The body limit is the cap `domain::media` states for each purpose,
        // applied here so an oversized upload is refused as it arrives rather
        // than after it has been read into memory.
        .route(
            "/api/v1/accounts/me/avatar",
            post(media::upload_avatar).layer(DefaultBodyLimit::max(
                domain::ImagePurpose::Avatar.max_upload_bytes(),
            )),
        )
        .route(
            "/api/v1/accounts/me/banner",
            post(media::upload_banner).layer(DefaultBodyLimit::max(
                domain::ImagePurpose::Banner.max_upload_bytes(),
            )),
        )
        .layer(GovernorLayer::new(profile_write_config).error_handler(rate_limit::error_response))
        .with_state(state.clone());

    let moderation = Router::new()
        .route(
            "/api/v1/servers/{id}/members/{account_id}",
            delete(handlers::kick_member),
        )
        .route("/api/v1/servers/{id}/bans", post(handlers::create_ban))
        .route("/api/v1/servers/{id}", delete(handlers::delete_server))
        // A full-server export is the same bulk-administrative-action tier
        // as kick/ban/delete-server, rate-limited the same way.
        .route(
            "/api/v1/servers/{id}/exports",
            post(handlers::request_export),
        )
        // Timeout targets a specific member the same way kick/ban do —
        // same rate-limit tier.
        .route(
            "/api/v1/servers/{id}/members/{account_id}/timeout",
            post(handlers::timeout_member),
        )
        .route(
            "/api/v1/servers/{id}/members/{account_id}/timeout",
            delete(handlers::clear_timeout),
        )
        // Regenerating the invite code invalidates the old one immediately
        // — a scripting mistake here locks out every pending invitee, worth
        // the same guardrail kick/ban/delete-server get.
        .route(
            "/api/v1/servers/{id}/invite-code/regenerate",
            post(handlers::regenerate_invite_code),
        )
        .layer(GovernorLayer::new(moderation_config).error_handler(rate_limit::error_response))
        .with_state(state.clone());

    // Account reads are not auth/recovery endpoints, so no rate limiting
    // here; the profile write is registered in its own group above.
    // `/accounts/me`, `/accounts/by-username/{username}` and `/accounts/bulk`
    // are registered as static routes ahead of the `/accounts/{id}` dynamic
    // route — axum's router prefers a static match over a param match, so none
    // of them falls into the `{id}` handler.
    let unlimited = Router::new()
        .route("/api/v1/sessions", get(handlers::list_sessions))
        .route("/api/v1/sessions", delete(handlers::revoke_all_sessions))
        .route("/api/v1/sessions/{id}", delete(handlers::revoke_session))
        .route("/api/v1/voice/ice", get(voice::get_ice_config))
        .route(
            "/api/v1/servers/{id}/voice",
            get(voice::get_server_voice_rosters),
        )
        .route("/api/v1/accounts/me", get(handlers::get_own_account))
        .route(
            "/api/v1/accounts/by-username/{username}",
            get(handlers::get_account_by_username),
        )
        .route("/api/v1/accounts/bulk", post(handlers::get_accounts_bulk))
        .route("/api/v1/media/{*key}", get(media::get_media))
        .route("/api/v1/accounts/{id}", get(handlers::get_account))
        .route("/api/v1/servers", post(handlers::create_server))
        .route("/api/v1/servers", get(handlers::list_servers))
        .route("/api/v1/servers/{id}", get(handlers::get_server))
        .route(
            "/api/v1/servers/{id}/channels",
            post(handlers::create_channel),
        )
        .route(
            "/api/v1/servers/{id}/channels",
            get(handlers::list_channels),
        )
        .route("/api/v1/servers/{id}/members", get(handlers::list_members))
        // ---- search ----
        .route("/api/v1/servers/{id}/search", get(handlers::search_messages))
        // ---- full export ----
        // POST (create) is on the rate-limited `moderation` router above;
        // GET (poll status) is unrestricted like every other read here.
        .route(
            "/api/v1/servers/{id}/exports/{job_id}",
            get(handlers::get_export_job),
        )
        // ---- visibility model ----
        .route(
            "/api/v1/servers/{id}/visibility",
            patch(handlers::update_server_visibility),
        )
        .route(
            "/api/v1/servers/{id}/channels/{channel_id}/visibility",
            patch(handlers::update_channel_visibility),
        )
        // ---- channel permission overrides ----
        .route(
            "/api/v1/servers/{id}/channels/{channel_id}/restricted",
            patch(handlers::update_channel_restricted),
        )
        .route(
            "/api/v1/servers/{id}/channels/{channel_id}/permissions/{role_id}",
            put(handlers::set_channel_role_permission),
        )
        .route(
            "/api/v1/channels/{id}/permissions",
            get(handlers::list_channel_role_permissions),
        )
        .route(
            "/api/v1/invites/{code}/memberships",
            post(handlers::join_via_invite),
        )
        // ---- M2: roles & permissions ----
        // kick/ban/delete-server are registered on the `moderation` router
        // above instead — they're rate-limited, these aren't.
        .route("/api/v1/servers/{id}/roles", post(handlers::create_role))
        .route("/api/v1/servers/{id}/roles", get(handlers::list_roles))
        // Static "order" ahead of the dynamic `{role_id}` route below, same
        // "me"-before-`{id}` precedent the accounts routes already use —
        // axum's router prefers the static match.
        .route(
            "/api/v1/servers/{id}/roles/order",
            patch(handlers::reorder_roles),
        )
        .route(
            "/api/v1/servers/{id}/roles/{role_id}",
            patch(handlers::update_role),
        )
        .route(
            "/api/v1/servers/{id}/roles/{role_id}",
            delete(handlers::delete_role),
        )
        .route(
            "/api/v1/servers/{id}/members/{account_id}/roles",
            patch(handlers::set_member_roles),
        )
        .route("/api/v1/servers/{id}/bans", get(handlers::list_bans))
        .route(
            "/api/v1/servers/{id}/bans/{account_id}",
            delete(handlers::delete_ban),
        )
        // ---- role permissions v2 ----
        .route(
            "/api/v1/servers/{id}/members/{account_id}/nickname",
            patch(handlers::update_member_nickname),
        )
        .route(
            "/api/v1/channels/{id}/messages/{message_id}/pin",
            post(handlers::pin_message),
        )
        .route(
            "/api/v1/channels/{id}/messages/{message_id}/pin",
            delete(handlers::unpin_message),
        )
        .route(
            "/api/v1/channels/{id}/pins",
            get(handlers::list_pinned_messages),
        )
        .route("/api/v1/servers/{id}/leave", post(handlers::leave_server))
        .route("/api/v1/dms", post(handlers::create_dm))
        .route("/api/v1/dms", get(handlers::list_dms))
        .route("/api/v1/group-dms", post(handlers::create_group_dm))
        .route("/api/v1/friends", post(handlers::send_friend_request))
        .route("/api/v1/friends", get(handlers::list_friendships))
        .route("/api/v1/friends/{id}", delete(handlers::remove_friendship))
        .route("/api/v1/blocks", post(handlers::create_block))
        .route("/api/v1/blocks", get(handlers::list_blocks))
        .route("/api/v1/blocks/{id}", delete(handlers::remove_block))
        // ---- threads ----
        .route(
            "/api/v1/channels/{id}/threads",
            post(handlers::create_thread),
        )
        .route(
            "/api/v1/channels/{id}/threads",
            get(handlers::list_threads),
        )
        .route(
            "/api/v1/channels/{id}/messages",
            post(handlers::send_message),
        )
        .route(
            "/api/v1/channels/{id}/messages",
            get(handlers::list_messages),
        )
        .route(
            "/api/v1/channels/{id}/messages/{message_id}",
            patch(handlers::edit_message),
        )
        .route(
            "/api/v1/channels/{id}/messages/{message_id}",
            delete(handlers::delete_message),
        )
        // Not rate-limited, same as the other non-auth/recovery routes
        // above. Its own auth is handled inside the handler (its own
        // handshake, not `AuthenticatedUser` — an unauthenticated socket is
        // closed with code 4001, not an HTTP 401, so it can't reuse that
        // extractor).
        .route("/api/v1/gateway", get(gateway::gateway))
        .with_state(state.clone());

    Router::new()
        .merge(rate_limited)
        .merge(profile_writes)
        .merge(moderation)
        .merge(unlimited)
        // Server-rendered, unauthenticated, no shared `AppState`
        // extractor conflict — this router carries its own `State<AppState>`
        // per handler, same as every group above.
        .merge(public::router().with_state(state))
}
