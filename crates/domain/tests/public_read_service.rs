//! The public read path's domain layer — `get_public_thread` and
//! `list_public_threads`. Same harness as `domain_service.rs`.

use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::{
    CreateChannelInput, CreateServerInput, CreateThreadInput, DomainError, DomainService,
    SendMessageInput,
};
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};

async fn test_services() -> (
    DomainService,
    AuthService,
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
        AuthService::new(pool, std::sync::Arc::new(mailer::CaptureMailer::new())),
        container,
    )
}

async fn register(auth: &AuthService, email: &str, username: &str) -> Uuid {
    auth.create_verified_account(RegisterInput {
        email: email.to_string(),
        username: username.to_string(),
        password: "correct horse battery staple".to_string(),
        display_name: "Test User".to_string(),
    })
    .await
    .expect("registration succeeds")
    .id
}

#[tokio::test]
async fn a_thread_on_a_public_server_is_anonymously_readable() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Public Place".to_string(), visibility: Some("public".to_string()) })
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    let thread = domain
        .create_thread(alice, channel.id, CreateThreadInput { title: "How do I configure X?".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");
    domain
        .send_message(alice, thread.id, SendMessageInput { content: "does anyone know?".to_string() })
        .await
        .expect("send_message succeeds");

    let (returned_thread, messages) = domain
        .get_public_thread(thread.id, None)
        .await
        .expect("a public thread is anonymously readable");

    assert_eq!(returned_thread.id, thread.id);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "does anyone know?");
    assert_eq!(messages[0].author_display_name, "Test User");
}

#[tokio::test]
async fn a_thread_on_a_private_server_is_not_anonymously_readable() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Private Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    let thread = domain
        .create_thread(alice, channel.id, CreateThreadInput { title: "secret topic".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    let result = domain.get_public_thread(thread.id, None).await;
    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn a_non_thread_channel_id_is_not_a_valid_public_thread_url() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Public Place".to_string(), visibility: Some("public".to_string()) })
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");

    // The plain text channel itself is public too, but it is not a thread —
    // the public read path only has URLs for threads.
    let result = domain.get_public_thread(channel.id, None).await;
    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn list_public_threads_includes_public_but_not_private_or_unlisted() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;

    let public_server = domain
        .create_server(alice, CreateServerInput { name: "Public".to_string(), visibility: Some("public".to_string()) })
        .await
        .expect("create_server succeeds");
    let public_channel = domain
        .create_channel(alice, public_server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    let public_thread = domain
        .create_thread(alice, public_channel.id, CreateThreadInput { title: "public topic".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    let unlisted_server = domain
        .create_server(alice, CreateServerInput { name: "Unlisted".to_string(), visibility: Some("unlisted".to_string()) })
        .await
        .expect("create_server succeeds");
    let unlisted_channel = domain
        .create_channel(alice, unlisted_server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    domain
        .create_thread(alice, unlisted_channel.id, CreateThreadInput { title: "unlisted topic".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    let private_server = domain
        .create_server(alice, CreateServerInput { name: "Private".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let private_channel = domain
        .create_channel(alice, private_server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    domain
        .create_thread(alice, private_channel.id, CreateThreadInput { title: "private topic".to_string(), root_message_id: None })
        .await
        .expect("create_thread succeeds");

    let sitemap = domain
        .list_public_threads()
        .await
        .expect("list_public_threads succeeds");

    assert_eq!(sitemap.len(), 1);
    assert_eq!(sitemap[0].id, public_thread.id);
}
