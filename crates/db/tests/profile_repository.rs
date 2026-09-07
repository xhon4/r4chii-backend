use app_core::{new_id, Uuid};
use chrono::{DateTime, Utc};
use db::profile::{get_profiles_bulk, get_server_context, replace_links, ProfileLinkInput};
use sqlx::PgPool;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};

async fn test_pool() -> (
    PgPool,
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
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

    (pool, container)
}

async fn insert_account(pool: &PgPool, username: &str) -> Uuid {
    let id = new_id();
    sqlx::query("INSERT INTO account (id, username, email, display_name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(username)
        .bind(format!("{username}@example.com"))
        .bind(format!("{username} display"))
        .execute(pool)
        .await
        .expect("account inserts");
    id
}

/// `replace_links` takes the caller's connection, so the transaction boundary
/// belongs to the caller. These tests open one per replacement; dropping it on
/// an error is what rolls the replacement back.
async fn replace_links_in_transaction(
    pool: &PgPool,
    account_id: Uuid,
    links: &[ProfileLinkInput],
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    replace_links(&mut transaction, account_id, links).await?;
    transaction.commit().await
}

#[tokio::test]
async fn bulk_profiles_reads_a2_fields_and_returns_profiles_without_links() {
    let (pool, _container) = test_pool().await;
    let account_id = insert_account(&pool, "profile-empty").await;
    let expires_at: DateTime<Utc> = "2030-01-02T03:04:05Z".parse().expect("valid timestamp");

    sqlx::query(
        "UPDATE account SET avatar_url = $2, bio = $3, banner_url = $4, accent_color = $5, \
         pronouns = $6, status = 'dnd', custom_status = $7, custom_emoji = $8, \
         custom_expires_at = $9, theme = $10::jsonb, vis_bio = 'private', \
         vis_communities = 'public', vis_friends = 'private', deleted_at = $11 WHERE id = $1",
    )
    .bind(account_id)
    .bind("https://cdn.example/avatar.png")
    .bind("A profile bio")
    .bind("https://cdn.example/banner.png")
    .bind("#A1B2C3")
    .bind("she/her")
    .bind("Away")
    .bind("☕")
    .bind(expires_at)
    .bind(r#"{"layout":"default"}"#)
    .bind(expires_at)
    .execute(&pool)
    .await
    .expect("profile fields update");

    let profiles = get_profiles_bulk(&pool, &[account_id])
        .await
        .expect("bulk profile read succeeds");

    assert_eq!(profiles.len(), 1);
    let profile = &profiles[0];
    assert_eq!(profile.id, account_id);
    assert_eq!(
        profile.avatar_url.as_deref(),
        Some("https://cdn.example/avatar.png")
    );
    assert_eq!(profile.bio.as_deref(), Some("A profile bio"));
    assert_eq!(
        profile.banner_url.as_deref(),
        Some("https://cdn.example/banner.png")
    );
    assert_eq!(profile.accent_color.as_deref(), Some("#A1B2C3"));
    assert_eq!(profile.pronouns.as_deref(), Some("she/her"));
    assert_eq!(profile.status, "dnd");
    assert_eq!(profile.custom_status.as_deref(), Some("Away"));
    assert_eq!(profile.custom_emoji.as_deref(), Some("☕"));
    assert_eq!(profile.custom_expires_at, Some(expires_at));
    assert_eq!(profile.theme, r#"{"layout": "default"}"#);
    assert_eq!(profile.vis_bio, "private");
    assert_eq!(profile.vis_communities, "public");
    assert_eq!(profile.vis_friends, "private");
    assert_eq!(profile.deleted_at, Some(expires_at));
    assert!(profile.links.is_empty());
}

#[tokio::test]
async fn bulk_profiles_returns_links_in_position_order() {
    let (pool, _container) = test_pool().await;
    let account_id = insert_account(&pool, "profile-links").await;

    replace_links_in_transaction(
        &pool,
        account_id,
        &[
            ProfileLinkInput::new("Second", "https://second.example"),
            ProfileLinkInput::new("First", "https://first.example"),
        ],
    )
    .await
    .expect("links replace");

    let profiles = get_profiles_bulk(&pool, &[account_id])
        .await
        .expect("bulk profile read succeeds");
    let links = &profiles[0].links;
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].position, 0);
    assert_eq!(links[0].label, "Second");
    assert_eq!(links[1].position, 1);
    assert_eq!(links[1].label, "First");
}

#[tokio::test]
async fn replace_links_completely_replaces_the_existing_set() {
    let (pool, _container) = test_pool().await;
    let account_id = insert_account(&pool, "profile-replace").await;

    replace_links_in_transaction(
        &pool,
        account_id,
        &[ProfileLinkInput::new("Old", "https://old.example")],
    )
    .await
    .expect("initial links replace");
    replace_links_in_transaction(
        &pool,
        account_id,
        &[
            ProfileLinkInput::new("New one", "https://new-one.example"),
            ProfileLinkInput::new("New two", "https://new-two.example"),
        ],
    )
    .await
    .expect("replacement links replace");

    let profiles = get_profiles_bulk(&pool, &[account_id])
        .await
        .expect("bulk profile read succeeds");
    let links = &profiles[0].links;
    assert_eq!(links.len(), 2);
    assert_eq!(links[0].label, "New one");
    assert_eq!(links[1].label, "New two");
    assert!(links.iter().all(|link| link.id.get_version_num() == 7));
    assert_ne!(links[0].id, links[1].id);

    replace_links_in_transaction(&pool, account_id, &[])
        .await
        .expect("empty replacement clears links");
    let profiles = get_profiles_bulk(&pool, &[account_id])
        .await
        .expect("bulk profile read succeeds");
    assert!(profiles[0].links.is_empty());
}

#[tokio::test]
async fn replace_links_rejects_an_unknown_account_for_empty_and_non_empty_replacements() {
    let (pool, _container) = test_pool().await;
    let account_id = new_id();

    for links in [
        vec![],
        vec![ProfileLinkInput::new("Link", "https://link.example")],
    ] {
        let error = replace_links_in_transaction(&pool, account_id, &links)
            .await
            .expect_err("unknown accounts are rejected before replacing links");
        assert!(matches!(error, sqlx::Error::RowNotFound));
    }
}

#[tokio::test]
async fn replace_links_rolls_back_when_a_new_link_violates_a_constraint() {
    let (pool, _container) = test_pool().await;
    let account_id = insert_account(&pool, "profile-atomic").await;

    replace_links_in_transaction(
        &pool,
        account_id,
        &[ProfileLinkInput::new("Kept", "https://kept.example")],
    )
    .await
    .expect("initial links replace");

    let result = replace_links_in_transaction(
        &pool,
        account_id,
        &[ProfileLinkInput::new("", "https://invalid.example")],
    )
    .await;
    assert!(
        result.is_err(),
        "the database constraint rejects empty labels"
    );

    let profiles = get_profiles_bulk(&pool, &[account_id])
        .await
        .expect("bulk profile read succeeds");
    let links = &profiles[0].links;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].label, "Kept");
}

#[tokio::test]
async fn bulk_profiles_returns_empty_for_empty_input_and_omits_unknown_ids() {
    let (pool, _container) = test_pool().await;
    let account_id = insert_account(&pool, "profile-known").await;
    let unknown_id = new_id();

    assert!(get_profiles_bulk(&pool, &[])
        .await
        .expect("empty bulk profile read succeeds")
        .is_empty());

    let profiles = get_profiles_bulk(&pool, &[account_id, unknown_id, account_id])
        .await
        .expect("bulk profile read succeeds");
    let ids: Vec<_> = profiles.into_iter().map(|profile| profile.id).collect();
    assert_eq!(ids, vec![account_id, account_id]);
}

#[tokio::test]
async fn bulk_profiles_returns_several_requested_accounts_in_one_operation() {
    let (pool, _container) = test_pool().await;
    let first = insert_account(&pool, "profile-bulk-first").await;
    let second = insert_account(&pool, "profile-bulk-second").await;
    let third = insert_account(&pool, "profile-bulk-third").await;

    let profiles = get_profiles_bulk(&pool, &[third, first, second])
        .await
        .expect("one bulk profile operation succeeds");

    let ids: Vec<_> = profiles.into_iter().map(|profile| profile.id).collect();
    assert_eq!(ids, vec![third, first, second]);
}

#[tokio::test]
async fn server_context_returns_none_when_the_account_has_no_membership() {
    let (pool, _container) = test_pool().await;
    let owner_id = insert_account(&pool, "context-no-membership-owner").await;
    let account_id = insert_account(&pool, "context-no-membership-account").await;
    let server_id = new_id();

    sqlx::query(
        "INSERT INTO server (id, owner_account_id, name, visibility, invite_code) \
         VALUES ($1, $2, 'No membership server', 'private', 'no-membership-invite')",
    )
    .bind(server_id)
    .bind(owner_id)
    .execute(&pool)
    .await
    .expect("server inserts");

    let context = get_server_context(&pool, server_id, account_id)
        .await
        .expect("server context read succeeds");
    assert!(context.is_none());
}

#[tokio::test]
async fn server_context_returns_membership_nickname_and_effective_roles() {
    let (pool, _container) = test_pool().await;
    let owner_id = insert_account(&pool, "context-owner").await;
    let account_id = insert_account(&pool, "context-member").await;
    let server_id = new_id();
    let membership_id = new_id();
    let default_role_id = new_id();
    let moderator_role_id = new_id();

    sqlx::query(
        "INSERT INTO server (id, owner_account_id, name, visibility, invite_code) \
         VALUES ($1, $2, 'Context server', 'private', 'context-invite')",
    )
    .bind(server_id)
    .bind(owner_id)
    .execute(&pool)
    .await
    .expect("server inserts");
    sqlx::query(
        "INSERT INTO membership (id, server_id, account_id, nickname, role) \
         VALUES ($1, $2, $3, 'Context nickname', 'member')",
    )
    .bind(membership_id)
    .bind(server_id)
    .bind(account_id)
    .execute(&pool)
    .await
    .expect("membership inserts");
    sqlx::query(
        "INSERT INTO server_role (id, server_id, name, position, is_default) \
         VALUES ($1, $2, 'everyone', 0, true), ($3, $2, 'Moderator', 5, false)",
    )
    .bind(default_role_id)
    .bind(server_id)
    .bind(moderator_role_id)
    .execute(&pool)
    .await
    .expect("roles insert");
    sqlx::query("INSERT INTO membership_role (membership_id, role_id) VALUES ($1, $2)")
        .bind(membership_id)
        .bind(moderator_role_id)
        .execute(&pool)
        .await
        .expect("role assignment inserts");

    let context = get_server_context(&pool, server_id, account_id)
        .await
        .expect("server context read succeeds")
        .expect("member has server context");

    assert_eq!(context.nickname.as_deref(), Some("Context nickname"));
    assert_eq!(context.roles[0].id, moderator_role_id);
    assert_eq!(context.roles[0].name, "Moderator");
    assert!(context.roles.iter().any(|role| role.id == default_role_id));
}
