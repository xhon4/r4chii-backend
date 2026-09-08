//! Visibility model — `resolve_read_access`, channel visibility
//! overrides, and server visibility updates. Same harness as
//! `domain_service.rs`.

use domain::{CreateChannelInput, CreateServerInput, DomainError, ReadAccess};

mod common;
use common::*;

fn create_server_input(name: &str, visibility: Option<&str>) -> CreateServerInput {
    CreateServerInput {
        name: name.to_string(),
        visibility: visibility.map(str::to_string),
    }
}

#[tokio::test]
async fn a_member_always_resolves_to_member_access_regardless_of_visibility() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    // Default visibility (private) — the owner is still a Member, not
    // Public, and a Member never needs the channel to be public at all.
    let server = domain
        .create_server(alice, create_server_input("Alice's Place", None))
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");

    let access = domain
        .resolve_read_access(Some(alice), channel.id)
        .await
        .expect("member can always resolve read access");
    assert_eq!(access, ReadAccess::Member);
}

#[tokio::test]
async fn an_anonymous_caller_gets_public_access_only_on_a_public_server() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let private_server = domain
        .create_server(alice, create_server_input("Private Place", None))
        .await
        .expect("create_server succeeds");
    let private_channel = domain
        .create_channel(
            alice,
            private_server.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");

    let denied = domain.resolve_read_access(None, private_channel.id).await;
    assert!(
        matches!(denied, Err(DomainError::ChannelNotFound)),
        "a private server must not leak existence to an anonymous caller"
    );

    let public_server = domain
        .create_server(alice, create_server_input("Public Place", Some("public")))
        .await
        .expect("create_server succeeds");
    let public_channel = domain
        .create_channel(
            alice,
            public_server.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");

    let allowed = domain
        .resolve_read_access(None, public_channel.id)
        .await
        .expect("a public server's channel is anonymously readable");
    assert_eq!(allowed, ReadAccess::Public);
}

#[tokio::test]
async fn a_channel_override_can_narrow_a_public_server_to_private() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Public Place", Some("public")))
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput { name: "staff".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");

    // Alice is the owner, so MANAGE_VISIBILITY is implied without an
    // explicit role grant.
    domain
        .update_channel_visibility(alice, server.id, channel.id, Some("private".to_string()))
        .await
        .expect("narrowing a channel below its public server is allowed");

    let denied = domain.resolve_read_access(None, channel.id).await;
    assert!(
        matches!(denied, Err(DomainError::ChannelNotFound)),
        "the narrowed channel must not be anonymously readable even though its server is public"
    );
}

#[tokio::test]
async fn a_channel_override_cannot_be_broader_than_its_server() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let server = domain
        .create_server(alice, create_server_input("Private Place", None))
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");

    let result = domain
        .update_channel_visibility(alice, server.id, channel.id, Some("public".to_string()))
        .await;

    assert!(
        matches!(result, Err(DomainError::Validation(_))),
        "a private server cannot have a public channel"
    );
}

#[tokio::test]
async fn a_plain_member_without_manage_visibility_cannot_update_channel_visibility() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place", Some("public")))
        .await
        .expect("create_server succeeds");
    domain
        .join_via_invite(bob, &server.invite_code.clone().expect("owner sees invite code"))
        .await
        .expect("bob joins");
    let channel = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");

    let result = domain
        .update_channel_visibility(bob, server.id, channel.id, Some("private".to_string()))
        .await;

    assert!(matches!(result, Err(DomainError::MissingPermission)));
}

#[tokio::test]
async fn only_the_owner_can_update_server_visibility() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Alice's Place", None))
        .await
        .expect("create_server succeeds");
    domain
        .join_via_invite(bob, &server.invite_code.clone().expect("owner sees invite code"))
        .await
        .expect("bob joins");

    let denied = domain
        .update_server_visibility(bob, server.id, "public".to_string())
        .await;
    assert!(matches!(denied, Err(DomainError::MissingPermission)));

    let updated = domain
        .update_server_visibility(alice, server.id, "public".to_string())
        .await
        .expect("owner can flip server visibility");
    assert_eq!(updated.visibility, "public");
}
