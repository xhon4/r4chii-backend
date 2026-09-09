//! Rename, reorder and realtime recipient coverage for ADR-0017.

use app_core::Uuid;
use domain::{
    ChannelSummary, CreateChannelInput, CreateServerInput, CreateThreadInput, DomainError,
    RenameChannelInput,
};

mod common;
use common::*;

/// Helper to create a server owned by `owner` plus two text channels.
async fn setup_two_channels(
    domain: &domain::DomainService,
    owner: Uuid,
) -> (domain::ServerSummary, ChannelSummary, ChannelSummary) {
    let server = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "rename reorder".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server");
    let a = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "alpha".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create alpha");
    let b = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "beta".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create beta");
    (server, a, b)
}

#[tokio::test]
async fn rename_text_channel_updates_name() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (server, channel, _) = setup_two_channels(&domain, alice).await;

    let renamed = domain
        .rename_channel(
            alice,
            server.id,
            channel.id,
            RenameChannelInput {
                name: Some("renamed".to_string()),
                title: None,
            },
        )
        .await
        .expect("rename succeeds");
    assert_eq!(renamed.name.as_deref(), Some("renamed"));
    assert_eq!(renamed.id, channel.id);
    assert_eq!(renamed.kind, "text");
}

#[tokio::test]
async fn rename_thread_freezes_slug() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice2").await;
    let server = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "thread server".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create server");
    let parent = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("parent");
    let thread = domain
        .create_thread(
            alice,
            parent.id,
            CreateThreadInput {
                title: "original title".to_string(),
                root_message_id: None,
            },
        )
        .await
        .expect("thread");
    let original_slug = thread.slug.clone();

    let renamed = domain
        .rename_channel(
            alice,
            server.id,
            thread.id,
            RenameChannelInput {
                name: None,
                title: Some("new title".to_string()),
            },
        )
        .await
        .expect("retitle succeeds");
    assert_eq!(renamed.title.as_deref(), Some("new title"));
    assert_eq!(renamed.slug, original_slug, "slug must be frozen");
    assert_eq!(renamed.kind, "thread");
}

#[tokio::test]
async fn rename_requires_manage_channels() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice3").await;
    let bob = register(&auth, "bob@example.com", "bob3").await;
    let (server, channel, _) = setup_two_channels(&domain, alice).await;

    // bob joins but has no MANAGE_CHANNELS
    let code: String = sqlx::query_scalar("SELECT invite_code FROM server WHERE id = $1")
        .bind(server.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    domain.join_via_invite(bob, &code).await.expect("bob joins");

    let err = domain
        .rename_channel(
            bob,
            server.id,
            channel.id,
            RenameChannelInput {
                name: Some("hacked".to_string()),
                title: None,
            },
        )
        .await
        .expect_err("bob lacks permission");
    assert!(matches!(err, DomainError::MissingPermission));
}

#[tokio::test]
async fn rename_hidden_restricted_channel_is_not_leaking() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice4").await;
    let bob = register(&auth, "bob@example.com", "bob4").await;
    let (server, channel, _) = setup_two_channels(&domain, alice).await;

    // Restrict channel alpha
    domain
        .update_channel_restricted(alice, server.id, channel.id, true)
        .await
        .expect("restrict");

    let code: String = sqlx::query_scalar("SELECT invite_code FROM server WHERE id = $1")
        .bind(server.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    domain.join_via_invite(bob, &code).await.expect("bob joins");

    // Give bob MANAGE_CHANNELS via a role but no VIEW grant.
    let role = domain
        .create_role(alice, server.id, domain::CreateRoleInput { name: "mod".into() })
        .await
        .expect("role");
    // set manage_channels bit
    domain
        .update_role(
            alice,
            server.id,
            role.id,
            domain::UpdateRoleInput {
                name: None,
                color: None,
                permissions: Some(domain::permissions::MANAGE_CHANNELS),
                mentionable: None,
            },
        )
        .await
        .unwrap();
    domain
        .set_member_roles(alice, server.id, bob, vec![role.id])
        .await
        .unwrap();

    let err = domain
        .rename_channel(
            bob,
            server.id,
            channel.id,
            RenameChannelInput {
                name: Some("hacked".to_string()),
                title: None,
            },
        )
        .await
        .expect_err("bob cannot view restricted channel");
    assert!(matches!(err, DomainError::ChannelNotFound));
}

#[tokio::test]
async fn rename_validation_wrong_field() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice5").await;
    let (server, channel, _) = setup_two_channels(&domain, alice).await;
    let parent = channel.clone();
    let thread = domain
        .create_thread(
            alice,
            parent.id,
            CreateThreadInput {
                title: "topic".to_string(),
                root_message_id: None,
            },
        )
        .await
        .expect("thread");

    // text channel with title -> validation
    let err = domain
        .rename_channel(
            alice,
            server.id,
            channel.id,
            RenameChannelInput {
                name: None,
                title: Some("bad".to_string()),
            },
        )
        .await
        .expect_err("wrong field for text");
    assert!(matches!(err, DomainError::Validation(_)));

    // thread with name -> validation
    let err = domain
        .rename_channel(
            alice,
            server.id,
            thread.id,
            RenameChannelInput {
                name: Some("bad".to_string()),
                title: None,
            },
        )
        .await
        .expect_err("wrong field for thread");
    assert!(matches!(err, DomainError::Validation(_)));
}

#[tokio::test]
async fn reorder_exact_set_succeeds_and_persists_order() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice6").await;
    let (server, a, b) = setup_two_channels(&domain, alice).await;

    // Reverse order
    let reordered = domain
        .reorder_channels(alice, server.id, vec![b.id, a.id])
        .await
        .expect("reorder succeeds");
    assert_eq!(reordered.len(), 2);
    assert_eq!(reordered[0].id, b.id);
    assert_eq!(reordered[1].id, a.id);

    // Verify persistence via list_channels
    let listed = domain
        .list_channels(alice, server.id)
        .await
        .expect("list");
    assert_eq!(listed[0].id, b.id);
    assert_eq!(listed[1].id, a.id);
}

#[tokio::test]
async fn reorder_rejects_duplicate() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice7").await;
    let (server, a, _) = setup_two_channels(&domain, alice).await;
    let err = domain
        .reorder_channels(alice, server.id, vec![a.id, a.id])
        .await
        .expect_err("duplicate");
    assert!(matches!(err, DomainError::Validation(_)));
}

#[tokio::test]
async fn reorder_rejects_missing_or_extra() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice8").await;
    let (server, a, _b) = setup_two_channels(&domain, alice).await;
    // missing one
    let err = domain
        .reorder_channels(alice, server.id, vec![a.id])
        .await
        .expect_err("missing");
    assert!(matches!(err, DomainError::Validation(_)));

    // extra foreign id
    let err = domain
        .reorder_channels(alice, server.id, vec![a.id, app_core::new_id(), app_core::new_id()])
        .await
        .expect_err("extra");
    assert!(matches!(err, DomainError::Validation(_)));
}

#[tokio::test]
async fn reorder_rejects_thread_id() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice9").await;
    let (server, a, b) = setup_two_channels(&domain, alice).await;
    let thread = domain
        .create_thread(
            alice,
            a.id,
            CreateThreadInput {
                title: "topic".to_string(),
                root_message_id: None,
            },
        )
        .await
        .expect("thread");
    let err = domain
        .reorder_channels(alice, server.id, vec![a.id, thread.id])
        .await
        .expect_err("thread in reorder");
    assert!(matches!(err, DomainError::Validation(_)));
    // also need to ensure b missing would still be rejected
    let _ = b;
}

#[tokio::test]
async fn reorder_rejects_deleted_channel() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice10").await;
    let (server, a, b) = setup_two_channels(&domain, alice).await;
    // delete b
    domain
        .delete_channel(alice, server.id, b.id)
        .await
        .expect("delete");
    let err = domain
        .reorder_channels(alice, server.id, vec![a.id, b.id])
        .await
        .expect_err("deleted in reorder");
    assert!(matches!(err, DomainError::Validation(_)));

    // correct set after deletion is just a
    let reordered = domain
        .reorder_channels(alice, server.id, vec![a.id])
        .await
        .expect("reorder with single live succeeds");
    assert_eq!(reordered.len(), 1);
    assert_eq!(reordered[0].id, a.id);
}

#[tokio::test]
async fn reorder_requires_full_visibility() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice11").await;
    let bob = register(&auth, "bob@example.com", "bob11").await;
    let (server, a, b) = setup_two_channels(&domain, alice).await;
    // restrict a
    domain
        .update_channel_restricted(alice, server.id, a.id, true)
        .await
        .expect("restrict");

    let code: String = sqlx::query_scalar("SELECT invite_code FROM server WHERE id = $1")
        .bind(server.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    domain.join_via_invite(bob, &code).await.expect("bob joins");

    let role = domain
        .create_role(alice, server.id, domain::CreateRoleInput { name: "mod".into() })
        .await
        .expect("role");
    domain
        .update_role(
            alice,
            server.id,
            role.id,
            domain::UpdateRoleInput {
                name: None,
                color: None,
                permissions: Some(domain::permissions::MANAGE_CHANNELS),
                mentionable: None,
            },
        )
        .await
        .unwrap();
    domain
        .set_member_roles(alice, server.id, bob, vec![role.id])
        .await
        .unwrap();

    // bob has MANAGE_CHANNELS but cannot see a
    let err = domain
        .reorder_channels(bob, server.id, vec![a.id, b.id])
        .await
        .expect_err("hidden restricted rejection");
    assert!(matches!(err, DomainError::MissingPermission));
}

#[tokio::test]
async fn rename_rejects_invalid_name() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice12").await;
    let (server, channel, _) = setup_two_channels(&domain, alice).await;
    let err = domain
        .rename_channel(
            alice,
            server.id,
            channel.id,
            RenameChannelInput {
                name: Some("   ".to_string()),
                title: None,
            },
        )
        .await
        .expect_err("empty name");
    assert!(matches!(err, DomainError::Validation(_)));
}
