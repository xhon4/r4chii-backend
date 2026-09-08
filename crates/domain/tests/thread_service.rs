//! Threads as first-class `channel` rows. Same harness as
//! `domain_service.rs`.

use app_core::Uuid;
use domain::{
    CreateChannelInput, CreateServerInput, CreateThreadInput, DomainError, DomainService,
    ReadAccess, SendMessageInput,
};

mod common;
use common::*;

async fn setup_server_and_channel(domain: &DomainService, owner: Uuid) -> (Uuid, Uuid) {
    let server = domain
        .create_server(
            owner,
            CreateServerInput { name: "Alice's Place".to_string(), visibility: None },
        )
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");
    (server.id, channel.id)
}

#[tokio::test]
async fn a_standalone_thread_is_a_channel_row_of_kind_thread() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server_id, channel_id) = setup_server_and_channel(&domain, alice).await;

    let thread = domain
        .create_thread(
            alice,
            channel_id,
            CreateThreadInput { title: "How do I configure X?".to_string(), root_message_id: None },
        )
        .await
        .expect("create_thread succeeds");

    assert_eq!(thread.kind, "thread");
    assert_eq!(thread.parent_channel_id, Some(channel_id));
    assert_eq!(thread.root_message_id, None);
    assert_eq!(thread.title.as_deref(), Some("How do I configure X?"));
    assert!(thread.slug.as_deref().is_some_and(|s| s.starts_with("how-do-i-configure-x-")));
}

#[tokio::test]
async fn a_reply_thread_carries_its_root_message_id() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server_id, channel_id) = setup_server_and_channel(&domain, alice).await;

    let message = domain
        .send_message(alice, channel_id, SendMessageInput { content: "anyone know?".to_string() })
        .await
        .expect("send_message succeeds");

    let thread = domain
        .create_thread(
            alice,
            channel_id,
            CreateThreadInput { title: "re: anyone know?".to_string(), root_message_id: Some(message.id) },
        )
        .await
        .expect("create_thread succeeds");

    assert_eq!(thread.root_message_id, Some(message.id));
}

#[tokio::test]
async fn creating_a_thread_from_a_message_in_a_different_channel_is_rejected() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (server_id, channel_id) = setup_server_and_channel(&domain, alice).await;
    let other_channel = domain
        .create_channel(alice, server_id, CreateChannelInput { name: "off-topic".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");

    let message = domain
        .send_message(alice, other_channel.id, SendMessageInput { content: "hello".to_string() })
        .await
        .expect("send_message succeeds");

    let result = domain
        .create_thread(
            alice,
            channel_id,
            CreateThreadInput { title: "wrong parent".to_string(), root_message_id: Some(message.id) },
        )
        .await;

    assert!(matches!(result, Err(DomainError::MessageNotFound)));
}

#[tokio::test]
async fn a_non_member_cannot_create_a_thread() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (_server_id, channel_id) = setup_server_and_channel(&domain, alice).await;

    let result = domain
        .create_thread(
            bob,
            channel_id,
            CreateThreadInput { title: "intruder".to_string(), root_message_id: None },
        )
        .await;

    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn a_thread_cannot_be_created_inside_another_thread() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server_id, channel_id) = setup_server_and_channel(&domain, alice).await;

    let thread = domain
        .create_thread(
            alice,
            channel_id,
            CreateThreadInput { title: "top level".to_string(), root_message_id: None },
        )
        .await
        .expect("create_thread succeeds");

    let nested = domain
        .create_thread(
            alice,
            thread.id,
            CreateThreadInput { title: "nested".to_string(), root_message_id: None },
        )
        .await;

    assert!(matches!(nested, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn list_threads_returns_every_thread_under_a_channel_newest_first() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server_id, channel_id) = setup_server_and_channel(&domain, alice).await;

    let first = domain
        .create_thread(alice, channel_id, CreateThreadInput { title: "first".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");
    let second = domain
        .create_thread(alice, channel_id, CreateThreadInput { title: "second".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    let threads = domain
        .list_threads(alice, channel_id)
        .await
        .expect("list_threads succeeds");

    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].id, second.id);
    assert_eq!(threads[1].id, first.id);
}

#[tokio::test]
async fn messages_can_be_sent_and_listed_inside_a_thread_exactly_like_a_channel() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server_id, channel_id) = setup_server_and_channel(&domain, alice).await;

    let thread = domain
        .create_thread(alice, channel_id, CreateThreadInput { title: "a topic".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    domain
        .send_message(alice, thread.id, SendMessageInput { content: "first reply".to_string() })
        .await
        .expect("sending a message inside a thread succeeds exactly like any channel");

    let messages = domain
        .list_messages(alice, thread.id, Default::default())
        .await
        .expect("list_messages succeeds");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content.as_deref(), Some("first reply"));
}

#[tokio::test]
async fn a_thread_inherits_its_servers_visibility_for_resolve_read_access() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(
            alice,
            CreateServerInput { name: "Public Place".to_string(), visibility: Some("public".to_string()) },
        )
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    let thread = domain
        .create_thread(alice, channel.id, CreateThreadInput { title: "public topic".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    let access = domain
        .resolve_read_access(None, thread.id)
        .await
        .expect("a thread under a public server is anonymously readable");
    assert_eq!(access, ReadAccess::Public);
}
