use domain::{CreateGroupDmInput, DomainError};

mod common;
use common::*;

#[tokio::test]
async fn blocking_an_account_creates_a_block_row() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (block, created) = domain.block_account(alice, bob).await.expect("block succeeds");

    assert!(created);
    assert_eq!(block.account_id, bob);
}

#[tokio::test]
async fn blocking_the_same_account_twice_is_idempotent() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.block_account(alice, bob).await.unwrap();
    let (block, created) = domain.block_account(alice, bob).await.expect("second block succeeds");

    assert!(!created);
    assert_eq!(block.account_id, bob);
}

#[tokio::test]
async fn blocking_someone_removes_any_existing_friendship() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.send_friend_request(alice, bob).await.unwrap();
    domain.send_friend_request(bob, alice).await.unwrap(); // accepted

    domain.block_account(alice, bob).await.expect("block succeeds");

    assert!(domain.list_friendships(alice).await.unwrap().is_empty());
    assert!(domain.list_friendships(bob).await.unwrap().is_empty());
}

#[tokio::test]
async fn block_with_yourself_is_rejected() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain.block_account(alice, alice).await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn blocking_a_nonexistent_account_returns_account_not_found() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let ghost = app_core::new_id();

    let result = domain.block_account(alice, ghost).await;

    assert!(matches!(result, Err(DomainError::AccountNotFound)));
}

#[tokio::test]
async fn list_blocks_returns_only_the_callers_own_outbound_blocks() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    domain.block_account(alice, bob).await.unwrap();
    domain.block_account(carol, alice).await.unwrap(); // block placed against alice, not by her

    let blocks = domain.list_blocks(alice).await.unwrap();

    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].account_id, bob);
}

#[tokio::test]
async fn unblocking_removes_the_block_row() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.block_account(alice, bob).await.unwrap();
    domain.unblock_account(alice, bob).await.expect("unblock succeeds");

    assert!(domain.list_blocks(alice).await.unwrap().is_empty());
}

#[tokio::test]
async fn unblocking_a_nonexistent_block_returns_block_not_found() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let result = domain.unblock_account(alice, bob).await;

    assert!(matches!(result, Err(DomainError::BlockNotFound)));
}

#[tokio::test]
async fn a_block_prevents_creating_a_new_dm_in_either_direction() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.block_account(alice, bob).await.unwrap();

    let from_blocker = domain.create_dm(alice, bob).await;
    let from_blocked = domain.create_dm(bob, alice).await;

    assert!(matches!(from_blocker, Err(DomainError::Blocked)));
    assert!(matches!(from_blocked, Err(DomainError::Blocked)));
}

#[tokio::test]
async fn a_block_placed_after_a_dm_exists_prevents_further_messages_in_it() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (channel, _created) = domain.create_dm(alice, bob).await.unwrap();
    domain
        .send_message(alice, channel.id, domain::SendMessageInput { content: "hi".to_string() })
        .await
        .expect("message before the block succeeds");

    domain.block_account(bob, alice).await.unwrap();

    let blocked_send = domain
        .send_message(alice, channel.id, domain::SendMessageInput { content: "hi again".to_string() })
        .await;

    assert!(matches!(blocked_send, Err(DomainError::Blocked)));
}

#[tokio::test]
async fn unblocking_restores_the_ability_to_message_in_an_existing_dm() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (channel, _created) = domain.create_dm(alice, bob).await.unwrap();
    domain.block_account(bob, alice).await.unwrap();
    domain.unblock_account(bob, alice).await.unwrap();

    domain
        .send_message(alice, channel.id, domain::SendMessageInput { content: "hi".to_string() })
        .await
        .expect("message after unblock succeeds");
}

#[tokio::test]
async fn a_block_prevents_creating_a_group_dm_that_includes_the_blocked_account() {
    let (domain, auth, _pool, _container) = test_services_with_pool().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    domain.block_account(alice, bob).await.unwrap();

    let from_blocker = domain
        .create_group_dm(alice, CreateGroupDmInput { account_ids: vec![bob, carol] })
        .await;
    let from_blocked = domain
        .create_group_dm(bob, CreateGroupDmInput { account_ids: vec![alice, carol] })
        .await;

    assert!(matches!(from_blocker, Err(DomainError::Blocked)));
    assert!(matches!(from_blocked, Err(DomainError::Blocked)));
}
