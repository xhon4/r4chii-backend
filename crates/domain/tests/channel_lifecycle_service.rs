//! Soft channel deletion snapshots public thread tombstones while hiding live content.

use domain::{
    CreateChannelInput, CreateServerInput, CreateThreadInput, DomainError, SendMessageInput,
};

mod common;
use common::*;

#[tokio::test]
async fn deleting_a_public_parent_makes_its_thread_gone_without_exposing_messages() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "Public Place".to_string(),
                visibility: Some("public".to_string()),
            },
        )
        .await
        .expect("create server succeeds");
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
        .expect("create channel succeeds");
    let thread = domain
        .create_thread(
            alice,
            channel.id,
            CreateThreadInput {
                title: "public topic".to_string(),
                root_message_id: None,
            },
        )
        .await
        .expect("create thread succeeds");
    domain
        .send_message(
            alice,
            thread.id,
            SendMessageInput {
                content: "removed content".to_string(),
            },
        )
        .await
        .expect("send message succeeds");

    domain
        .delete_channel(alice, server.id, channel.id)
        .await
        .expect("delete channel succeeds");

    assert!(matches!(
        domain.get_public_thread(thread.id, None).await,
        Err(DomainError::ThreadGone)
    ));
    let snapshot: (bool, Option<bool>) = sqlx::query_as(
        "SELECT deleted_at IS NULL, public_tombstone_eligible FROM channel WHERE id = $1",
    )
    .bind(thread.id)
    .fetch_one(&pool)
    .await
    .expect("thread snapshot is retained");
    assert_eq!(snapshot, (true, Some(true)));
    assert_eq!(
        db::channel::count_by_server(&pool, server.id)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn private_unlisted_and_restricted_threads_stay_not_found_after_deletion() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice2").await;
    let server = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "Public Place".to_string(),
                visibility: Some("public".to_string()),
            },
        )
        .await
        .expect("create server succeeds");
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
        .expect("create channel succeeds");

    for visibility in ["private", "unlisted"] {
        let thread = domain
            .create_thread(
                alice,
                channel.id,
                CreateThreadInput {
                    title: format!("{visibility} topic"),
                    root_message_id: None,
                },
            )
            .await
            .expect("create thread succeeds");
        domain
            .update_channel_visibility(alice, server.id, thread.id, Some(visibility.to_string()))
            .await
            .expect("narrow visibility");
        domain
            .delete_channel(alice, server.id, thread.id)
            .await
            .expect("delete thread succeeds");
        assert!(matches!(
            domain.get_public_thread(thread.id, None).await,
            Err(DomainError::ChannelNotFound)
        ));
    }

    domain
        .update_channel_restricted(alice, server.id, channel.id, true)
        .await
        .expect("restrict channel succeeds");
    let restricted_thread = domain
        .create_thread(
            alice,
            channel.id,
            CreateThreadInput {
                title: "restricted topic".to_string(),
                root_message_id: None,
            },
        )
        .await
        .expect("create thread succeeds");
    domain
        .delete_channel(alice, server.id, restricted_thread.id)
        .await
        .expect("delete restricted thread succeeds");
    assert!(matches!(
        domain.get_public_thread(restricted_thread.id, None).await,
        Err(DomainError::ChannelNotFound)
    ));
}

#[tokio::test]
async fn the_first_thread_snapshot_survives_a_later_parent_deletion() {
    let (domain, auth, pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice3").await;
    let server = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "Public Place".to_string(),
                visibility: Some("public".to_string()),
            },
        )
        .await
        .expect("create server succeeds");
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
        .expect("create channel succeeds");
    let thread = domain
        .create_thread(
            alice,
            channel.id,
            CreateThreadInput {
                title: "public topic".to_string(),
                root_message_id: None,
            },
        )
        .await
        .expect("create thread succeeds");

    domain
        .delete_channel(alice, server.id, thread.id)
        .await
        .expect("direct delete succeeds");
    domain
        .update_server_visibility(alice, server.id, "private".to_string())
        .await
        .expect("server visibility changes later");
    domain
        .delete_channel(alice, server.id, channel.id)
        .await
        .expect("parent delete succeeds");

    let eligible: Option<bool> =
        sqlx::query_scalar("SELECT public_tombstone_eligible FROM channel WHERE id = $1")
            .bind(thread.id)
            .fetch_one(&pool)
            .await
            .expect("thread remains retained");
    assert_eq!(eligible, Some(true));
}
