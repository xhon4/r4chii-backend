//! Channel visibility restriction — `channel.restricted` +
//! `channel_role_permission`. The case that matters most: a restricted
//! channel inside an otherwise-public server must never leak into
//! the public read path or the sitemap, and must never surface in search for
//! a member without a grant. Same harness as `visibility_service.rs`.

use app_core::Uuid;
use chrono::{Duration, Utc};
use domain::{
    channel_permissions, permissions, ChannelSummary, CreateChannelInput, CreateRoleInput,
    CreateServerInput, CreateThreadInput, DomainError, DomainService, ReadAccess, SearchInput,
    SendMessageInput, ServerSummary, TimeoutInput, UpdateRoleInput,
};

mod common;
use common::*;

async fn create_server(domain: &DomainService, owner: Uuid, name: &str, visibility: Option<&str>) -> ServerSummary {
    domain
        .create_server(
            owner,
            CreateServerInput { name: name.to_string(), visibility: visibility.map(str::to_string) },
        )
        .await
        .expect("create_server succeeds")
}

async fn create_channel(domain: &DomainService, owner: Uuid, server_id: Uuid, name: &str) -> ChannelSummary {
    domain
        .create_channel(
            owner,
            server_id,
            CreateChannelInput { name: name.to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds")
}

async fn join(domain: &DomainService, account: Uuid, invite_code: &str) {
    domain
        .join_via_invite(account, invite_code)
        .await
        .expect("join_via_invite succeeds");
}

async fn grant_channel_view(
    domain: &DomainService,
    owner: Uuid,
    server_id: Uuid,
    channel_id: Uuid,
    target: Uuid,
    role_name: &str,
) {
    let role = domain
        .create_role(owner, server_id, CreateRoleInput { name: role_name.to_string() })
        .await
        .expect("create_role succeeds");
    domain
        .set_member_roles(owner, server_id, target, vec![role.id])
        .await
        .expect("set_member_roles succeeds");
    domain
        .set_channel_role_permission(owner, server_id, channel_id, role.id, channel_permissions::VIEW_CHANNEL)
        .await
        .expect("set_channel_role_permission succeeds");
}

// ---- baseline: unrestricted is unaffected ----

#[tokio::test]
async fn channel_role_permission_setter_discards_unknown_bits() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let owner = register(&auth, "owner_bits@example.com", "owner_bits").await;
    let server = create_server(&domain, owner, "Permission Bits", None).await;
    let channel = create_channel(&domain, owner, server.id, "staff-only").await;
    let role = domain
        .create_role(
            owner,
            server.id,
            CreateRoleInput {
                name: "Staff".to_string(),
            },
        )
        .await
        .expect("create_role succeeds");

    domain
        .set_channel_role_permission(
            owner,
            server.id,
            channel.id,
            role.id,
            channel_permissions::VIEW_CHANNEL | 2 | 4 | (1 << 12),
        )
        .await
        .expect("set_channel_role_permission succeeds");

    let stored: i64 = sqlx::query_scalar(
        "SELECT permissions FROM channel_role_permission WHERE channel_id = $1 AND role_id = $2",
    )
    .bind(channel.id)
    .bind(role.id)
    .fetch_one(&pool)
    .await
    .expect("permission row exists");
    assert_eq!(stored, channel_permissions::VIEW_CHANNEL);

    domain
        .set_channel_role_permission(owner, server.id, channel.id, role.id, 2 | 4)
        .await
        .expect("set_channel_role_permission succeeds");
    let stored: i64 = sqlx::query_scalar(
        "SELECT permissions FROM channel_role_permission WHERE channel_id = $1 AND role_id = $2",
    )
    .bind(channel.id)
    .bind(role.id)
    .fetch_one(&pool)
    .await
    .expect("permission row exists");
    assert_eq!(stored, 0);
}

#[tokio::test]
async fn canonicalize_channel_permissions_migration_preserves_rows_and_view_bits() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let owner = register(&auth, "owner_legacy_bits@example.com", "owner_legacy_bits").await;
    let server = create_server(&domain, owner, "Legacy Permission Bits", None).await;
    let channel = create_channel(&domain, owner, server.id, "staff-only").await;
    let mut expected = Vec::new();

    for (name, permissions) in [
        (
            "View and retired",
            channel_permissions::VIEW_CHANNEL | 2 | 4,
        ),
        ("Retired only", 2 | 4),
        ("View only", channel_permissions::VIEW_CHANNEL),
    ] {
        let role = domain
            .create_role(
                owner,
                server.id,
                CreateRoleInput {
                    name: name.to_string(),
                },
            )
            .await
            .expect("create_role succeeds");
        sqlx::query(
            "INSERT INTO channel_role_permission (channel_id, role_id, permissions) VALUES ($1, $2, $3)",
        )
        .bind(channel.id)
        .bind(role.id)
        .bind(permissions)
        .execute(&pool)
        .await
        .expect("historical permission row inserts");
        expected.push((role.id, permissions & channel_permissions::VIEW_CHANNEL));
    }

    sqlx::query(include_str!(
        "../../../migrations/0021_canonicalize_channel_permissions.sql"
    ))
    .execute(&pool)
    .await
    .expect("canonicalization migration succeeds");

    for (role_id, expected_permissions) in expected {
        let stored: i64 = sqlx::query_scalar(
            "SELECT permissions FROM channel_role_permission WHERE channel_id = $1 AND role_id = $2",
        )
        .bind(channel.id)
        .bind(role_id)
        .fetch_one(&pool)
        .await
        .expect("historical row remains");
        assert_eq!(stored, expected_permissions);
    }
}

// ---- baseline: unrestricted is unaffected ----

#[tokio::test]
async fn an_unrestricted_channel_is_visible_to_every_member_as_before() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner1@example.com", "owner1").await;
    let plain = register(&auth, "plain1@example.com", "plain1").await;

    let server = create_server(&domain, owner, "Server1", None).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id, "general").await;

    let channels = domain.list_channels(plain, server.id).await.expect("list_channels succeeds");
    assert!(channels.iter().any(|c| c.id == channel.id));

    let send = domain
        .send_message(plain, channel.id, SendMessageInput { content: "hi".to_string() })
        .await;
    assert!(send.is_ok());
}

// ---- restricted: invisible without a grant, visible with one ----

#[tokio::test]
async fn a_restricted_channel_is_absent_from_list_channels_without_a_grant() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner2@example.com", "owner2").await;
    let plain = register(&auth, "plain2@example.com", "plain2").await;
    let staff = register(&auth, "staff2@example.com", "staff2").await;

    let server = create_server(&domain, owner, "Server2", None).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    join(&domain, staff, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id, "staff-only").await;

    domain
        .update_channel_restricted(owner, server.id, channel.id, true)
        .await
        .expect("update_channel_restricted succeeds");
    grant_channel_view(&domain, owner, server.id, channel.id, staff, "Staff").await;

    let plain_channels = domain.list_channels(plain, server.id).await.expect("list_channels succeeds");
    assert!(!plain_channels.iter().any(|c| c.id == channel.id));

    let staff_channels = domain.list_channels(staff, server.id).await.expect("list_channels succeeds");
    assert!(staff_channels.iter().any(|c| c.id == channel.id));

    let owner_channels = domain.list_channels(owner, server.id).await.expect("list_channels succeeds");
    assert!(owner_channels.iter().any(|c| c.id == channel.id), "the owner always sees it");
}

#[tokio::test]
async fn a_plain_member_cannot_send_or_read_in_a_restricted_channel_without_a_grant() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner3@example.com", "owner3").await;
    let plain = register(&auth, "plain3@example.com", "plain3").await;

    let server = create_server(&domain, owner, "Server3", None).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id, "staff-only").await;
    domain
        .update_channel_restricted(owner, server.id, channel.id, true)
        .await
        .expect("update_channel_restricted succeeds");

    let send = domain
        .send_message(plain, channel.id, SendMessageInput { content: "hi".to_string() })
        .await;
    assert!(matches!(send, Err(DomainError::ChannelNotFound)));

    let list = domain.list_messages(plain, channel.id, Default::default()).await;
    assert!(matches!(list, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn a_thread_inherits_its_parent_channels_restriction_not_its_own() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner4@example.com", "owner4").await;
    let plain = register(&auth, "plain4@example.com", "plain4").await;
    let staff = register(&auth, "staff4@example.com", "staff4").await;

    let server = create_server(&domain, owner, "Server4", None).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    join(&domain, staff, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id, "staff-only").await;

    let thread = domain
        .create_thread(
            owner,
            channel.id,
            CreateThreadInput { title: "planning".to_string(), root_message_id: None },
        )
        .await
        .expect("create_thread succeeds");

    domain
        .update_channel_restricted(owner, server.id, channel.id, true)
        .await
        .expect("update_channel_restricted succeeds");
    grant_channel_view(&domain, owner, server.id, channel.id, staff, "Staff").await;

    // Plain cannot even reach the thread, despite it never having its own
    // `restricted` flag flipped.
    let plain_denied = domain.list_threads(plain, channel.id).await;
    assert!(matches!(plain_denied, Err(DomainError::ChannelNotFound)));

    // Staff, granted on the PARENT channel, can.
    let staff_threads = domain.list_threads(staff, channel.id).await.expect("list_threads succeeds");
    assert!(staff_threads.iter().any(|t| t.id == thread.id));
}

// ---- the critical case: never leaks through the public surfaces ----

#[tokio::test]
async fn a_restricted_channel_in_a_public_server_never_appears_in_the_sitemap_or_public_read_path() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner5@example.com", "owner5").await;

    let server = create_server(&domain, owner, "Public Server5", Some("public")).await;
    let channel = create_channel(&domain, owner, server.id, "staff-only").await;
    let thread = domain
        .create_thread(
            owner,
            channel.id,
            CreateThreadInput { title: "secret planning".to_string(), root_message_id: None },
        )
        .await
        .expect("create_thread succeeds");

    // Sanity: BEFORE restricting, the thread is genuinely public.
    let before = domain.resolve_read_access(None, thread.id).await.expect("resolves");
    assert_eq!(before, ReadAccess::Public);
    let sitemap_before = domain.list_public_threads().await.expect("list_public_threads succeeds");
    assert!(sitemap_before.iter().any(|t| t.id == thread.id));

    domain
        .update_channel_restricted(owner, server.id, channel.id, true)
        .await
        .expect("update_channel_restricted succeeds");

    // An anonymous caller must never resolve this, regardless of the
    // channel/thread's own `visibility` still being effectively public.
    let anonymous = domain.resolve_read_access(None, thread.id).await;
    assert!(
        matches!(anonymous, Err(DomainError::ChannelNotFound)),
        "a restricted channel must never be publicly readable, even inside a public server"
    );

    // And it must be gone from the sitemap that drives crawler discovery.
    let sitemap_after = domain.list_public_threads().await.expect("list_public_threads succeeds");
    assert!(
        !sitemap_after.iter().any(|t| t.id == thread.id),
        "a restricted channel's threads must never appear in the public sitemap"
    );

    // An authenticated member without a grant is ALSO denied — restriction
    // is stricter than plain membership, not just stricter than anonymity.
    let plain = register(&auth, "plain5@example.com", "plain5").await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    let member_denied = domain.resolve_read_access(Some(plain), thread.id).await;
    assert!(matches!(member_denied, Err(DomainError::ChannelNotFound)));

    // A granted member gets ReadAccess::Member (not Public — a real grant,
    // not just an accident of the channel's own visibility field).
    let staff = register(&auth, "staff5@example.com", "staff5").await;
    join(&domain, staff, server.invite_code.as_ref().unwrap()).await;
    grant_channel_view(&domain, owner, server.id, channel.id, staff, "Staff").await;
    let staff_access = domain.resolve_read_access(Some(staff), thread.id).await.expect("resolves");
    assert_eq!(staff_access, ReadAccess::Member);
}

// ---- search must not leak a restricted channel's messages either ----

#[tokio::test]
async fn search_excludes_a_restricted_channels_messages_for_a_member_without_a_grant() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner6@example.com", "owner6").await;
    let plain = register(&auth, "plain6@example.com", "plain6").await;
    let staff = register(&auth, "staff6@example.com", "staff6").await;

    let server = create_server(&domain, owner, "Server6", None).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    join(&domain, staff, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id, "staff-only").await;

    domain
        .send_message(owner, channel.id, SendMessageInput { content: "unique-classified-plan".to_string() })
        .await
        .expect("send_message succeeds");

    domain
        .update_channel_restricted(owner, server.id, channel.id, true)
        .await
        .expect("update_channel_restricted succeeds");
    grant_channel_view(&domain, owner, server.id, channel.id, staff, "Staff").await;

    let plain_results = domain
        .search_messages(plain, server.id, SearchInput { query: "unique-classified-plan".to_string(), ..Default::default() })
        .await
        .expect("search_messages succeeds");
    assert!(
        plain_results.is_empty(),
        "a plain member must never find a restricted channel's messages via search"
    );

    let staff_results = domain
        .search_messages(staff, server.id, SearchInput { query: "unique-classified-plan".to_string(), ..Default::default() })
        .await
        .expect("search_messages succeeds");
    assert_eq!(staff_results.len(), 1, "a granted member's search still finds it");

    let owner_results = domain
        .search_messages(owner, server.id, SearchInput { query: "unique-classified-plan".to_string(), ..Default::default() })
        .await
        .expect("search_messages succeeds");
    assert_eq!(owner_results.len(), 1, "the owner bypasses the restriction");
}

// ---- permission checks around the admin actions themselves ----

#[tokio::test]
async fn only_manage_channels_can_flip_restricted_or_grant_a_role() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner7@example.com", "owner7").await;
    let plain = register(&auth, "plain7@example.com", "plain7").await;

    let server = create_server(&domain, owner, "Server7", None).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id, "general").await;

    let denied_restrict = domain.update_channel_restricted(plain, server.id, channel.id, true).await;
    assert!(matches!(denied_restrict, Err(DomainError::MissingPermission)));

    let role = domain
        .create_role(owner, server.id, CreateRoleInput { name: "Staff".to_string() })
        .await
        .expect("create_role succeeds");
    let denied_grant = domain
        .set_channel_role_permission(plain, server.id, channel.id, role.id, channel_permissions::VIEW_CHANNEL)
        .await;
    assert!(matches!(denied_grant, Err(DomainError::MissingPermission)));

    // Granting MANAGE_CHANNELS (M2's existing bit) is enough — no new bit
    // was needed for this ADR's admin actions.
    let manage_channels_role = domain
        .create_role(owner, server.id, CreateRoleInput { name: "Channel Admin".to_string() })
        .await
        .expect("create_role succeeds");
    domain
        .update_role(
            owner,
            server.id,
            manage_channels_role.id,
            UpdateRoleInput {
                name: None,
                color: None,
                permissions: Some(permissions::MANAGE_CHANNELS),
                mentionable: None,
            },
        )
        .await
        .expect("update_role succeeds");
    domain
        .set_member_roles(owner, server.id, plain, vec![manage_channels_role.id])
        .await
        .expect("set_member_roles succeeds");

    domain
        .update_channel_restricted(plain, server.id, channel.id, true)
        .await
        .expect("MANAGE_CHANNELS holder may flip restricted");
    domain
        .set_channel_role_permission(plain, server.id, channel.id, role.id, channel_permissions::VIEW_CHANNEL)
        .await
        .expect("MANAGE_CHANNELS holder may grant a role");
}

#[tokio::test]
async fn a_thread_cannot_be_restricted_or_granted_directly() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner8@example.com", "owner8").await;

    let server = create_server(&domain, owner, "Server8", None).await;
    let channel = create_channel(&domain, owner, server.id, "general").await;
    let thread = domain
        .create_thread(owner, channel.id, CreateThreadInput { title: "topic".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    let restrict_result = domain.update_channel_restricted(owner, server.id, thread.id, true).await;
    assert!(matches!(restrict_result, Err(DomainError::Validation(_))));

    let role = domain
        .create_role(owner, server.id, CreateRoleInput { name: "Staff".to_string() })
        .await
        .expect("create_role succeeds");
    let grant_result = domain
        .set_channel_role_permission(owner, server.id, thread.id, role.id, channel_permissions::VIEW_CHANNEL)
        .await;
    assert!(matches!(grant_result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn can_join_voice_respects_permissions_restrictions_and_channel_kind() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner_v@example.com", "owner_v").await;
    let alice = register(&auth, "alice_v@example.com", "alice_v").await;
    let bob = register(&auth, "bob_v@example.com", "bob_v").await;
    let non_member = register(&auth, "outsider@example.com", "outsider").await;

    let server = create_server(&domain, owner, "Voice Server", None).await;
    let invite = server.invite_code.expect("invite code exists");
    join(&domain, alice, &invite).await;
    join(&domain, bob, &invite).await;

    // 1. Text channel -> cannot join voice even as member/owner
    let text_channel = create_channel(&domain, owner, server.id, "general-text").await;
    assert!(!domain.can_join_voice(owner, text_channel.id).await.unwrap());
    assert!(!domain.can_join_voice(alice, text_channel.id).await.unwrap());

    // 2. Open voice channel -> owner and members can join, non-member cannot
    let voice_channel = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "General Voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("voice channel creates");

    assert!(domain.can_join_voice(owner, voice_channel.id).await.unwrap());
    assert!(domain.can_join_voice(alice, voice_channel.id).await.unwrap());
    assert!(!domain.can_join_voice(non_member, voice_channel.id).await.unwrap());

    // 3. Restricted voice channel -> only owner, admin, or role with VIEW_CHANNEL can join
    let secret_voice = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "Secret Voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("secret voice channel creates");

    domain
        .update_channel_restricted(owner, server.id, secret_voice.id, true)
        .await
        .expect("restricting channel succeeds");

    // Owner can join (owner bypass)
    assert!(domain.can_join_voice(owner, secret_voice.id).await.unwrap());
    // Alice and Bob cannot join without grant
    assert!(!domain.can_join_voice(alice, secret_voice.id).await.unwrap());
    assert!(!domain.can_join_voice(bob, secret_voice.id).await.unwrap());

    // Grant VIEW_CHANNEL to Alice via Staff role
    grant_channel_view(&domain, owner, server.id, secret_voice.id, alice, "Staff").await;
    assert!(domain.can_join_voice(alice, secret_voice.id).await.unwrap());
    assert!(!domain.can_join_voice(bob, secret_voice.id).await.unwrap());

    // 4. Timed-out member cannot join even if permitted
    domain
        .timeout_member(
            owner,
            server.id,
            alice,
            TimeoutInput {
                until: Utc::now() + Duration::hours(1),
                reason: None,
            },
        )
        .await
        .expect("timeout succeeds");
    assert!(!domain.can_join_voice(alice, secret_voice.id).await.unwrap());
    assert!(!domain.can_join_voice(alice, voice_channel.id).await.unwrap());
}

