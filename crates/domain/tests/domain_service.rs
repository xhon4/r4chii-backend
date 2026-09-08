use domain::{CreateChannelInput, CreateServerInput, DomainError};

mod common;
use common::*;

fn create_server_input(name: &str) -> CreateServerInput {
    CreateServerInput {
        name: name.to_string(),
        visibility: None,
    }
}

#[tokio::test]
async fn create_server_makes_the_creator_the_owner_with_a_visible_invite_code() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    assert_eq!(server.owner_account_id, alice);
    assert_eq!(server.name, "Alice's Place");
    assert_eq!(server.visibility, "private");
    assert!(
        server.invite_code.is_some(),
        "the creator (owner) must see the invite code"
    );
    assert_eq!(
        server.invite_code.as_ref().map(|c| c.chars().count()),
        Some(10)
    );
}

#[tokio::test]
async fn create_server_rejects_an_empty_name() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain.create_server(alice, create_server_input("")).await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn create_server_rejects_an_invalid_visibility() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "Alice's Place".to_string(),
                visibility: Some("secret".to_string()),
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn get_server_returns_404_equivalent_for_a_non_member() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let result = domain.get_server(bob, server.id).await;

    assert!(matches!(result, Err(DomainError::ServerNotFound)));
}

#[tokio::test]
async fn get_server_returns_server_not_found_for_a_nonexistent_id() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain.get_server(alice, app_core::new_id()).await;

    assert!(matches!(result, Err(DomainError::ServerNotFound)));
}

#[tokio::test]
async fn get_server_hides_the_invite_code_from_a_plain_member() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees code"))
        .await
        .expect("bob joins");

    let owner_view = domain
        .get_server(alice, server.id)
        .await
        .expect("owner can view");
    let member_view = domain
        .get_server(bob, server.id)
        .await
        .expect("member can view");

    assert!(owner_view.invite_code.is_some());
    assert!(member_view.invite_code.is_none());
}

#[tokio::test]
async fn list_servers_never_includes_a_server_only_another_account_belongs_to() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let bob_servers = domain
        .list_servers(bob)
        .await
        .expect("list_servers succeeds");

    assert!(bob_servers.is_empty());
}

#[tokio::test]
async fn list_servers_includes_a_server_the_caller_joined_without_the_invite_code() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees code"))
        .await
        .expect("bob joins");

    let bob_servers = domain
        .list_servers(bob)
        .await
        .expect("list_servers succeeds");

    assert_eq!(bob_servers.len(), 1);
    assert_eq!(bob_servers[0].id, server.id);
    assert!(bob_servers[0].invite_code.is_none());

    let alice_servers = domain
        .list_servers(alice)
        .await
        .expect("list_servers succeeds");
    assert_eq!(alice_servers.len(), 1);
    assert!(alice_servers[0].invite_code.is_some());
}

#[tokio::test]
async fn create_channel_by_a_non_member_returns_server_not_found() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let result = domain
        .create_channel(
            bob,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::ServerNotFound)));
}

#[tokio::test]
async fn create_channel_rejects_once_the_server_hits_the_channel_cap() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    // Fills the server to exactly `MAX_CHANNELS_PER_SERVER` (500, private to
    // `domain`) via direct inserts — bypassing the service layer for speed,
    // since the cap check itself is the only thing under test here.
    for i in 0..500 {
        db::channel::insert_server_channel(&pool, app_core::new_id(), server.id, "text", &format!("filler-{i}"))
            .await
            .expect("direct channel insert succeeds");
    }

    let result = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "one-too-many".to_string(),
                kind: None,
            },
        )
        .await;
    assert!(matches!(result, Err(DomainError::ChannelLimitReached)));
}

#[tokio::test]
async fn list_channels_by_a_non_member_returns_server_not_found() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let result = domain.list_channels(bob, server.id).await;

    assert!(matches!(result, Err(DomainError::ServerNotFound)));
}

#[tokio::test]
async fn create_channel_then_list_channels_returns_the_expected_fields() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create_channel succeeds");

    assert_eq!(channel.server_id, Some(server.id));
    assert_eq!(channel.kind, "text");
    assert_eq!(channel.name.as_deref(), Some("general"));

    let channels = domain
        .list_channels(alice, server.id)
        .await
        .expect("list_channels succeeds");

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].id, channel.id);
    assert_eq!(channels[0].name.as_deref(), Some("general"));
}

#[tokio::test]
async fn create_channel_rejects_an_empty_name() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let result = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "  ".to_string(),
                kind: None,
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn join_via_invite_with_an_unknown_code_returns_invalid_invite() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let result = domain.join_via_invite(bob, "doesnotexist").await;

    assert!(matches!(result, Err(DomainError::InvalidInvite)));
}

#[tokio::test]
async fn join_via_invite_adds_a_membership_row_for_the_joiner() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let joined = domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees code"))
        .await
        .expect("bob joins");

    assert_eq!(joined.id, server.id);
    assert!(joined.invite_code.is_none());

    let channels = domain
        .list_channels(bob, server.id)
        .await
        .expect("bob is now a member and can list channels");
    assert!(channels.is_empty());
}

#[tokio::test]
async fn join_via_invite_when_already_a_member_returns_conflict_and_does_not_duplicate() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");
    let invite_code = server.invite_code.clone().expect("owner sees code");

    domain
        .join_via_invite(bob, &invite_code)
        .await
        .expect("bob's first join succeeds");

    let result = domain.join_via_invite(bob, &invite_code).await;

    assert!(matches!(result, Err(DomainError::AlreadyMember)));

    // Confirm the app-level check and the DB's UNIQUE constraint both held:
    // exactly one membership row exists for (server, bob), not two.
    let member_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM membership WHERE server_id = $1 AND account_id = $2",
    )
    .bind(server.id)
    .bind(bob)
    .fetch_one(&pool)
    .await
    .expect("count query succeeds");

    assert_eq!(member_count, 1);
}

#[tokio::test]
async fn list_members_returns_every_member_with_their_role() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");
    let invite_code = server.invite_code.clone().expect("owner sees code");

    domain
        .join_via_invite(bob, &invite_code)
        .await
        .expect("bob joins");

    let members = domain
        .list_members(alice, server.id)
        .await
        .expect("list_members succeeds");

    assert_eq!(members.len(), 2);

    let owner = members
        .iter()
        .find(|m| m.account_id == alice)
        .expect("alice is listed");
    assert_eq!(owner.role, "owner");
    assert_eq!(owner.username, "alice");
    assert_eq!(owner.display_name, "Test User");

    let member = members
        .iter()
        .find(|m| m.account_id == bob)
        .expect("bob is listed");
    assert_eq!(member.role, "member");
    assert_eq!(member.username, "bob");
}

#[tokio::test]
async fn list_members_by_a_non_member_returns_server_not_found() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let mallory = register(&auth, "mallory@example.com", "mallory").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let result = domain.list_members(mallory, server.id).await;

    assert!(matches!(result, Err(DomainError::ServerNotFound)));
}

#[tokio::test]
async fn list_members_for_a_nonexistent_server_returns_server_not_found() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain.list_members(alice, app_core::new_id()).await;

    assert!(matches!(result, Err(DomainError::ServerNotFound)));
}

#[tokio::test]
async fn create_channel_defaults_to_a_text_channel() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create_channel succeeds");

    assert_eq!(channel.kind, "text");
    assert_eq!(channel.server_id, Some(server.id));
}

#[tokio::test]
async fn create_channel_accepts_a_voice_kind() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "General Voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("create_channel succeeds");

    assert_eq!(channel.kind, "voice");
    assert_eq!(channel.server_id, Some(server.id));
}

#[tokio::test]
async fn create_channel_rejects_a_kind_that_is_not_text_or_voice() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    // `dm`/`group_dm` are real channel kinds but must never be creatable
    // through the server-channel route — they'd have a NULL server_id.
    for kind in ["dm", "group_dm", "video", ""] {
        let result = domain
            .create_channel(
                alice,
                server.id,
                CreateChannelInput {
                    name: "nope".to_string(),
                    kind: Some(kind.to_string()),
                },
            )
            .await;

        assert!(
            matches!(result, Err(DomainError::Validation(_))),
            "kind {kind:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn a_voice_channel_is_authorized_by_server_membership_like_a_text_channel() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let mallory = register(&auth, "mallory@example.com", "mallory").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");
    let invite_code = server.invite_code.clone().expect("owner sees code");
    domain
        .join_via_invite(bob, &invite_code)
        .await
        .expect("bob joins");

    let voice = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "General Voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("create_channel succeeds");

    // Bob is a server member but holds no `channel_member` row — a voice
    // channel must take the `membership` path, not the dm/group_dm one.
    let authorized = domain
        .authorized_account_ids(voice.id)
        .await
        .expect("authorized_account_ids succeeds");
    assert!(authorized.contains(&alice));
    assert!(authorized.contains(&bob));
    assert!(!authorized.contains(&mallory));

    let accessible = domain
        .accessible_channel_ids(bob)
        .await
        .expect("accessible_channel_ids succeeds");
    assert!(
        accessible.contains(&voice.id),
        "a server member must receive realtime events for a voice channel"
    );

    let outsider = domain
        .accessible_channel_ids(mallory)
        .await
        .expect("accessible_channel_ids succeeds");
    assert!(!outsider.contains(&voice.id));
}

#[tokio::test]
async fn voice_channels_appear_in_list_channels() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place"))
        .await
        .expect("create_server succeeds");

    domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("text channel created");
    domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "General Voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("voice channel created");

    let channels = domain
        .list_channels(alice, server.id)
        .await
        .expect("list_channels succeeds");

    assert_eq!(channels.len(), 2);
    assert!(channels.iter().any(|c| c.kind == "text"));
    assert!(channels.iter().any(|c| c.kind == "voice"));
}
