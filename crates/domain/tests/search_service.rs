//! Full-text search over a server's messages. Same harness as
//! `domain_service.rs`.

use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::{
    CreateChannelInput, CreateServerInput, DomainError, DomainService, SearchInput,
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
async fn search_finds_a_message_by_word_and_ignores_unrelated_ones() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");

    domain
        .send_message(alice, channel.id, SendMessageInput { content: "how do I configure the widget".to_string() })
        .await
        .expect("send_message succeeds");
    domain
        .send_message(alice, channel.id, SendMessageInput { content: "completely unrelated chatter".to_string() })
        .await
        .expect("send_message succeeds");

    let results = domain
        .search_messages(alice, server.id, SearchInput { query: "widget".to_string(), ..Default::default() })
        .await
        .expect("search_messages succeeds");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content.as_deref(), Some("how do I configure the widget"));
}

#[tokio::test]
async fn search_respects_the_channel_filter() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let general = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    let off_topic = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "off-topic".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");

    domain
        .send_message(alice, general.id, SendMessageInput { content: "widget in general".to_string() })
        .await
        .expect("send_message succeeds");
    domain
        .send_message(alice, off_topic.id, SendMessageInput { content: "widget in off-topic".to_string() })
        .await
        .expect("send_message succeeds");

    let results = domain
        .search_messages(
            alice,
            server.id,
            SearchInput { query: "widget".to_string(), channel_id: Some(general.id), ..Default::default() },
        )
        .await
        .expect("search_messages succeeds");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].channel_id, general.id);
}

#[tokio::test]
async fn search_respects_the_author_filter() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");
    domain
        .join_via_invite(bob, &server.invite_code.clone().expect("owner sees invite code"))
        .await
        .expect("bob joins");

    domain
        .send_message(alice, channel.id, SendMessageInput { content: "widget from alice".to_string() })
        .await
        .expect("send_message succeeds");
    domain
        .send_message(bob, channel.id, SendMessageInput { content: "widget from bob".to_string() })
        .await
        .expect("send_message succeeds");

    let results = domain
        .search_messages(
            alice,
            server.id,
            SearchInput { query: "widget".to_string(), author_account_id: Some(bob), ..Default::default() },
        )
        .await
        .expect("search_messages succeeds");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].author_account_id, bob);
}

#[tokio::test]
async fn search_excludes_soft_deleted_messages() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");
    let channel = domain
        .create_channel(alice, server.id, CreateChannelInput { name: "general".to_string(), kind: None })
        .await
        .expect("create_channel succeeds");

    let message = domain
        .send_message(alice, channel.id, SendMessageInput { content: "widget to be deleted".to_string() })
        .await
        .expect("send_message succeeds");
    domain
        .delete_message(alice, channel.id, message.id)
        .await
        .expect("delete_message succeeds");

    let results = domain
        .search_messages(alice, server.id, SearchInput { query: "widget".to_string(), ..Default::default() })
        .await
        .expect("search_messages succeeds");

    assert!(results.is_empty());
}

#[tokio::test]
async fn a_non_member_cannot_search_a_servers_messages() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");

    let result = domain
        .search_messages(bob, server.id, SearchInput { query: "widget".to_string(), ..Default::default() })
        .await;

    assert!(matches!(result, Err(DomainError::ServerNotFound)));
}

#[tokio::test]
async fn an_empty_query_is_rejected() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let server = domain
        .create_server(alice, CreateServerInput { name: "Alice's Place".to_string(), visibility: None })
        .await
        .expect("create_server succeeds");

    let result = domain
        .search_messages(alice, server.id, SearchInput { query: "   ".to_string(), ..Default::default() })
        .await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}
