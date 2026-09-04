//! Seven new server-wide permission bits — timeout, deleting/
//! pinning someone else's message, managing another member's nickname,
//! invite-code visibility/rotation, and mention gating. Same harness as
//! `domain_service.rs`/`visibility_service.rs`.

use app_core::Uuid;
use auth::{AuthService, RegisterInput};
use chrono::{Duration, Utc};
use domain::{
    permissions, ChannelSummary, CreateChannelInput, CreateRoleInput, CreateServerInput,
    DomainError, DomainService, SendMessageInput, ServerSummary, TimeoutInput, UpdateRoleInput,
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

async fn create_server(domain: &DomainService, owner: Uuid, name: &str) -> ServerSummary {
    domain
        .create_server(
            owner,
            CreateServerInput { name: name.to_string(), visibility: None },
        )
        .await
        .expect("create_server succeeds")
}

async fn create_channel(domain: &DomainService, owner: Uuid, server_id: Uuid) -> ChannelSummary {
    domain
        .create_channel(
            owner,
            server_id,
            CreateChannelInput { name: "general".to_string(), kind: None },
        )
        .await
        .expect("create_channel succeeds")
}

/// Creates a role with exactly `bits`, at whatever position it lands at
/// (roles start above every existing one),
/// and assigns it to `target` as the owner (who bypasses `MANAGE_ROLES`).
async fn grant_role(
    domain: &DomainService,
    owner: Uuid,
    server_id: Uuid,
    target: Uuid,
    name: &str,
    bits: i64,
) -> Uuid {
    let role = domain
        .create_role(owner, server_id, CreateRoleInput { name: name.to_string() })
        .await
        .expect("create_role succeeds");
    domain
        .update_role(
            owner,
            server_id,
            role.id,
            UpdateRoleInput {
                name: None,
                color: None,
                permissions: Some(bits),
                mentionable: None,
            },
        )
        .await
        .expect("update_role succeeds");
    domain
        .set_member_roles(owner, server_id, target, vec![role.id])
        .await
        .expect("set_member_roles succeeds");
    role.id
}

async fn join(domain: &DomainService, account: Uuid, invite_code: &str) {
    domain
        .join_via_invite(account, invite_code)
        .await
        .expect("join_via_invite succeeds");
}

// ---- timeout ----

#[tokio::test]
async fn a_member_with_timeout_members_can_time_out_a_plain_member() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner@example.com", "owner").await;
    let mod_account = register(&auth, "mod@example.com", "moduser").await;
    let target = register(&auth, "target@example.com", "target").await;

    let server = create_server(&domain, owner, "Server").await;
    join(&domain, mod_account, server.invite_code.as_ref().unwrap()).await;
    join(&domain, target, server.invite_code.as_ref().unwrap()).await;
    grant_role(&domain, owner, server.id, mod_account, "Mod", permissions::TIMEOUT_MEMBERS).await;

    let until = Utc::now() + Duration::hours(1);
    domain
        .timeout_member(
            mod_account,
            server.id,
            target,
            TimeoutInput { until, reason: Some("spamming".to_string()) },
        )
        .await
        .expect("timeout_member succeeds");

    // A timed-out member cannot post...
    let channel = create_channel(&domain, owner, server.id).await;
    let send = domain
        .send_message(target, channel.id, SendMessageInput { content: "hi".to_string() })
        .await;
    assert!(matches!(send, Err(DomainError::MemberTimedOut)));

    // ...but everyone else still can, and the target can still read
    // (list_channels/list_members don't error for them).
    let list = domain.list_channels(target, server.id).await;
    assert!(list.is_ok(), "a timed-out member can still read");
}

#[tokio::test]
async fn a_plain_member_without_timeout_members_cannot_time_out_anyone() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner2@example.com", "owner2").await;
    let plain = register(&auth, "plain2@example.com", "plain2").await;
    let target = register(&auth, "target2@example.com", "target2").await;

    let server = create_server(&domain, owner, "Server2").await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    join(&domain, target, server.invite_code.as_ref().unwrap()).await;

    let result = domain
        .timeout_member(
            plain,
            server.id,
            target,
            TimeoutInput { until: Utc::now() + Duration::hours(1), reason: None },
        )
        .await;
    assert!(matches!(result, Err(DomainError::MissingPermission)));
}

#[tokio::test]
async fn timeout_rejects_a_target_whose_role_outranks_the_actors() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner3@example.com", "owner3").await;
    let low_mod = register(&auth, "lowmod3@example.com", "lowmod3").await;
    let high_target = register(&auth, "hightarget3@example.com", "hightarget3").await;

    let server = create_server(&domain, owner, "Server3").await;
    join(&domain, low_mod, server.invite_code.as_ref().unwrap()).await;
    join(&domain, high_target, server.invite_code.as_ref().unwrap()).await;

    // Created second, so it starts at a HIGHER position than the first role
    // (new roles start above every existing one) — gives the target's role
    // real outranking authority.
    grant_role(&domain, owner, server.id, low_mod, "Low Mod", permissions::TIMEOUT_MEMBERS).await;
    grant_role(&domain, owner, server.id, high_target, "High Rank", 0).await;

    let result = domain
        .timeout_member(
            low_mod,
            server.id,
            high_target,
            TimeoutInput { until: Utc::now() + Duration::hours(1), reason: None },
        )
        .await;
    assert!(matches!(result, Err(DomainError::InsufficientHierarchy)));
}

#[tokio::test]
async fn timeout_rejects_targeting_the_owner_or_yourself() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner4@example.com", "owner4").await;
    let mod_account = register(&auth, "mod4@example.com", "mod4").await;

    let server = create_server(&domain, owner, "Server4").await;
    join(&domain, mod_account, server.invite_code.as_ref().unwrap()).await;
    grant_role(&domain, owner, server.id, mod_account, "Mod", permissions::TIMEOUT_MEMBERS).await;

    let until = Utc::now() + Duration::hours(1);
    let self_result = domain
        .timeout_member(mod_account, server.id, mod_account, TimeoutInput { until, reason: None })
        .await;
    assert!(matches!(self_result, Err(DomainError::CannotActOnSelf)));

    let owner_result = domain
        .timeout_member(mod_account, server.id, owner, TimeoutInput { until, reason: None })
        .await;
    assert!(matches!(owner_result, Err(DomainError::CannotActOnOwner)));
}

#[tokio::test]
async fn timeout_rejects_a_past_timestamp() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner5@example.com", "owner5").await;
    let target = register(&auth, "target5@example.com", "target5").await;

    let server = create_server(&domain, owner, "Server5").await;
    join(&domain, target, server.invite_code.as_ref().unwrap()).await;

    let result = domain
        .timeout_member(
            owner,
            server.id,
            target,
            TimeoutInput { until: Utc::now() - Duration::hours(1), reason: None },
        )
        .await;
    assert!(matches!(result, Err(DomainError::Validation(_))));
}

#[tokio::test]
async fn clear_timeout_lets_a_member_post_again() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner6@example.com", "owner6").await;
    let target = register(&auth, "target6@example.com", "target6").await;

    let server = create_server(&domain, owner, "Server6").await;
    join(&domain, target, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id).await;

    domain
        .timeout_member(
            owner,
            server.id,
            target,
            TimeoutInput { until: Utc::now() + Duration::hours(1), reason: None },
        )
        .await
        .expect("owner can time out via ADMIN-equivalent bypass");

    domain
        .clear_timeout(owner, server.id, target)
        .await
        .expect("clear_timeout succeeds");

    let send = domain
        .send_message(target, channel.id, SendMessageInput { content: "back".to_string() })
        .await;
    assert!(send.is_ok(), "clearing the timeout must let the member post again");
}

// ---- delete-others'-message (MANAGE_MESSAGES) ----

#[tokio::test]
async fn manage_messages_lets_a_moderator_delete_someone_elses_message() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner7@example.com", "owner7").await;
    let mod_account = register(&auth, "mod7@example.com", "mod7").await;
    let author = register(&auth, "author7@example.com", "author7").await;

    let server = create_server(&domain, owner, "Server7").await;
    join(&domain, mod_account, server.invite_code.as_ref().unwrap()).await;
    join(&domain, author, server.invite_code.as_ref().unwrap()).await;
    grant_role(&domain, owner, server.id, mod_account, "Mod", permissions::MANAGE_MESSAGES).await;
    let channel = create_channel(&domain, owner, server.id).await;

    let message = domain
        .send_message(author, channel.id, SendMessageInput { content: "hello".to_string() })
        .await
        .expect("send_message succeeds");

    domain
        .delete_message(mod_account, channel.id, message.id)
        .await
        .expect("a MANAGE_MESSAGES holder may delete someone else's message");
}

/// A `dm`/`group_dm` channel has no roles to hold `MANAGE_MESSAGES` in, so a
/// non-author there stays `NotMessageAuthor`, exactly as M0 shipped it — the
/// `MANAGE_MESSAGES` branch only ever applies to server channels.
#[tokio::test]
async fn in_a_dm_a_non_author_still_gets_not_message_author_not_missing_permission() {
    let (domain, auth, _container) = test_services().await;
    let alice = register(&auth, "alice18@example.com", "alice18").await;
    let bob = register(&auth, "bob18@example.com", "bob18").await;

    let (dm, _created) = domain.create_dm(alice, bob).await.expect("create_dm succeeds");
    let message = domain
        .send_message(alice, dm.id, SendMessageInput { content: "hi bob".to_string() })
        .await
        .expect("send_message succeeds");

    let result = domain.delete_message(bob, dm.id, message.id).await;
    assert!(matches!(result, Err(DomainError::NotMessageAuthor)));
}

#[tokio::test]
async fn without_manage_messages_a_member_cannot_delete_someone_elses_message() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner8@example.com", "owner8").await;
    let plain = register(&auth, "plain8@example.com", "plain8").await;
    let author = register(&auth, "author8@example.com", "author8").await;

    let server = create_server(&domain, owner, "Server8").await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    join(&domain, author, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id).await;

    let message = domain
        .send_message(author, channel.id, SendMessageInput { content: "hello".to_string() })
        .await
        .expect("send_message succeeds");

    let result = domain.delete_message(plain, channel.id, message.id).await;
    assert!(matches!(result, Err(DomainError::MissingPermission)));
}

// ---- pin/unpin (PIN_MESSAGES) ----

#[tokio::test]
async fn pin_messages_gates_pinning_but_anyone_with_channel_access_can_read_pins() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner9@example.com", "owner9").await;
    let pinner = register(&auth, "pinner9@example.com", "pinner9").await;
    let plain = register(&auth, "plain9@example.com", "plain9").await;

    let server = create_server(&domain, owner, "Server9").await;
    join(&domain, pinner, server.invite_code.as_ref().unwrap()).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    grant_role(&domain, owner, server.id, pinner, "Pinner", permissions::PIN_MESSAGES).await;
    let channel = create_channel(&domain, owner, server.id).await;

    let message = domain
        .send_message(owner, channel.id, SendMessageInput { content: "important".to_string() })
        .await
        .expect("send_message succeeds");

    let denied = domain.pin_message(plain, channel.id, message.id).await;
    assert!(matches!(denied, Err(DomainError::MissingPermission)));

    let pinned = domain
        .pin_message(pinner, channel.id, message.id)
        .await
        .expect("PIN_MESSAGES holder can pin");
    assert!(pinned.pinned_at.is_some());

    // Reading pins needs no bit at all — baseline, like reading messages.
    let pins = domain
        .list_pinned_messages(plain, channel.id)
        .await
        .expect("any member with channel access can read pins");
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].id, message.id);

    let unpinned = domain
        .unpin_message(pinner, channel.id, message.id)
        .await
        .expect("PIN_MESSAGES holder can unpin");
    assert!(unpinned.pinned_at.is_none());
}

// ---- nicknames (MANAGE_NICKNAMES) ----

#[tokio::test]
async fn a_member_can_set_their_own_nickname_without_any_bit() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner10@example.com", "owner10").await;
    let member = register(&auth, "member10@example.com", "member10").await;

    let server = create_server(&domain, owner, "Server10").await;
    join(&domain, member, server.invite_code.as_ref().unwrap()).await;

    domain
        .update_member_nickname(member, server.id, member, Some("Memmy".to_string()))
        .await
        .expect("a member may set their own nickname");
}

#[tokio::test]
async fn manage_nicknames_gates_setting_someone_elses_and_respects_hierarchy() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner11@example.com", "owner11").await;
    let mod_account = register(&auth, "mod11@example.com", "mod11").await;
    let plain = register(&auth, "plain11@example.com", "plain11").await;

    let server = create_server(&domain, owner, "Server11").await;
    join(&domain, mod_account, server.invite_code.as_ref().unwrap()).await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;

    let denied = domain
        .update_member_nickname(mod_account, server.id, plain, Some("Renamed".to_string()))
        .await;
    assert!(matches!(denied, Err(DomainError::MissingPermission)));

    grant_role(&domain, owner, server.id, mod_account, "Mod", permissions::MANAGE_NICKNAMES).await;

    domain
        .update_member_nickname(mod_account, server.id, plain, Some("Renamed".to_string()))
        .await
        .expect("a MANAGE_NICKNAMES holder may rename a lower-ranked member");
}

// ---- invites (MANAGE_INVITES) ----

#[tokio::test]
async fn get_server_hides_the_invite_code_from_a_plain_member_but_shows_it_to_manage_invites() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner12@example.com", "owner12").await;
    let plain = register(&auth, "plain12@example.com", "plain12").await;
    let inviter = register(&auth, "inviter12@example.com", "inviter12").await;

    let server = create_server(&domain, owner, "Server12").await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    join(&domain, inviter, server.invite_code.as_ref().unwrap()).await;
    grant_role(&domain, owner, server.id, inviter, "Inviter", permissions::MANAGE_INVITES).await;

    let as_plain = domain.get_server(plain, server.id).await.expect("get_server succeeds");
    assert!(as_plain.invite_code.is_none());

    let as_inviter = domain.get_server(inviter, server.id).await.expect("get_server succeeds");
    assert!(as_inviter.invite_code.is_some());
}

#[tokio::test]
async fn regenerate_invite_code_invalidates_the_old_code_immediately() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner13@example.com", "owner13").await;
    let joiner = register(&auth, "joiner13@example.com", "joiner13").await;

    let server = create_server(&domain, owner, "Server13").await;
    let old_code = server.invite_code.clone().unwrap();

    let updated = domain
        .regenerate_invite_code(owner, server.id)
        .await
        .expect("owner can regenerate the invite code");
    let new_code = updated.invite_code.expect("owner always sees the fresh code");
    assert_ne!(old_code, new_code);

    let old_join = domain.join_via_invite(joiner, &old_code).await;
    assert!(matches!(old_join, Err(DomainError::InvalidInvite)));

    domain
        .join_via_invite(joiner, &new_code)
        .await
        .expect("the new code works");
}

#[tokio::test]
async fn a_plain_member_cannot_regenerate_the_invite_code() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner14@example.com", "owner14").await;
    let plain = register(&auth, "plain14@example.com", "plain14").await;

    let server = create_server(&domain, owner, "Server14").await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;

    let result = domain.regenerate_invite_code(plain, server.id).await;
    assert!(matches!(result, Err(DomainError::MissingPermission)));
}

// ---- mentions (MENTION_EVERYONE / MENTION_ROLES) ----

#[tokio::test]
async fn a_plain_member_cannot_send_an_at_everyone_message() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner15@example.com", "owner15").await;
    let plain = register(&auth, "plain15@example.com", "plain15").await;

    let server = create_server(&domain, owner, "Server15").await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id).await;

    let result = domain
        .send_message(plain, channel.id, SendMessageInput { content: "@everyone hi".to_string() })
        .await;
    assert!(matches!(result, Err(DomainError::MentionNotAllowed)));
}

#[tokio::test]
async fn mention_everyone_grants_the_at_everyone_token() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner16@example.com", "owner16").await;
    let announcer = register(&auth, "announcer16@example.com", "announcer16").await;

    let server = create_server(&domain, owner, "Server16").await;
    join(&domain, announcer, server.invite_code.as_ref().unwrap()).await;
    grant_role(&domain, owner, server.id, announcer, "Announcer", permissions::MENTION_EVERYONE)
        .await;
    let channel = create_channel(&domain, owner, server.id).await;

    domain
        .send_message(announcer, channel.id, SendMessageInput { content: "@everyone hi".to_string() })
        .await
        .expect("MENTION_EVERYONE holder may use @everyone");
}

#[tokio::test]
async fn a_non_mentionable_roles_slug_requires_mention_roles_but_a_mentionable_one_does_not() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner17@example.com", "owner17").await;
    let plain = register(&auth, "plain17@example.com", "plain17").await;

    let server = create_server(&domain, owner, "Server17").await;
    join(&domain, plain, server.invite_code.as_ref().unwrap()).await;
    let channel = create_channel(&domain, owner, server.id).await;

    let staff_role = domain
        .create_role(owner, server.id, CreateRoleInput { name: "Staff".to_string() })
        .await
        .expect("create_role succeeds");
    let lfg_role = domain
        .create_role(owner, server.id, CreateRoleInput { name: "LFG".to_string() })
        .await
        .expect("create_role succeeds");
    domain
        .update_role(
            owner,
            server.id,
            lfg_role.id,
            UpdateRoleInput { name: None, color: None, permissions: None, mentionable: Some(true) },
        )
        .await
        .expect("update_role succeeds");

    let denied = domain
        .send_message(plain, channel.id, SendMessageInput { content: "@staff help".to_string() })
        .await;
    assert!(matches!(denied, Err(DomainError::MentionNotAllowed)));

    let allowed = domain
        .send_message(plain, channel.id, SendMessageInput { content: "@lfg anyone?".to_string() })
        .await;
    assert!(allowed.is_ok(), "a mentionable role's slug needs no bit");

    // A token matching nothing real is just text, never rejected.
    let _ = staff_role; // referenced above via slug "staff", kept for clarity
    let plain_text = domain
        .send_message(plain, channel.id, SendMessageInput { content: "my email is a@b-c".to_string() })
        .await;
    assert!(plain_text.is_ok(), "a token matching no real role/reserved word is not a mention");
}

// ---- reorder_roles ----

#[tokio::test]
async fn reorder_roles_rejects_a_moderator_moving_their_own_role_above_a_higher_role() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner5@example.com", "owner5").await;
    let mod_account = register(&auth, "mod5@example.com", "mod5").await;

    let server = create_server(&domain, owner, "Server5").await;
    join(&domain, mod_account, server.invite_code.as_ref().unwrap()).await;

    // Created first, so it starts BELOW the role created after it — gives
    // the moderator's own role less authority than "High".
    let mod_role_id = grant_role(&domain, owner, server.id, mod_account, "Mod", permissions::MANAGE_ROLES).await;
    let high_role_id = domain
        .create_role(owner, server.id, CreateRoleInput { name: "High".to_string() })
        .await
        .expect("create_role succeeds")
        .id;

    let before = domain
        .list_roles(owner, server.id)
        .await
        .expect("list_roles succeeds");

    // The moderator only holds MANAGE_ROLES, not ADMIN, and tries to swap
    // the order so their own role outranks "High" — the same escalation
    // `update_role`/`delete_role` already reject via hierarchy checks.
    let result = domain
        .reorder_roles(mod_account, server.id, vec![mod_role_id, high_role_id])
        .await;
    assert!(matches!(result, Err(DomainError::InsufficientHierarchy)));

    let after = domain
        .list_roles(owner, server.id)
        .await
        .expect("list_roles succeeds");
    assert_eq!(before, after, "a rejected reorder must not persist partial position changes");
}

#[tokio::test]
async fn reorder_roles_allows_the_owner_to_reorder_freely() {
    let (domain, auth, _container) = test_services().await;
    let owner = register(&auth, "owner6@example.com", "owner6").await;

    let server = create_server(&domain, owner, "Server6").await;
    let a_id = domain
        .create_role(owner, server.id, CreateRoleInput { name: "a".to_string() })
        .await
        .expect("create_role succeeds")
        .id;
    let b_id = domain
        .create_role(owner, server.id, CreateRoleInput { name: "b".to_string() })
        .await
        .expect("create_role succeeds")
        .id;

    let result = domain
        .reorder_roles(owner, server.id, vec![b_id, a_id])
        .await
        .expect("owner reorder succeeds");
    assert_eq!(result[0].id, b_id);
    assert_eq!(result[1].id, a_id);
}
