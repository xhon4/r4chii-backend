use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::{CreateChannelInput, CreateServerInput, CreateThreadInput, DomainService};
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

    let pool = db::build_pool(&database_url).await.expect("pool connects");
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

fn create_server_input(name: &str) -> CreateServerInput {
    CreateServerInput {
        name: name.to_string(),
        visibility: None,
    }
}

#[tokio::test]
async fn observers_include_every_fellow_member_across_multiple_servers_and_threads() {
    // Round-2 judgment (S9): the old resolver walked accessible_channel_ids,
    // which counts every thread (a thread is itself a channel row) —
    // cost grew unboundedly with thread count. The set-based replacement
    // must still return exactly the same observer set: this seeds two
    // servers, each with several threads, and checks nothing was lost or
    // duplicated by switching implementations.
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let carol = register(&auth, "carol@example.com", "carol").await;

    let server_a = domain
        .create_server(alice, create_server_input("Server A"))
        .await
        .unwrap();
    domain
        .join_via_invite(bob, server_a.invite_code.as_ref().unwrap())
        .await
        .unwrap();

    let text_channel_a = domain
        .create_channel(
            alice,
            server_a.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");

    for i in 0..3 {
        domain
            .create_thread(
                alice,
                text_channel_a.id,
                CreateThreadInput { title: format!("Thread {i}"), root_message_id: None },
            )
            .await
            .expect("thread creation succeeds");
    }

    let server_b = domain
        .create_server(carol, create_server_input("Server B"))
        .await
        .unwrap();
    domain
        .join_via_invite(alice, server_b.invite_code.as_ref().unwrap())
        .await
        .unwrap();

    let text_channel_b = domain
        .create_channel(
            carol,
            server_b.id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds");
    for i in 0..2 {
        domain
            .create_thread(
                carol,
                text_channel_b.id,
                CreateThreadInput { title: format!("B Thread {i}"), root_message_id: None },
            )
            .await
            .unwrap();
    }

    // Alice is in both servers: her observers are bob (Server A) and carol
    // (Server B), plus herself.
    let observers = domain.presence_observer_account_ids(alice).await.unwrap();

    assert!(observers.contains(&alice), "subject sees their own transitions");
    assert!(observers.contains(&bob), "fellow Server A member observes alice");
    assert!(observers.contains(&carol), "fellow Server B member observes alice");
    assert_eq!(observers.len(), 3, "no duplicates across the two servers' threads");
}

#[tokio::test]
async fn a_dm_partner_is_an_observer_even_with_no_shared_server() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    domain.create_dm(alice, bob).await.unwrap();

    let observers = domain.presence_observer_account_ids(alice).await.unwrap();
    assert!(observers.contains(&bob));
}

#[tokio::test]
async fn blocking_someone_removes_you_from_their_observer_set_but_not_the_reverse() {
    // "a block hides the blocked user's social presence from the blocker"
    // — directional. Alice blocking bob means bob's presence
    // updates no longer reach alice; it does not touch alice's own
    // visibility to bob.
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;

    let server = domain
        .create_server(alice, create_server_input("Shared Server"))
        .await
        .unwrap();
    domain
        .join_via_invite(bob, server.invite_code.as_ref().unwrap())
        .await
        .unwrap();

    domain.block_account(alice, bob).await.unwrap();

    let bobs_observers = domain.presence_observer_account_ids(bob).await.unwrap();
    assert!(
        !bobs_observers.contains(&alice),
        "alice blocked bob, so alice must not observe bob's presence"
    );

    let alices_observers = domain.presence_observer_account_ids(alice).await.unwrap();
    assert!(
        alices_observers.contains(&bob),
        "bob did not block alice, so bob still observes alice's presence"
    );
}
