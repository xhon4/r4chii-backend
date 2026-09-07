use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};

/// A migrated database in a fresh container.
async fn test_pool() -> (
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

    (pool, container)
}

#[tokio::test]
async fn migrations_create_core_tables() {
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

    let expected_tables = [
        "account",
        "session",
        "server",
        "channel",
        "message",
        "friendship",
        "block",
        "account_profile_link",
    ];

    for table in expected_tables {
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = $1)",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|err| panic!("failed to check table {table}: {err}"));

        assert!(exists.0, "expected table `{table}` to exist after migrations");
    }

    let profile_columns: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'account' \
         AND (column_name, udt_name) IN ( \
             ('status', 'presence_status'), \
             ('custom_status', 'text'), \
             ('custom_emoji', 'text'), \
             ('custom_expires_at', 'timestamptz'), \
             ('theme', 'jsonb'), \
             ('vis_bio', 'visibility'), \
             ('vis_communities', 'visibility'), \
             ('vis_friends', 'visibility'), \
             ('deleted_at', 'timestamptz') \
         )",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or_else(|err| panic!("failed to inspect account profile columns: {err}"));

    assert_eq!(profile_columns.0, 9, "expected all account profile columns");

    let link_id_has_default: (bool,) = sqlx::query_as(
        "SELECT EXISTS ( \
             SELECT 1 FROM pg_attrdef default_value \
             JOIN pg_attribute attribute \
               ON attribute.attrelid = default_value.adrelid \
              AND attribute.attnum = default_value.adnum \
             WHERE default_value.adrelid = 'account_profile_link'::regclass \
               AND attribute.attname = 'id' \
         )",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or_else(|err| panic!("failed to inspect account_profile_link id default: {err}"));

    assert!(
        !link_id_has_default.0,
        "profile link IDs must be generated in Rust"
    );

    sqlx::query(
        "INSERT INTO account (id, username, email, display_name) \
         VALUES ( \
             '00000000-0000-0000-0000-000000000017', \
             'profile-migration-test', \
             'profile-migration-test@example.com', \
             'Profile Migration Test' \
         )",
    )
    .execute(&pool)
    .await
    .unwrap_or_else(|err| panic!("failed to create profile migration account: {err}"));

    let defaults: (String, String, String, String, String) = sqlx::query_as(
        "SELECT status::text, theme::text, vis_bio::text, vis_communities::text, vis_friends::text \
         FROM account WHERE username = 'profile-migration-test'",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or_else(|err| panic!("failed to read account profile defaults: {err}"));

    assert_eq!(
        defaults,
        (
            "online".to_owned(),
            "{}".to_owned(),
            "public".to_owned(),
            "friends".to_owned(),
            "friends".to_owned(),
        ),
        "new accounts must receive the profile column defaults"
    );

    sqlx::query(
        "INSERT INTO account_profile_link (id, account_id, label, url, position) \
         VALUES ( \
             '00000000-0000-0000-0000-000000000018', \
             '00000000-0000-0000-0000-000000000017', \
             'Personal site', \
             'https://example.com', \
             0 \
         )",
    )
    .execute(&pool)
    .await
    .unwrap_or_else(|err| panic!("failed to create profile link: {err}"));

    let duplicate_position = sqlx::query(
        "INSERT INTO account_profile_link (id, account_id, label, url, position) \
         VALUES ( \
             '00000000-0000-0000-0000-000000000019', \
             '00000000-0000-0000-0000-000000000017', \
             'Second link', \
             'https://example.org', \
             0 \
         )",
    )
    .execute(&pool)
    .await;

    assert!(
        duplicate_position.is_err(),
        "profile links must be unique per account position"
    );

    let invalid_custom_status = sqlx::query(
        "INSERT INTO account (id, username, email, display_name, custom_status) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("00000000-0000-0000-0000-000000000020")
    .bind("profile-status-limit-test")
    .bind("profile-status-limit-test@example.com")
    .bind("Profile Status Limit Test")
    .bind("x".repeat(129))
    .execute(&pool)
    .await;

    assert!(
        invalid_custom_status.is_err(),
        "custom statuses longer than 128 characters must be rejected"
    );
}

#[tokio::test]
async fn a_username_identifies_an_account_regardless_of_case_or_compatibility_form() {
    let (pool, _container) = test_pool().await;

    sqlx::query(
        "INSERT INTO account (id, username, email, display_name) VALUES \
         ('00000000-0000-0000-0000-000000000018', 'CaseTest', \
          'case@example.com', 'Case Test')",
    )
    .execute(&pool)
    .await
    .expect("account inserts");

    let normalized: (String,) =
        sqlx::query_as("SELECT username_normalized FROM account WHERE username = 'CaseTest'")
            .fetch_one(&pool)
            .await
            .expect("generated column is readable");
    assert_eq!(
        normalized.0, "casetest",
        "the generated column folds case without touching the display value"
    );

    for (index, clash) in ["casetest", "CASETEST", "ＣａｓｅＴｅｓｔ"].iter().enumerate() {
        let result = sqlx::query(
            "INSERT INTO account (id, username, email, display_name) \
             VALUES ($1, $2, $3, 'Clash')",
        )
        .bind(app_core::new_id())
        .bind(clash)
        .bind(format!("clash{index}@example.com"))
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "`{clash}` must collide with the existing account"
        );
    }
}

#[tokio::test]
async fn a_custom_emoji_is_bounded_in_bytes() {
    let (pool, _container) = test_pool().await;

    sqlx::query(
        "INSERT INTO account (id, username, email, display_name) VALUES \
         ('00000000-0000-0000-0000-000000000019', 'emoji-bound', \
          'emoji@example.com', 'Emoji Bound')",
    )
    .execute(&pool)
    .await
    .expect("account inserts");

    // A four-person family with skin tone modifiers: 41 bytes, and one
    // character to a reader.
    let widest = "👨🏻\u{200D}👩🏽\u{200D}👧🏾\u{200D}👦🏿";
    assert!(widest.len() <= 64, "the ceiling must clear a real sequence");
    sqlx::query("UPDATE account SET custom_emoji = $1 WHERE username = 'emoji-bound'")
        .bind(widest)
        .execute(&pool)
        .await
        .expect("a real emoji sequence fits");

    let result = sqlx::query("UPDATE account SET custom_emoji = $1 WHERE username = 'emoji-bound'")
        .bind("a".repeat(65))
        .execute(&pool)
        .await;
    assert!(result.is_err(), "past the ceiling the check must reject");
}
