//! `DomainService::get_profile_context` — data gathering only (relationship,
//! directional blocks, tombstone, shared server context). The privacy
//! judgment over these facts is `decide_profile_visibility`, already fully
//! covered by `crates/domain/src/profile_visibility.rs`'s own unit tests;
//! nothing here re-tests that decision.

use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::{CreateServerInput, DomainError, DomainService, ProfileViewerRelationship};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};

async fn test_services() -> (
    DomainService,
    AuthService,
    db::PgPool,
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        // postgres:16, the tag production runs (docker-compose.yml).
        // The crate default is 11-alpine: five majors and a different
        // libc away from the database this schema is deployed on.
        .with_tag("16")
        .start()
        .await
        .expect("postgres container starts");

    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let database_url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let pool = db::build_pool(&database_url)
        .await
        .expect("pool connects");
    db::run_migrations(&pool).await.expect("migrations run");

    (
        DomainService::new(pool.clone()),
        AuthService::new(pool.clone(), std::sync::Arc::new(mailer::CaptureMailer::new())),
        pool,
        container,
    )
}

async fn register(auth: &AuthService, email: &str, username: &str) -> Uuid {
    auth.create_verified_account(RegisterInput {
        email: email.to_string(),
        username: username.to_string(),
        password: "correct horse battery staple".to_string(),
        display_name: username.to_string(),
    })
    .await
    .expect("registration succeeds")
    .id
}

#[tokio::test]
async fn self_view_reports_no_relationship_facts_and_no_blocks() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let ctx = domain
        .get_profile_context(alice, alice, None)
        .await
        .expect("self view succeeds");

    assert_eq!(ctx.relationship, ProfileViewerRelationship::SelfView);
    assert!(!ctx.caller_blocked_owner);
    assert!(!ctx.owner_blocked_caller);
    assert!(!ctx.has_shared_server_context);
    assert!(ctx.server_context.is_none());
}

#[tokio::test]
async fn an_accepted_friendship_yields_friend_relationship() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.send_friend_request(alice, bob).await.unwrap();
    domain.send_friend_request(bob, alice).await.unwrap(); // accepted

    let ctx = domain.get_profile_context(alice, bob, None).await.unwrap();

    assert_eq!(ctx.relationship, ProfileViewerRelationship::Friend);
}

#[tokio::test]
async fn no_friendship_yields_none_relationship() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let ctx = domain.get_profile_context(alice, bob, None).await.unwrap();

    assert_eq!(ctx.relationship, ProfileViewerRelationship::None);
}

#[tokio::test]
async fn a_pending_friend_request_does_not_yield_friend_relationship() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.send_friend_request(alice, bob).await.unwrap();

    let from_alice = domain.get_profile_context(alice, bob, None).await.unwrap();
    let from_bob = domain.get_profile_context(bob, alice, None).await.unwrap();

    assert_eq!(from_alice.relationship, ProfileViewerRelationship::None);
    assert_eq!(from_bob.relationship, ProfileViewerRelationship::None);
}

#[tokio::test]
async fn directional_blocks_are_reported_from_both_sides() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.block_account(alice, bob).await.unwrap();

    let from_alice = domain.get_profile_context(alice, bob, None).await.unwrap();
    assert!(from_alice.caller_blocked_owner);
    assert!(!from_alice.owner_blocked_caller);

    let from_bob = domain.get_profile_context(bob, alice, None).await.unwrap();
    assert!(!from_bob.caller_blocked_owner);
    assert!(from_bob.owner_blocked_caller);
}

#[tokio::test]
async fn a_deleted_account_is_reported_via_deleted_at() {
    let (domain, auth, pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    // No account-deletion service method exists yet; set the column directly.
    sqlx::query("UPDATE account SET deleted_at = now() WHERE id = $1")
        .bind(bob)
        .execute(&pool)
        .await
        .expect("deleted_at updates");

    let ctx = domain.get_profile_context(alice, bob, None).await.unwrap();

    assert!(ctx.profile.deleted_at.is_some());
}

#[tokio::test]
async fn shared_server_context_requires_both_caller_and_target_to_be_members() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    let server = domain
        .create_server(
            alice,
            CreateServerInput { name: "Alice's Place".to_string(), visibility: None },
        )
        .await
        .expect("create_server succeeds");
    domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees invite code"))
        .await
        .expect("bob joins");

    // Both alice and bob are members: shared context, target's nickname/roles come back.
    let shared = domain
        .get_profile_context(alice, bob, Some(server.id))
        .await
        .unwrap();
    assert!(shared.has_shared_server_context);
    assert!(shared.server_context.is_some());

    // Carol is not a member of the server at all: no shared context, even
    // though the target (bob) is a member.
    let not_shared = domain
        .get_profile_context(carol, bob, Some(server.id))
        .await
        .unwrap();
    assert!(!not_shared.has_shared_server_context);
    assert!(not_shared.server_context.is_none());

    // Bob (the target) not being a member of a DIFFERENT server: no context.
    let other_server = domain
        .create_server(
            carol,
            CreateServerInput { name: "Carol's Place".to_string(), visibility: None },
        )
        .await
        .expect("create_server succeeds");
    let target_not_member = domain
        .get_profile_context(carol, bob, Some(other_server.id))
        .await
        .unwrap();
    assert!(!target_not_member.has_shared_server_context);
    assert!(target_not_member.server_context.is_none());
}

#[tokio::test]
async fn a_nonexistent_target_returns_account_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let ghost = app_core::new_id();

    let result = domain.get_profile_context(alice, ghost, None).await;

    assert!(matches!(result, Err(DomainError::AccountNotFound)));
}

#[tokio::test]
async fn bulk_gathers_the_same_relationship_facts_as_the_single_lookup() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;
    let dave = register(&auth, "dave@example.com", "dave").await;

    domain.send_friend_request(alice, bob).await.unwrap();
    domain.send_friend_request(bob, alice).await.unwrap();
    domain.block_account(alice, carol).await.unwrap();
    domain.block_account(dave, alice).await.unwrap();

    let contexts = domain
        .get_profile_contexts_bulk(alice, &[bob, carol, dave, alice])
        .await
        .unwrap();

    assert_eq!(contexts.len(), 4);
    assert_eq!(contexts[0].relationship, ProfileViewerRelationship::Friend);
    assert!(contexts[1].caller_blocked_owner);
    assert!(!contexts[1].owner_blocked_caller);
    assert!(contexts[2].owner_blocked_caller);
    assert!(!contexts[2].caller_blocked_owner);
    assert_eq!(contexts[3].relationship, ProfileViewerRelationship::SelfView);

    // Every entry must agree with what the single-account path reports.
    for (context, target) in contexts.iter().zip([bob, carol, dave, alice]) {
        let single = domain.get_profile_context(alice, target, None).await.unwrap();
        assert_eq!(context.relationship, single.relationship);
        assert_eq!(context.caller_blocked_owner, single.caller_blocked_owner);
        assert_eq!(context.owner_blocked_caller, single.owner_blocked_caller);
    }
}

#[tokio::test]
async fn bulk_skips_ids_that_name_no_account_and_never_carries_server_context() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let contexts = domain
        .get_profile_contexts_bulk(alice, &[bob, app_core::new_id()])
        .await
        .unwrap();

    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].profile.id, bob);
    assert!(contexts[0].server_context.is_none());
    assert!(!contexts[0].has_shared_server_context);
}

#[tokio::test]
async fn a_username_resolves_to_the_same_context_as_its_id() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let by_username = domain
        .get_profile_context_by_username(alice, "bob", None)
        .await
        .unwrap();
    assert_eq!(by_username.profile.id, bob);

    let missing = domain
        .get_profile_context_by_username(alice, "Bob", None)
        .await;
    assert!(
        matches!(missing, Err(DomainError::AccountNotFound)),
        "the lookup is exact; usernames are unique case-sensitively"
    );
}
