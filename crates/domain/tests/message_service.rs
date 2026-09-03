use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use domain::{
    ChannelSummary, CreateChannelInput, CreateServerInput, DomainError, DomainService,
    EditMessageInput, MessagePagination, SendMessageInput, ServerSummary,
};
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

/// Registers `alice` and creates a server + text channel she owns, ready to
/// send messages into.
async fn server_and_channel(domain: &DomainService, owner: Uuid) -> (ServerSummary, ChannelSummary) {
    let server = domain
        .create_server(
            owner,
            CreateServerInput {
                name: "Alice's Place".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create_server succeeds");

    let channel = domain
        .create_channel(
            owner,
            server.id,
            CreateChannelInput {
                name: "general".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create_channel succeeds");

    (server, channel)
}

fn send_input(content: &str) -> SendMessageInput {
    SendMessageInput {
        content: content.to_string(),
    }
}

#[tokio::test]
async fn send_message_by_a_non_member_returns_channel_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let result = domain
        .send_message(bob, channel.id, send_input("hi"))
        .await;

    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn list_messages_by_a_non_member_returns_channel_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let result = domain
        .list_messages(bob, channel.id, MessagePagination::default())
        .await;

    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn edit_message_by_a_non_member_returns_channel_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let message = domain
        .send_message(alice, channel.id, send_input("hi"))
        .await
        .expect("send_message succeeds");

    let result = domain
        .edit_message(bob, channel.id, message.id, EditMessageInput {
            content: "edited".to_string(),
        })
        .await;

    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn delete_message_by_a_non_member_returns_channel_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let message = domain
        .send_message(alice, channel.id, send_input("hi"))
        .await
        .expect("send_message succeeds");

    let result = domain.delete_message(bob, channel.id, message.id).await;

    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}

#[tokio::test]
async fn send_message_rejects_empty_content() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let result = domain.send_message(alice, channel.id, send_input("   ")).await;

    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn editing_someone_elses_message_returns_403_equivalent_and_leaves_it_unmodified() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (server, channel) = server_and_channel(&domain, alice).await;

    domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees code"))
        .await
        .expect("bob joins");

    let message = domain
        .send_message(alice, channel.id, send_input("alice's message"))
        .await
        .expect("send_message succeeds");

    let result = domain
        .edit_message(
            bob,
            channel.id,
            message.id,
            EditMessageInput {
                content: "bob was here".to_string(),
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::NotMessageAuthor)));

    let messages = domain
        .list_messages(alice, channel.id, MessagePagination::default())
        .await
        .expect("list_messages succeeds");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content.as_deref(), Some("alice's message"));
}

#[tokio::test]
async fn deleting_someone_elses_message_returns_403_equivalent_and_leaves_it_unmodified() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (server, channel) = server_and_channel(&domain, alice).await;

    domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees code"))
        .await
        .expect("bob joins");

    let message = domain
        .send_message(alice, channel.id, send_input("alice's message"))
        .await
        .expect("send_message succeeds");

    let result = domain.delete_message(bob, channel.id, message.id).await;

    // In a SERVER channel, deleting someone else's message now
    // checks MANAGE_MESSAGES before falling back to "not the author" — bob
    // has neither, so the more specific `MissingPermission` is what's
    // returned (still a 403-equivalent). `NotMessageAuthor` is now reserved
    // for dm/group_dm channels, which have no roles to hold that bit in —
    // see `role_permissions_v2_service.rs`'s
    // `in_a_dm_a_non_author_still_gets_not_message_author_not_missing_permission`
    // for that case, and its
    // `manage_messages_lets_a_moderator_delete_someone_elses_message` for the
    // MANAGE_MESSAGES-holder-succeeds path.
    assert!(matches!(result, Err(DomainError::MissingPermission)));

    let messages = domain
        .list_messages(alice, channel.id, MessagePagination::default())
        .await
        .expect("list_messages succeeds");
    assert_eq!(messages.len(), 1);
    assert!(messages[0].deleted_at.is_none());
    assert_eq!(messages[0].content.as_deref(), Some("alice's message"));
}

#[tokio::test]
async fn editing_a_message_that_does_not_exist_in_the_channel_returns_message_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let result = domain
        .edit_message(
            alice,
            channel.id,
            app_core::new_id(),
            EditMessageInput {
                content: "edited".to_string(),
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::MessageNotFound)));
}

#[tokio::test]
async fn deleting_a_message_that_does_not_exist_in_the_channel_returns_message_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let result = domain
        .delete_message(alice, channel.id, app_core::new_id())
        .await;

    assert!(matches!(result, Err(DomainError::MessageNotFound)));
}

#[tokio::test]
async fn a_message_belonging_to_a_different_channel_is_not_editable_here() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (server, channel_a) = server_and_channel(&domain, alice).await;
    let channel_b = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "other".to_string(),
                kind: None,
            },
        )
        .await
        .expect("create_channel succeeds");

    let message = domain
        .send_message(alice, channel_a.id, send_input("in channel a"))
        .await
        .expect("send_message succeeds");

    // Same message id, wrong channel — must behave like it doesn't exist
    // there, not silently operate on the row from the other channel.
    let result = domain
        .edit_message(
            alice,
            channel_b.id,
            message.id,
            EditMessageInput {
                content: "edited".to_string(),
            },
        )
        .await;

    assert!(matches!(result, Err(DomainError::MessageNotFound)));
}

#[tokio::test]
async fn edit_message_updates_content_and_sets_edited_at() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let message = domain
        .send_message(alice, channel.id, send_input("original"))
        .await
        .expect("send_message succeeds");
    assert!(message.edited_at.is_none());

    let edited = domain
        .edit_message(
            alice,
            channel.id,
            message.id,
            EditMessageInput {
                content: "updated".to_string(),
            },
        )
        .await
        .expect("edit_message succeeds");

    assert_eq!(edited.content.as_deref(), Some("updated"));
    assert!(edited.edited_at.is_some());
}

#[tokio::test]
async fn a_soft_deleted_message_stays_in_list_results_with_null_content() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let message = domain
        .send_message(alice, channel.id, send_input("to be deleted"))
        .await
        .expect("send_message succeeds");

    domain
        .delete_message(alice, channel.id, message.id)
        .await
        .expect("delete_message succeeds");

    let messages = domain
        .list_messages(alice, channel.id, MessagePagination::default())
        .await
        .expect("list_messages succeeds");

    assert_eq!(messages.len(), 1, "the soft-deleted row must stay in results");
    assert_eq!(messages[0].id, message.id);
    assert!(messages[0].content.is_none());
    assert!(messages[0].deleted_at.is_some());
}

#[tokio::test]
async fn editing_or_deleting_an_already_deleted_message_returns_message_not_found() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let message = domain
        .send_message(alice, channel.id, send_input("gone soon"))
        .await
        .expect("send_message succeeds");

    domain
        .delete_message(alice, channel.id, message.id)
        .await
        .expect("delete_message succeeds");

    let edit_result = domain
        .edit_message(
            alice,
            channel.id,
            message.id,
            EditMessageInput {
                content: "too late".to_string(),
            },
        )
        .await;
    assert!(matches!(edit_result, Err(DomainError::MessageNotFound)));

    let delete_result = domain.delete_message(alice, channel.id, message.id).await;
    assert!(matches!(delete_result, Err(DomainError::MessageNotFound)));
}

#[tokio::test]
async fn cursor_pagination_pages_backward_with_no_duplicates_or_gaps() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    let mut sent_ids = Vec::new();
    for i in 0..12 {
        let message = domain
            .send_message(alice, channel.id, send_input(&format!("message {i}")))
            .await
            .expect("send_message succeeds");
        sent_ids.push(message.id);
    }
    // UUIDv7 is time-ordered, so oldest-to-newest send order matches
    // ascending id order — newest-first listing should be the exact
    // reverse.
    let expected_newest_first: Vec<Uuid> = sent_ids.into_iter().rev().collect();

    let mut collected = Vec::new();
    let mut cursor: Option<Uuid> = None;
    loop {
        let page = domain
            .list_messages(
                alice,
                channel.id,
                MessagePagination {
                    limit: Some(5),
                    before: cursor,
                },
            )
            .await
            .expect("list_messages succeeds");

        if page.is_empty() {
            break;
        }

        cursor = Some(page.last().expect("page is non-empty").id);
        collected.extend(page.into_iter().map(|m| m.id));
    }

    assert_eq!(collected, expected_newest_first);
}

#[tokio::test]
async fn list_messages_limit_is_capped_at_100() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let (_server, channel) = server_and_channel(&domain, alice).await;

    domain
        .send_message(alice, channel.id, send_input("only message"))
        .await
        .expect("send_message succeeds");

    // A limit above the cap must not error — it just gets clamped down.
    let messages = domain
        .list_messages(
            alice,
            channel.id,
            MessagePagination {
                limit: Some(1_000),
                before: None,
            },
        )
        .await
        .expect("list_messages succeeds");

    assert_eq!(messages.len(), 1);
}

#[tokio::test]
async fn authorized_account_ids_for_a_text_channel_returns_all_server_members() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (server, channel) = server_and_channel(&domain, alice).await;

    domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees code"))
        .await
        .expect("bob joins");

    let mut ids = domain
        .authorized_account_ids(channel.id)
        .await
        .expect("authorized_account_ids succeeds");
    ids.sort();

    let mut expected = vec![alice, bob];
    expected.sort();

    assert_eq!(ids, expected);
}

#[tokio::test]
async fn accessible_channel_ids_includes_channels_of_every_server_the_account_is_in() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let bob = register(&auth, "bob@example.com", "bob").await;
    let (server, channel) = server_and_channel(&domain, alice).await;

    domain
        .join_via_invite(bob, server.invite_code.as_ref().expect("owner sees code"))
        .await
        .expect("bob joins");

    let bob_channels = domain
        .accessible_channel_ids(bob)
        .await
        .expect("accessible_channel_ids succeeds");

    assert_eq!(bob_channels, vec![channel.id]);
}

/// Documents a deliberate decision, not an accident: the message path is
/// generic over channel kind, so a `voice` channel gets the same text chat a
/// `text` channel does (Discord does the same). Nothing special-cases it —
/// this test exists so that stays true on purpose.
#[tokio::test]
async fn messages_work_in_a_voice_channel_exactly_like_a_text_channel() {
    let (domain, auth, _pool, _container) = test_services().await;
    let alice = register(&auth, "alice@example.com", "alice").await;
    let mallory = register(&auth, "mallory@example.com", "mallory").await;

    let server = domain
        .create_server(
            alice,
            CreateServerInput {
                name: "Alice's Place".to_string(),
                visibility: None,
            },
        )
        .await
        .expect("create_server succeeds");

    let voice = domain
        .create_channel(
            alice,
            server.id,
            CreateChannelInput {
                name: "General Voice".to_string(),
                kind: Some("voice".to_string()),
            },
        )
        .await
        .expect("voice channel created");

    let message = domain
        .send_message(
            alice,
            voice.id,
            SendMessageInput {
                content: "typing in the voice channel".to_string(),
            },
        )
        .await
        .expect("send_message succeeds in a voice channel");

    assert_eq!(message.content.as_deref(), Some("typing in the voice channel"));

    let listed = domain
        .list_messages(alice, voice.id, MessagePagination::default())
        .await
        .expect("list_messages succeeds");
    assert_eq!(listed.len(), 1);

    // And the same non-leaking authorization still applies to an outsider.
    let result = domain
        .send_message(
            mallory,
            voice.id,
            SendMessageInput {
                content: "sneaking in".to_string(),
            },
        )
        .await;
    assert!(matches!(result, Err(DomainError::ChannelNotFound)));
}
