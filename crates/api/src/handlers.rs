//! HTTP handlers: parse/validate input at the edge, call the `auth`
//! service, map the result to a response. No business logic here — that
//! all lives in `auth::AuthService`.

use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        Path, Query, State,
    },
    http::StatusCode,
    Json,
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use uuid::Uuid;

use crate::{
    dto::{
        AccountResponse, BanListResponse, BanResponse, BlockListResponse, BlockResponse,
        BulkProfilesRequest,
        ChannelListResponse, ChannelResponse, ChannelRolePermissionListResponse,
        ChannelRolePermissionResponse, CreateBanRequest, CreateBlockRequest,
        CreateChannelRequest, CreateDmRequest, CreateGroupDmRequest, CreateRoleRequest,
        CreateServerRequest, CreateThreadRequest, EditMessageRequest, ExportJobResponse,
        FriendshipListResponse, FriendshipResponse,
        LoginRequest, LoginResponse, MemberRolesResponse, MessageListQuery, MessageListResponse,
        MessageResponse, MessageSearchQuery, ProfileQuery, ProfileResponse,
        ProfileSummaryListResponse, ProfileSummaryResponse, RegisterRequest,
        ReorderRolesRequest,
        ResendCodeRequest, RoleListResponse, RoleResponse, SendFriendRequestRequest,
        SendMessageRequest, ServerListResponse, ServerMemberListResponse, ServerMemberResponse,
        ServerResponse, SessionListResponse, SetChannelRolePermissionRequest,
        SetMemberRolesRequest, TimeoutRequest, UpdateAccountRequest,
        UpdateChannelRestrictedRequest, UpdateChannelVisibilityRequest, UpdateNicknameRequest,
        UpdateRoleRequest, UpdateServerVisibilityRequest, VerifyRegistrationRequest,
    },
    error::ApiError,
    extract::{AuthenticatedUser, SESSION_COOKIE_NAME},
    AppState,
};

/// Matches auth's idle sliding window (`verify_session`'s 14-day check) —
/// the cookie should not outlive the session it carries.
const SESSION_COOKIE_MAX_AGE_DAYS: i64 = 14;

/// Upper bound on `POST /accounts/bulk`. Bounds the response size and the
/// array bound into the repository's single set-based query.
const MAX_BULK_PROFILE_IDS: usize = 100;

/// Starts a registration: `POST /registrations`.
///
/// 202, not 201 — nothing the caller can go and fetch was created. The account
/// only exists once the code comes back, and until then there
/// is deliberately no resource to hand them a URL for.
///
/// The response is identical whether the address was free or already taken, so
/// this cannot be used to enumerate who is registered.
pub async fn start_registration(
    State(state): State<AppState>,
    body: Result<Json<RegisterRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = body?;
    state.auth.register(body.into()).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Re-issues a code: `POST /registration-codes`.
///
/// Also uniformly 202, for the same reason: replying differently when no
/// registration is pending would leak which addresses are mid-signup.
pub async fn resend_registration_code(
    State(state): State<AppState>,
    body: Result<Json<ResendCodeRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = body?;
    state.auth.resend_verification_code(&body.email).await?;
    Ok(StatusCode::ACCEPTED)
}

/// Creates the account: `POST /accounts`.
///
/// Still the account-creating endpoint it always was; it now requires proof of
/// the address rather than taking it on faith.
pub async fn register(
    State(state): State<AppState>,
    body: Result<Json<VerifyRegistrationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<AccountResponse>), ApiError> {
    let Json(body) = body?;
    let account = state.auth.verify_registration(body.into()).await?;
    // A newly created account has no profile links yet.
    Ok((
        StatusCode::CREATED,
        Json(AccountResponse::build(account, Vec::new())),
    ))
}

pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Result<(StatusCode, CookieJar, Json<LoginResponse>), ApiError> {
    let Json(body) = body?;
    let (session, raw_token) = state.auth.login(body.into()).await?;

    // httpOnly + SameSite=Lax + Secure;
    // desktop/other clients instead read `token` from the body below.
    let cookie = Cookie::build((SESSION_COOKIE_NAME, raw_token.clone()))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(SESSION_COOKIE_MAX_AGE_DAYS))
        .build();
    let jar = jar.add(cookie);

    let response = LoginResponse {
        session: session.into(),
        token: raw_token,
    };

    Ok((StatusCode::CREATED, jar, Json(response)))
}

/// Someone else's profile, filtered by `decide_profile_visibility` before
/// the response is built. Requires authentication like every other route in
/// this API except register/login.
pub async fn get_account(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(account_id): Path<Uuid>,
    query: Result<Query<ProfileQuery>, QueryRejection>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let Query(query) = query?;
    let ctx = state
        .domain
        .get_profile_context(context.account_id, account_id, query.server_id)
        .await?;

    Ok(Json(
        build_profile_response(&state, ctx, query.server_id).await,
    ))
}

/// The same profile as `get_account`, addressed by username so a client can
/// resolve a `/users/:username` URL without knowing the account id.
pub async fn get_account_by_username(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(username): Path<String>,
    query: Result<Query<ProfileQuery>, QueryRejection>,
) -> Result<Json<ProfileResponse>, ApiError> {
    let Query(query) = query?;
    let ctx = state
        .domain
        .get_profile_context_by_username(context.account_id, &username, query.server_id)
        .await?;

    Ok(Json(build_profile_response(&state, ctx, query.server_id).await))
}

/// Applies the visibility decision and resolves presence for one profile.
/// Shared by the id and username routes so neither can drift from the other.
async fn build_profile_response(
    state: &AppState,
    ctx: domain::ProfileContext,
    server_id: Option<Uuid>,
) -> ProfileResponse {
    let account_id = ctx.profile.id;
    let decision = domain::decide_profile_visibility(ctx.visibility_input());

    // The folded exposure: a hidden presence never reaches the hub lookup.
    let online = if decision.presence_for_status(&ctx.profile.status)
        == domain::ProfilePresenceExposure::Real
    {
        state
            .realtime
            .presence_snapshot(&[account_id])
            .await
            .get(&account_id)
            .copied()
            == Some(realtime::PresenceStatus::Online)
    } else {
        false
    };

    ProfileResponse::build(ctx, decision, online, server_id)
}

/// Hydrates many profiles at once for member lists and message authors.
/// Reduced shape: no field here is gated by a visibility setting, so this
/// never becomes a way to read what `get_account` would have withheld.
pub async fn get_accounts_bulk(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    body: Result<Json<BulkProfilesRequest>, JsonRejection>,
) -> Result<Json<ProfileSummaryListResponse>, ApiError> {
    let Json(body) = body?;
    if body.ids.len() > MAX_BULK_PROFILE_IDS {
        return Err(ApiError::from(domain::DomainError::Validation(format!(
            "at most {MAX_BULK_PROFILE_IDS} ids may be requested at once"
        ))));
    }

    let contexts = state
        .domain
        .get_profile_contexts_bulk(context.account_id, &body.ids)
        .await?;

    let decided: Vec<(domain::ProfileContext, domain::ProfileVisibilityDecision)> = contexts
        .into_iter()
        .map(|ctx| {
            let decision = domain::decide_profile_visibility(ctx.visibility_input());
            (ctx, decision)
        })
        .collect();

    // One registry read for the whole page, and only for the accounts whose
    // presence is actually exposed — the rest are offline by decision.
    let exposed: Vec<Uuid> = decided
        .iter()
        .filter(|(ctx, decision)| {
            decision.presence_for_status(&ctx.profile.status)
                == domain::ProfilePresenceExposure::Real
        })
        .map(|(ctx, _)| ctx.profile.id)
        .collect();
    let presence = state.realtime.presence_snapshot(&exposed).await;

    Ok(Json(ProfileSummaryListResponse {
        items: decided
            .into_iter()
            .map(|(ctx, decision)| {
                let online = presence.get(&ctx.profile.id).copied()
                    == Some(realtime::PresenceStatus::Online);
                ProfileSummaryResponse::build(ctx, decision, online)
            })
            .collect(),
    }))
}

/// The caller's own profile — self shape, includes email. This is the only
/// way a fresh client learns its own account id/profile after logging in,
/// since the login response carries only session+token.
pub async fn get_own_account(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<Json<AccountResponse>, ApiError> {
    let account = state.auth.get_account(context.account_id).await?;
    let links = state.auth.list_profile_links(context.account_id).await?;
    Ok(Json(AccountResponse::build(account, links)))
}

/// Always updates the CALLER's own account — never takes an `id` path
/// param, which closes off any IDOR surface on this endpoint entirely.
pub async fn update_own_account(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    body: Result<Json<UpdateAccountRequest>, JsonRejection>,
) -> Result<Json<AccountResponse>, ApiError> {
    let Json(body) = body?;
    let account = state
        .auth
        .update_account(context.account_id, body.into())
        .await?;
    let links = state.auth.list_profile_links(context.account_id).await?;
    Ok(Json(AccountResponse::build(account, links)))
}

pub async fn list_sessions(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<Json<SessionListResponse>, ApiError> {
    let sessions = state.auth.list_sessions(context.account_id).await?;

    Ok(Json(SessionListResponse {
        items: sessions.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

pub async fn revoke_session(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(session_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .auth
        .revoke_session(context.account_id, session_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn revoke_all_sessions(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<StatusCode, ApiError> {
    state.auth.revoke_all_sessions(context.account_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn create_server(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    body: Result<Json<CreateServerRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ServerResponse>), ApiError> {
    let Json(body) = body?;
    let server = state
        .domain
        .create_server(context.account_id, body.into())
        .await?;
    Ok((StatusCode::CREATED, Json(server.into())))
}

pub async fn list_servers(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<Json<ServerListResponse>, ApiError> {
    let servers = state.domain.list_servers(context.account_id).await?;

    Ok(Json(ServerListResponse {
        items: servers.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

/// Membership-gated in `domain::DomainService::get_server`: a non-member
/// gets the same `server_not_found` 404 as a genuinely nonexistent id —
/// this endpoint does not reveal existence.
pub async fn get_server(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<ServerResponse>, ApiError> {
    let server = state
        .domain
        .get_server(context.account_id, server_id)
        .await?;
    Ok(Json(server.into()))
}

pub async fn create_channel(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
    body: Result<Json<CreateChannelRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ChannelResponse>), ApiError> {
    let Json(body) = body?;
    let channel = state
        .domain
        .create_channel(context.account_id, server_id, body.into())
        .await?;
    Ok((StatusCode::CREATED, Json(channel.into())))
}

pub async fn list_channels(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<ChannelListResponse>, ApiError> {
    let channels = state
        .domain
        .list_channels(context.account_id, server_id)
        .await?;

    Ok(Json(ChannelListResponse {
        items: channels.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

/// Full-text search across a server's messages, gated at server
/// membership like `list_members`/`list_channels`.
pub async fn search_messages(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
    query: Result<Query<MessageSearchQuery>, QueryRejection>,
) -> Result<Json<MessageListResponse>, ApiError> {
    let Query(query) = query?;
    let messages = state
        .domain
        .search_messages(context.account_id, server_id, query.into())
        .await?;

    Ok(Json(MessageListResponse {
        items: messages.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

/// `MANAGE_VISIBILITY`-gated, and rejects an override that ranks
/// broader than the server's own visibility (mapped to a 400 by
/// `ApiError`'s existing `DomainError::Validation` arm).
pub async fn update_channel_visibility(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, channel_id)): Path<(Uuid, Uuid)>,
    body: Result<Json<UpdateChannelVisibilityRequest>, JsonRejection>,
) -> Result<Json<ChannelResponse>, ApiError> {
    let Json(body) = body?;
    let channel = state
        .domain
        .update_channel_visibility(context.account_id, server_id, channel_id, body.visibility)
        .await?;
    Ok(Json(channel.into()))
}

// ---- channel permission overrides ----

pub async fn update_channel_restricted(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, channel_id)): Path<(Uuid, Uuid)>,
    body: Result<Json<UpdateChannelRestrictedRequest>, JsonRejection>,
) -> Result<Json<ChannelResponse>, ApiError> {
    let Json(body) = body?;
    let channel = state
        .domain
        .update_channel_restricted(context.account_id, server_id, channel_id, body.restricted)
        .await?;
    Ok(Json(channel.into()))
}

pub async fn set_channel_role_permission(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, channel_id, role_id)): Path<(Uuid, Uuid, Uuid)>,
    body: Result<Json<SetChannelRolePermissionRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = body?;
    state
        .domain
        .set_channel_role_permission(context.account_id, server_id, channel_id, role_id, body.permissions)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_channel_role_permissions(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<ChannelRolePermissionListResponse>, ApiError> {
    let grants = state
        .domain
        .list_channel_role_permissions(context.account_id, channel_id)
        .await?;

    Ok(Json(ChannelRolePermissionListResponse {
        items: grants
            .into_iter()
            .map(|(role_id, permissions)| ChannelRolePermissionResponse { role_id, permissions })
            .collect(),
    }))
}

/// Owner-only (checked in the service layer directly, same tier
/// as `delete_server`), no permission bit.
pub async fn update_server_visibility(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
    body: Result<Json<UpdateServerVisibilityRequest>, JsonRejection>,
) -> Result<Json<ServerResponse>, ApiError> {
    let Json(body) = body?;
    let server = state
        .domain
        .update_server_visibility(context.account_id, server_id, body.visibility)
        .await?;
    Ok(Json(server.into()))
}

/// The account-discovery path: who else is in a server you're in, so a
/// client can start a DM or friend request without the other person having
/// to hand over a raw UUID. Membership-gated in the service layer.
pub async fn list_members(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<ServerMemberListResponse>, ApiError> {
    let members = state
        .domain
        .list_members(context.account_id, server_id)
        .await?;

    // Presence is live socket state, not a column, so it is read from the
    // hub here rather than joined in the query — one registry read for the
    // whole page.
    let account_ids: Vec<Uuid> = members.iter().map(|member| member.account_id).collect();
    let presence = state.realtime.presence_snapshot(&account_ids).await;

    // A member the viewer has blocked reports Offline regardless of their
    // real status — "a block hides the blocked user's social presence from
    // the blocker". Directional: this masks what `context` sees, not what
    // anyone else sees of `context`.
    let blocked: std::collections::HashSet<Uuid> = state
        .domain
        .blocked_account_ids(context.account_id)
        .await?
        .into_iter()
        .collect();

    Ok(Json(ServerMemberListResponse {
        items: members
            .into_iter()
            .map(|member| {
                let status = if blocked.contains(&member.account_id) {
                    realtime::PresenceStatus::Offline
                } else {
                    presence
                        .get(&member.account_id)
                        .copied()
                        .unwrap_or(realtime::PresenceStatus::Offline)
                };
                ServerMemberResponse::new(member, status)
            })
            .collect(),
        next_cursor: None,
    }))
}

pub async fn join_via_invite(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(invite_code): Path<String>,
) -> Result<(StatusCode, Json<ServerResponse>), ApiError> {
    let server = state
        .domain
        .join_via_invite(context.account_id, &invite_code)
        .await?;
    Ok((StatusCode::CREATED, Json(server.into())))
}

/// Gets or creates the 1:1 dm with `body.account_id` (ROADMAP slice 6). 201
/// when a new channel was created, 200 when an existing one was found and
/// reused — the one place in this file a POST can return either, since
/// `domain::DomainService::create_dm` is deliberately idempotent.
pub async fn create_dm(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    body: Result<Json<CreateDmRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ChannelResponse>), ApiError> {
    let Json(body) = body?;
    let (channel, created) = state.domain.create_dm(context.account_id, body.account_id).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(channel.into())))
}

pub async fn list_dms(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<Json<ChannelListResponse>, ApiError> {
    let channels = state.domain.list_dms(context.account_id).await?;

    Ok(Json(ChannelListResponse {
        items: channels.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

/// Always creates a new group — not idempotent, unlike `create_dm` (see
/// `domain::DomainService::create_group_dm`).
pub async fn create_group_dm(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    body: Result<Json<CreateGroupDmRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ChannelResponse>), ApiError> {
    let Json(body) = body?;
    let channel = state
        .domain
        .create_group_dm(context.account_id, body.into())
        .await?;
    Ok((StatusCode::CREATED, Json(channel.into())))
}

/// Sends a friend request to `body.account_id`, or accepts one already sent
/// by them (ROADMAP slice 7). 201 when this call changed something, 200 for
/// a no-op — same created/no-op split as `create_dm`.
pub async fn send_friend_request(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    body: Result<Json<SendFriendRequestRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<FriendshipResponse>), ApiError> {
    let Json(body) = body?;
    let (friendship, changed) = state
        .domain
        .send_friend_request(context.account_id, body.account_id)
        .await?;
    let status = if changed {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(friendship.into())))
}

pub async fn list_friendships(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<Json<FriendshipListResponse>, ApiError> {
    let friendships = state.domain.list_friendships(context.account_id).await?;

    Ok(Json(FriendshipListResponse {
        items: friendships.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

/// Declines/cancels a pending request, or unfriends an accepted one — the
/// same operation on this schema's single canonical friendship row (see
/// `domain::DomainService::remove_friendship`).
pub async fn remove_friendship(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(account_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .domain
        .remove_friendship(context.account_id, account_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Blocks `body.account_id` (ROADMAP slice 7). 201 when a new block was
/// created, 200 when the account was already blocked — same created/no-op
/// split as `create_dm`.
pub async fn create_block(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    body: Result<Json<CreateBlockRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<BlockResponse>), ApiError> {
    let Json(body) = body?;
    let (block, created) = state
        .domain
        .block_account(context.account_id, body.account_id)
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(block.into())))
}

pub async fn list_blocks(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
) -> Result<Json<BlockListResponse>, ApiError> {
    let blocks = state.domain.list_blocks(context.account_id).await?;

    Ok(Json(BlockListResponse {
        items: blocks.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

pub async fn remove_block(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(account_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    state
        .domain
        .unblock_account(context.account_id, account_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Authorized exactly like posting a message in `channel_id`
/// (any account with access to it), no new permission bit.
pub async fn create_thread(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(channel_id): Path<Uuid>,
    body: Result<Json<CreateThreadRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ChannelResponse>), ApiError> {
    let Json(body) = body?;
    let thread = state
        .domain
        .create_thread(context.account_id, channel_id, body.into())
        .await?;
    Ok((StatusCode::CREATED, Json(thread.into())))
}

pub async fn list_threads(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<ChannelListResponse>, ApiError> {
    let threads = state
        .domain
        .list_threads(context.account_id, channel_id)
        .await?;

    Ok(Json(ChannelListResponse {
        items: threads.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

pub async fn send_message(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(channel_id): Path<Uuid>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<MessageResponse>), ApiError> {
    let Json(body) = body?;
    let message = state
        .domain
        .send_message(context.account_id, channel_id, body.into())
        .await?;

    // Best-effort: the message is already durably saved. Realtime delivery
    // never fails the HTTP request — there is no
    // server-side event replay/resume buffer, so a client that missed this
    // publish just refetches on reconnect.
    if let Err(err) = state
        .realtime
        .publish_message_create(channel_id, &message)
        .await
    {
        tracing::warn!(error = %err, "failed to publish message.create event");
    }

    Ok((StatusCode::CREATED, Json(message.into())))
}

/// Cursor pagination: `?limit=&before=`,
/// newest-first, `before` pages backward in time.
pub async fn list_messages(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(channel_id): Path<Uuid>,
    query: Result<Query<MessageListQuery>, QueryRejection>,
) -> Result<Json<MessageListResponse>, ApiError> {
    let Query(query) = query?;
    // The exact clamp `domain::DomainService::list_messages` itself applies —
    // reused here (not reinvented) purely to decide whether the page that
    // came back was a full page, for the `next_cursor` calculation below.
    let effective_limit = query
        .limit
        .unwrap_or(domain::DEFAULT_MESSAGE_LIMIT)
        .min(domain::MAX_MESSAGE_LIMIT);

    let messages = state
        .domain
        .list_messages(
            context.account_id,
            channel_id,
            domain::MessagePagination {
                limit: query.limit,
                before: query.before,
            },
        )
        .await?;

    // `next_cursor` is the last item's id only when a full page came back —
    // a short page means there's nothing older left to page to —
    // real cursor semantics, unlike the always-null collections
    // elsewhere in this file.
    let next_cursor = if messages.len() as u32 == effective_limit {
        messages.last().map(|message| message.id.to_string())
    } else {
        None
    };

    Ok(Json(MessageListResponse {
        items: messages.into_iter().map(Into::into).collect(),
        next_cursor,
    }))
}

pub async fn edit_message(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
    body: Result<Json<EditMessageRequest>, JsonRejection>,
) -> Result<Json<MessageResponse>, ApiError> {
    let Json(body) = body?;
    let message = state
        .domain
        .edit_message(context.account_id, channel_id, message_id, body.into())
        .await?;

    if let Err(err) = state
        .realtime
        .publish_message_update(channel_id, &message)
        .await
    {
        tracing::warn!(error = %err, "failed to publish message.update event");
    }

    Ok(Json(message.into()))
}

pub async fn delete_message(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    state
        .domain
        .delete_message(context.account_id, channel_id, message_id)
        .await?;

    if let Err(err) = state
        .realtime
        .publish_message_delete(channel_id, message_id)
        .await
    {
        tracing::warn!(error = %err, "failed to publish message.delete event");
    }

    Ok(StatusCode::NO_CONTENT)
}

// ---- role permissions v2 ----

pub async fn pin_message(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<MessageResponse>, ApiError> {
    let message = state
        .domain
        .pin_message(context.account_id, channel_id, message_id)
        .await?;

    if let Err(err) = state.realtime.publish_message_pin_update(channel_id, &message).await {
        tracing::warn!(error = %err, "failed to publish message.pin_update event");
    }

    Ok(Json(message.into()))
}

pub async fn unpin_message(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((channel_id, message_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<MessageResponse>, ApiError> {
    let message = state
        .domain
        .unpin_message(context.account_id, channel_id, message_id)
        .await?;

    if let Err(err) = state.realtime.publish_message_pin_update(channel_id, &message).await {
        tracing::warn!(error = %err, "failed to publish message.pin_update event");
    }

    Ok(Json(message.into()))
}

pub async fn list_pinned_messages(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(channel_id): Path<Uuid>,
) -> Result<Json<MessageListResponse>, ApiError> {
    let messages = state
        .domain
        .list_pinned_messages(context.account_id, channel_id)
        .await?;

    Ok(Json(MessageListResponse {
        items: messages.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

// ---- M2: roles & permissions ----

pub async fn create_role(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
    body: Result<Json<CreateRoleRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<RoleResponse>), ApiError> {
    let Json(body) = body?;
    let role = state
        .domain
        .create_role(context.account_id, server_id, body.into())
        .await?;

    if let Err(err) = state.realtime.publish_role_create(server_id, &role).await {
        tracing::warn!(error = %err, "failed to publish role.create event");
    }

    Ok((StatusCode::CREATED, Json(role.into())))
}

pub async fn list_roles(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<RoleListResponse>, ApiError> {
    let roles = state.domain.list_roles(context.account_id, server_id).await?;

    Ok(Json(RoleListResponse {
        items: roles.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

pub async fn update_role(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, role_id)): Path<(Uuid, Uuid)>,
    body: Result<Json<UpdateRoleRequest>, JsonRejection>,
) -> Result<Json<RoleResponse>, ApiError> {
    let Json(body) = body?;
    let role = state
        .domain
        .update_role(context.account_id, server_id, role_id, body.into())
        .await?;

    if let Err(err) = state.realtime.publish_role_update(server_id, &role).await {
        tracing::warn!(error = %err, "failed to publish role.update event");
    }

    Ok(Json(role.into()))
}

pub async fn delete_role(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, role_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    state
        .domain
        .delete_role(context.account_id, server_id, role_id)
        .await?;

    if let Err(err) = state.realtime.publish_role_delete(server_id, role_id).await {
        tracing::warn!(error = %err, "failed to publish role.delete event");
    }

    Ok(StatusCode::NO_CONTENT)
}

/// One `role.update` event per reordered role — simpler than inventing a
/// bulk event type for something that happens rarely (a moderator dragging
/// the role list), and every existing client already knows how to apply a
/// `role.update`.
pub async fn reorder_roles(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
    body: Result<Json<ReorderRolesRequest>, JsonRejection>,
) -> Result<Json<RoleListResponse>, ApiError> {
    let Json(body) = body?;
    let roles = state
        .domain
        .reorder_roles(context.account_id, server_id, body.role_ids)
        .await?;

    for role in &roles {
        if let Err(err) = state.realtime.publish_role_update(server_id, role).await {
            tracing::warn!(error = %err, "failed to publish role.update event");
        }
    }

    Ok(Json(RoleListResponse {
        items: roles.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

pub async fn set_member_roles(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, account_id)): Path<(Uuid, Uuid)>,
    body: Result<Json<SetMemberRolesRequest>, JsonRejection>,
) -> Result<Json<MemberRolesResponse>, ApiError> {
    let Json(body) = body?;
    let role_ids = state
        .domain
        .set_member_roles(context.account_id, server_id, account_id, body.role_ids)
        .await?;

    if let Err(err) = state
        .realtime
        .publish_member_roles_update(server_id, account_id, role_ids.clone())
        .await
    {
        tracing::warn!(error = %err, "failed to publish member.roles_update event");
    }

    Ok(Json(MemberRolesResponse { role_ids }))
}

/// Rate-limited (`lib.rs`'s `moderation` router group) — a destructive
/// action, same reasoning as `ban_member`/`delete_server`.
pub async fn kick_member(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, account_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let recipients = state
        .domain
        .kick_member(context.account_id, server_id, account_id)
        .await?;

    state
        .realtime
        .announce_member_leave(&recipients, server_id, account_id, realtime::MemberLeaveReason::Kicked)
        .await;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_bans(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<BanListResponse>, ApiError> {
    let bans = state
        .domain
        .list_server_bans(context.account_id, server_id)
        .await?;

    Ok(Json(BanListResponse {
        items: bans.into_iter().map(Into::into).collect(),
        next_cursor: None,
    }))
}

/// Rate-limited (`lib.rs`'s `moderation` router group).
pub async fn create_ban(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
    body: Result<Json<CreateBanRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<BanResponse>), ApiError> {
    let Json(body) = body?;
    let (recipients, ban) = state
        .domain
        .ban_member(context.account_id, server_id, body.account_id, body.reason)
        .await?;

    state
        .realtime
        .announce_member_leave(
            &recipients,
            server_id,
            body.account_id,
            realtime::MemberLeaveReason::Banned,
        )
        .await;

    Ok((StatusCode::CREATED, Json(ban.into())))
}

pub async fn delete_ban(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, account_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    state
        .domain
        .unban_member(context.account_id, server_id, account_id)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---- role permissions v2 (continued) ----

pub async fn timeout_member(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, account_id)): Path<(Uuid, Uuid)>,
    body: Result<Json<TimeoutRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = body?;
    let until = body.until;
    state
        .domain
        .timeout_member(context.account_id, server_id, account_id, body.into())
        .await?;

    if let Err(err) = state
        .realtime
        .publish_member_timeout_update(server_id, account_id, Some(until))
        .await
    {
        tracing::warn!(error = %err, "failed to publish member.timeout_update event");
    }

    Ok(StatusCode::NO_CONTENT)
}

pub async fn clear_timeout(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, account_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    state
        .domain
        .clear_timeout(context.account_id, server_id, account_id)
        .await?;

    if let Err(err) = state
        .realtime
        .publish_member_timeout_update(server_id, account_id, None)
        .await
    {
        tracing::warn!(error = %err, "failed to publish member.timeout_update event");
    }

    Ok(StatusCode::NO_CONTENT)
}

pub async fn update_member_nickname(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, account_id)): Path<(Uuid, Uuid)>,
    body: Result<Json<UpdateNicknameRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = body?;
    state
        .domain
        .update_member_nickname(context.account_id, server_id, account_id, body.nickname.clone())
        .await?;

    if let Err(err) = state
        .realtime
        .publish_member_nickname_update(server_id, account_id, body.nickname)
        .await
    {
        tracing::warn!(error = %err, "failed to publish member.nickname_update event");
    }

    Ok(StatusCode::NO_CONTENT)
}

/// No realtime event on regeneration — broadcasting the new code
/// server-wide would leak it to members who don't hold `MANAGE_INVITES` (the
/// whole point of gating who can see it). The caller gets the fresh code
/// directly in this response; that's enough for an action this infrequent.
pub async fn regenerate_invite_code(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<Json<ServerResponse>, ApiError> {
    let server = state
        .domain
        .regenerate_invite_code(context.account_id, server_id)
        .await?;

    Ok(Json(server.into()))
}

pub async fn leave_server(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let recipients = state
        .domain
        .leave_server(context.account_id, server_id)
        .await?;

    state
        .realtime
        .announce_member_leave(
            &recipients,
            server_id,
            context.account_id,
            realtime::MemberLeaveReason::Left,
        )
        .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Rate-limited (`lib.rs`'s `moderation` router group) — the most
/// destructive action in this file.
pub async fn delete_server(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    // Resolved BEFORE the delete: afterward, every `membership` row this
    // would read is already gone.
    let recipients = state
        .domain
        .server_member_account_ids(server_id)
        .await?;

    state.domain.delete_server(context.account_id, server_id).await?;

    state
        .realtime
        .announce_server_delete(&recipients, server_id)
        .await;

    Ok(StatusCode::NO_CONTENT)
}

/// Enqueues an export job; a background worker
/// (`DomainService::process_next_export_job`, spawned in `crates/server`)
/// processes it out of band. Rate-limited (`lib.rs`'s `moderation` router
/// group) — same bulk-administrative-action tier as kick/ban/delete-server.
pub async fn request_export(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path(server_id): Path<Uuid>,
) -> Result<(StatusCode, Json<ExportJobResponse>), ApiError> {
    let job = state
        .domain
        .request_export(context.account_id, server_id)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(job.into())))
}

/// Polls one export job's status; `download_url` is `null` until
/// `status == "done"`.
pub async fn get_export_job(
    State(state): State<AppState>,
    AuthenticatedUser(context): AuthenticatedUser,
    Path((server_id, job_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ExportJobResponse>, ApiError> {
    let job = state
        .domain
        .get_export_job(context.account_id, server_id, job_id)
        .await?;
    Ok(Json(job.into()))
}
