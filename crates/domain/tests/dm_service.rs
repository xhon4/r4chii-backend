use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::{CreateGroupDmInput, DomainError, DomainService, SendMessageInput};
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
        display_name: "Test User".to_string(),
    }
}

async fn register(auth: &AuthService, email: &str, username: &str) -> Uuid {
    auth.create_verified_account(register_input(email, username))
        .await
        .expect("registration succeeds")
        .id
}

#[tokio::test]
async fn create_dm_creates_a_new_dm_channel_with_both_participants() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (channel, created) = domain
        .create_dm(alice, bob)
        .await
        .expect("create_dm succeeds");

    assert!(created);
    assert_eq!(channel.kind, "dm");
    assert!(channel.server_id.is_none());

    let mut members = domain
        .authorized_account_ids(channel.id)
        .await
        .expect("authorized_account_ids succeeds");
    members.sort();
    let mut expected = vec![alice, bob];
    expected.sort();
    assert_eq!(members, expected);
}

#[tokio::test]
async fn create_dm_is_idempotent_regardless_of_argument_order() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let (first, first_created) = domain
        .create_dm(alice, bob)
        .await
        .expect("create_dm succeeds");
    assert!(first_created);

    let (second, second_created) = domain
        .create_dm(bob, alice)
        .await
        .expect("create_dm succeeds");
    assert!(!second_created);

    assert_eq!(first.id, second.id);
}

// Without the `pg_advisory_xact_lock` on the canonically-ordered account
// pair in `create_dm`, this is a TOCTOU race: several concurrent attempts
// can all miss the existing-DM check and each insert their own channel,
// leaving two "the" DM between alice and bob instead of one.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_create_dm_never_creates_duplicate_channels() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let handles: Vec<_> = (0..6)
        .map(|i| {
            let domain = domain.clone();
            let (first, second) = if i % 2 == 0 {
                (alice, bob)
            } else {
                (bob, alice)
            };
            tokio::spawn(async move { domain.create_dm(first, second).await })
        })
        .collect();

    let mut channel_ids = std::collections::HashSet::new();
    let mut created_count = 0;
    for handle in handles {
        let (channel, created) = handle
            .await
            .expect("task does not panic")
            .expect("create_dm succeeds");
        if created {
            created_count += 1;
        }
        channel_ids.insert(channel.id);
    }

    assert_eq!(created_count, 1, "exactly one attempt should create the DM");
    assert_eq!(
        channel_ids.len(),
        1,
        "all attempts must resolve to the same DM channel"
    );
}

#[tokio::test]
async fn create_dm_with_yourself_is_rejected() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain.create_dm(alice, alice).await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn create_dm_with_a_nonexistent_account_returns_account_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let result = domain.create_dm(alice, app_core::new_id()).await;

    assert!(matches!(result, Err(DomainError::AccountNotFound)));
}

#[tokio::test]
async fn messages_can_be_sent_in_a_dm_and_are_invisible_to_non_participants() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    let (channel, _created) = domain
        .create_dm(alice, bob)
        .await
        .expect("create_dm succeeds");

    domain
        .send_message(
            alice,
            channel.id,
            SendMessageInput {
                content: "hey bob".to_string(),
            },
        )
        .await
        .expect("send_message succeeds");

    let bob_view = domain
        .list_messages(bob, channel.id, Default::default())
        .await
        .expect("bob can list the dm");
    assert_eq!(bob_view.len(), 1);

    let carol_view = domain.list_messages(carol, channel.id, Default::default()).await;
    assert!(matches!(carol_view, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn create_group_dm_adds_the_creator_and_every_participant() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    let channel = domain
        .create_group_dm(
            alice,
            CreateGroupDmInput {
                account_ids: vec![bob, carol],
            },
        )
        .await
        .expect("create_group_dm succeeds");

    assert_eq!(channel.kind, "group_dm");

    let mut members = domain
        .authorized_account_ids(channel.id)
        .await
        .expect("authorized_account_ids succeeds");
    members.sort();
    let mut expected = vec![alice, bob, carol];
    expected.sort();
    assert_eq!(members, expected);
}

#[tokio::test]
async fn create_group_dm_dedupes_the_creator_and_repeated_ids() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    let channel = domain
        .create_group_dm(
            alice,
            CreateGroupDmInput {
                account_ids: vec![alice, bob, bob, carol],
            },
        )
        .await
        .expect("create_group_dm succeeds");

    let members = domain
        .authorized_account_ids(channel.id)
        .await
        .expect("authorized_account_ids succeeds");
    assert_eq!(members.len(), 3, "no duplicate channel_member rows");
}

#[tokio::test]
async fn create_group_dm_requires_at_least_two_other_participants() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let result = domain
        .create_group_dm(
            alice,
            CreateGroupDmInput {
                account_ids: vec![bob],
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn create_group_dm_with_a_nonexistent_participant_returns_account_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let result = domain
        .create_group_dm(
            alice,
            CreateGroupDmInput {
                account_ids: vec![bob, app_core::new_id()],
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::AccountNotFound)));
}

#[tokio::test]
async fn list_dms_returns_only_the_callers_dm_and_group_dm_channels() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    let (dm, _created) = domain
        .create_dm(alice, bob)
        .await
        .expect("create_dm succeeds");
    let group = domain
        .create_group_dm(
            alice,
            CreateGroupDmInput {
                account_ids: vec![bob, carol],
            },
        )
        .await
        .expect("create_group_dm succeeds");

    let mut alice_dms: Vec<Uuid> = domain
        .list_dms(alice)
        .await
        .expect("list_dms succeeds")
        .into_iter()
        .map(|c| c.id)
        .collect();
    alice_dms.sort();

    let mut expected = vec![dm.id, group.id];
    expected.sort();
    assert_eq!(alice_dms, expected);

    // Carol is only in the group dm, not the 1:1 between alice and bob.
    let carol_dms: Vec<Uuid> = domain
        .list_dms(carol)
        .await
        .expect("list_dms succeeds")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(carol_dms, vec![group.id]);
}
