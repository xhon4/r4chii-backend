use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::{DomainError, DomainService};
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

fn register_input(email: &str, username: &str) -> RegisterInput {
    RegisterInput {
        email: email.to_string(),
        username: username.to_string(),
        password: "correct horse battery staple".to_string(),
        display_name: username.to_string(),
    }
}

async fn register(auth: &AuthService, email: &str, username: &str) -> Uuid {
    auth.create_verified_account(register_input(email, username))
        .await
        .expect("registration succeeds")
        .id
}

#[tokio::test]
async fn sending_a_first_friend_request_creates_a_pending_row() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (friendship, changed) = domain
        .send_friend_request(alice, bob)
        .await
        .expect("request succeeds");

    assert!(changed);
    assert_eq!(friendship.status, "pending");
    assert_eq!(friendship.requested_by, alice);
    assert_eq!(friendship.account_id, bob);
}

#[tokio::test]
async fn a_reciprocal_request_accepts_the_existing_pending_request() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain
        .send_friend_request(alice, bob)
        .await
        .expect("request succeeds");

    let (accepted, changed) = domain
        .send_friend_request(bob, alice)
        .await
        .expect("accept succeeds");

    assert!(changed);
    assert_eq!(accepted.status, "accepted");
}

#[tokio::test]
async fn re_requesting_the_same_pending_direction_is_a_no_op() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain
        .send_friend_request(alice, bob)
        .await
        .expect("request succeeds");

    let (friendship, changed) = domain
        .send_friend_request(alice, bob)
        .await
        .expect("repeat request succeeds");

    assert!(!changed);
    assert_eq!(friendship.status, "pending");
}

#[tokio::test]
async fn requesting_an_already_accepted_friendship_is_a_no_op() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.send_friend_request(alice, bob).await.unwrap();
    domain.send_friend_request(bob, alice).await.unwrap();

    let (friendship, changed) = domain
        .send_friend_request(alice, bob)
        .await
        .expect("request succeeds");

    assert!(!changed);
    assert_eq!(friendship.status, "accepted");
}

#[tokio::test]
async fn friend_request_with_yourself_is_rejected() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain.send_friend_request(alice, alice).await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn friend_request_to_a_nonexistent_account_returns_account_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let ghost = app_core::new_id();

    let result = domain.send_friend_request(alice, ghost).await;

    assert!(matches!(result, Err(DomainError::AccountNotFound)));
}

#[tokio::test]
async fn list_friendships_returns_both_pending_and_accepted_rows_from_the_callers_perspective() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    domain.send_friend_request(alice, bob).await.unwrap();
    domain.send_friend_request(bob, alice).await.unwrap(); // accepted
    domain.send_friend_request(alice, carol).await.unwrap(); // still pending

    let friendships = domain.list_friendships(alice).await.unwrap();

    assert_eq!(friendships.len(), 2);
    assert!(friendships.iter().any(|f| f.account_id == bob && f.status == "accepted"));
    assert!(friendships.iter().any(|f| f.account_id == carol && f.status == "pending"));
}

#[tokio::test]
async fn removing_a_pending_request_lets_a_fresh_request_start_over() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.send_friend_request(alice, bob).await.unwrap();
    domain.remove_friendship(bob, alice).await.expect("decline succeeds");

    let friendships = domain.list_friendships(alice).await.unwrap();
    assert!(friendships.is_empty());

    let (friendship, changed) = domain.send_friend_request(alice, bob).await.unwrap();
    assert!(changed);
    assert_eq!(friendship.status, "pending");
}

#[tokio::test]
async fn removing_an_accepted_friendship_unfriends_both_sides() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.send_friend_request(alice, bob).await.unwrap();
    domain.send_friend_request(bob, alice).await.unwrap();

    domain.remove_friendship(alice, bob).await.expect("unfriend succeeds");

    assert!(domain.list_friendships(alice).await.unwrap().is_empty());
    assert!(domain.list_friendships(bob).await.unwrap().is_empty());
}

#[tokio::test]
async fn removing_a_nonexistent_friendship_returns_friend_request_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let result = domain.remove_friendship(alice, bob).await;

    assert!(matches!(result, Err(DomainError::FriendRequestNotFound)));
}

#[tokio::test]
async fn a_friend_request_between_a_blocked_pair_is_rejected_in_either_direction() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.block_account(alice, bob).await.unwrap();

    let from_blocker = domain.send_friend_request(alice, bob).await;
    let from_blocked = domain.send_friend_request(bob, alice).await;

    assert!(matches!(from_blocker, Err(DomainError::Blocked)));
    assert!(matches!(from_blocked, Err(DomainError::Blocked)));
}

// Without the `pg_advisory_xact_lock` on the canonically-ordered account
// pair in `send_friend_request`, this is the same class of TOCTOU race
// `create_dm` guards against: concurrent requests from both sides could
// each miss the other's insert and end up with two rows, or a duplicate-key
// error instead of a clean accept.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_mutual_friend_requests_settle_on_exactly_one_accepted_row() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let handles: Vec<_> = (0..6)
        .map(|i| {
            let domain = domain.clone();
            let (first, second) = if i % 2 == 0 { (alice, bob) } else { (bob, alice) };
            tokio::spawn(async move { domain.send_friend_request(first, second).await })
        })
        .collect();

    for handle in handles {
        handle
            .await
            .expect("task does not panic")
            .expect("send_friend_request succeeds");
    }

    let friendships = domain.list_friendships(alice).await.unwrap();
    assert_eq!(friendships.len(), 1);
    assert_eq!(friendships[0].status, "accepted");
}
