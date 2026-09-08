use auth::{AuthError, AuthService, RegisterInput, UpdateAccountInput};
use test_support::TestDb;

async fn test_service() -> (AuthService, TestDb) {
    let test_db = test_support::test_db().await;
    let pool = test_db.pool();

    (AuthService::new(pool, std::sync::Arc::new(mailer::CaptureMailer::new())), test_db)
}

fn register_input(email: &str, username: &str) -> RegisterInput {
    RegisterInput {
        email: email.to_string(),
        username: username.to_string(),
        password: "correct horse battery staple".to_string(),
        display_name: "Test User".to_string(),
    }
}

#[tokio::test]
async fn get_account_returns_the_full_summary_for_an_existing_account() {
    let (service, _container) = test_service().await;

    let registered = service
        .create_verified_account(register_input("alice@example.com", "alice"))
        .await
        .expect("registration succeeds");

    let fetched = service
        .get_account(registered.id)
        .await
        .expect("get_account succeeds");

    assert_eq!(fetched, registered);
}

#[tokio::test]
async fn get_account_returns_account_not_found_for_an_unknown_id() {
    let (service, _container) = test_service().await;

    let result = service.get_account(app_core::new_id()).await;

    assert!(matches!(result, Err(AuthError::AccountNotFound)));
}

#[tokio::test]
async fn update_account_with_all_none_is_a_harmless_no_op() {
    let (service, _container) = test_service().await;

    let registered = service
        .create_verified_account(register_input("bob@example.com", "bob"))
        .await
        .expect("registration succeeds");

    let updated = service
        .update_account(registered.id, UpdateAccountInput::default())
        .await
        .expect("no-op update succeeds");

    assert_eq!(updated, registered);
}

#[tokio::test]
async fn update_account_changes_only_the_requested_field() {
    let (service, _container) = test_service().await;

    let registered = service
        .create_verified_account(register_input("carol@example.com", "carol"))
        .await
        .expect("registration succeeds");

    let updated = service
        .update_account(
            registered.id,
            UpdateAccountInput {
                display_name: Some("Carol Danvers".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("update succeeds");

    assert_eq!(updated.display_name, "Carol Danvers");
    // Untouched fields keep their original values.
    assert_eq!(updated.username, registered.username);
    assert_eq!(updated.avatar_url, registered.avatar_url);
    assert_eq!(updated.email, registered.email);
}

#[tokio::test]
async fn update_account_can_set_username_and_avatar_url_together() {
    let (service, _container) = test_service().await;

    let registered = service
        .create_verified_account(register_input("dave@example.com", "dave"))
        .await
        .expect("registration succeeds");

    let updated = service
        .update_account(
            registered.id,
            UpdateAccountInput {
                username: Some("dave_new".to_string()),
                avatar_url: Some(Some("https://example.com/avatar.png".to_string())),
                ..Default::default()
            },
        )
        .await
        .expect("update succeeds");

    assert_eq!(updated.username, "dave_new");
    assert_eq!(
        updated.avatar_url,
        Some("https://example.com/avatar.png".to_string())
    );
    assert_eq!(updated.display_name, registered.display_name);
}

#[tokio::test]
async fn update_account_rejects_an_invalid_username_without_writing_anything() {
    let (service, _container) = test_service().await;

    let registered = service
        .create_verified_account(register_input("erin@example.com", "erin"))
        .await
        .expect("registration succeeds");

    let result = service
        .update_account(
            registered.id,
            UpdateAccountInput {
                username: Some("no".to_string()), // too short
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(result, Err(AuthError::Validation(_))));

    let unchanged = service
        .get_account(registered.id)
        .await
        .expect("get_account succeeds");
    assert_eq!(unchanged, registered);
}

#[tokio::test]
async fn update_account_rejects_a_username_already_taken_by_another_account() {
    let (service, _container) = test_service().await;

    let frank = service
        .create_verified_account(register_input("frank@example.com", "frank"))
        .await
        .expect("frank registers");
    let grace = service
        .create_verified_account(register_input("grace@example.com", "grace"))
        .await
        .expect("grace registers");

    let result = service
        .update_account(
            grace.id,
            UpdateAccountInput {
                username: Some("frank".to_string()),
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(result, Err(AuthError::UsernameTaken)));

    // The failed update must not partially apply — grace's row is untouched.
    let grace_unchanged = service
        .get_account(grace.id)
        .await
        .expect("get_account succeeds");
    assert_eq!(grace_unchanged, grace);

    // frank's row is untouched too.
    let frank_unchanged = service
        .get_account(frank.id)
        .await
        .expect("get_account succeeds");
    assert_eq!(frank_unchanged, frank);
}

#[tokio::test]
async fn update_account_only_touches_the_caller_supplied_account_id() {
    let (service, _container) = test_service().await;

    let heidi = service
        .create_verified_account(register_input("heidi@example.com", "heidi"))
        .await
        .expect("heidi registers");
    let ivan = service
        .create_verified_account(register_input("ivan@example.com", "ivan"))
        .await
        .expect("ivan registers");

    service
        .update_account(
            heidi.id,
            UpdateAccountInput {
                display_name: Some("Heidi Updated".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("heidi's update succeeds");

    let ivan_unchanged = service
        .get_account(ivan.id)
        .await
        .expect("get_account succeeds");
    assert_eq!(ivan_unchanged, ivan);
}

// --- Profile fields (migration 0004) ---------------------------------------

#[tokio::test]
async fn a_new_account_starts_with_every_profile_field_empty() {
    let (service, _container) = test_service().await;

    let account = service
        .create_verified_account(register_input("fresh@example.com", "fresh"))
        .await
        .expect("registration succeeds");

    assert_eq!(account.bio, None);
    assert_eq!(account.banner_url, None);
    assert_eq!(account.accent_color, None);
    assert_eq!(account.pronouns, None);
}

#[tokio::test]
async fn profile_fields_can_be_set_and_read_back() {
    let (service, _container) = test_service().await;

    let account = service
        .create_verified_account(register_input("set@example.com", "setter"))
        .await
        .expect("registration succeeds");

    let updated = service
        .update_account(
            account.id,
            UpdateAccountInput {
                bio: Some(Some("construyo cosas".to_string())),
                accent_color: Some(Some("#ffa800".to_string())),
                pronouns: Some(Some("él/ella".to_string())),
                ..Default::default()
            },
        )
        .await
        .expect("update succeeds");

    assert_eq!(updated.bio.as_deref(), Some("construyo cosas"));
    assert_eq!(updated.accent_color.as_deref(), Some("#ffa800"));
    assert_eq!(updated.pronouns.as_deref(), Some("él/ella"));
}

/// The distinction the whole `Option<Option<_>>` shape exists for. An absent
/// field must leave the column alone, and an explicit null must clear it.
/// Under the old COALESCE update both did the same thing, and a bio could be
/// set but never removed.
#[tokio::test]
async fn an_absent_field_is_left_alone_but_an_explicit_null_clears_it() {
    let (service, _container) = test_service().await;

    let account = service
        .create_verified_account(register_input("clear@example.com", "clearer"))
        .await
        .expect("registration succeeds");

    service
        .update_account(
            account.id,
            UpdateAccountInput {
                bio: Some(Some("temporal".to_string())),
                pronouns: Some(Some("she/her".to_string())),
                ..Default::default()
            },
        )
        .await
        .expect("update succeeds");

    // Touch only display_name: bio and pronouns are absent, so they survive.
    let after_unrelated = service
        .update_account(
            account.id,
            UpdateAccountInput {
                display_name: Some("Nombre Nuevo".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("update succeeds");

    assert_eq!(after_unrelated.bio.as_deref(), Some("temporal"));
    assert_eq!(after_unrelated.pronouns.as_deref(), Some("she/her"));

    // Now clear the bio explicitly, and only the bio.
    let after_clear = service
        .update_account(
            account.id,
            UpdateAccountInput {
                bio: Some(None),
                ..Default::default()
            },
        )
        .await
        .expect("update succeeds");

    assert_eq!(after_clear.bio, None, "an explicit null must clear the bio");
    assert_eq!(
        after_clear.pronouns.as_deref(),
        Some("she/her"),
        "clearing one field must not disturb another"
    );
}

#[tokio::test]
async fn an_invalid_accent_color_is_rejected_without_writing_anything() {
    let (service, _container) = test_service().await;

    let account = service
        .create_verified_account(register_input("colour@example.com", "colourist"))
        .await
        .expect("registration succeeds");

    let result = service
        .update_account(
            account.id,
            UpdateAccountInput {
                display_name: Some("Se escribe igual".to_string()),
                accent_color: Some(Some("orange".to_string())),
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(result, Err(AuthError::Validation(_))), "got: {result:?}");

    // Validation runs before the UPDATE, so the valid field in the same
    // request must not have been written either.
    let unchanged = service.get_account(account.id).await.expect("fetch succeeds");
    assert_eq!(unchanged.display_name, "Test User");
    assert_eq!(unchanged.accent_color, None);
}

#[tokio::test]
async fn invalid_pronouns_are_rejected() {
    let (service, _container) = test_service().await;

    let account = service
        .create_verified_account(register_input("pron@example.com", "pronouns"))
        .await
        .expect("registration succeeds");

    for bad in ["she", "she/her/hers", "their/theirs", "12/34"] {
        let result = service
            .update_account(
                account.id,
                UpdateAccountInput {
                    pronouns: Some(Some(bad.to_string())),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(result, Err(AuthError::Validation(_))), "{bad} should be rejected");
    }
}

/// The service validates first, but the database is the backstop. If the
/// service check were ever bypassed or weakened, migration 0004's CHECK
/// constraints still refuse the row.
#[tokio::test]
async fn a_bio_past_the_limit_is_rejected() {
    let (service, _container) = test_service().await;

    let account = service
        .create_verified_account(register_input("long@example.com", "verbose"))
        .await
        .expect("registration succeeds");

    let result = service
        .update_account(
            account.id,
            UpdateAccountInput {
                bio: Some(Some("a".repeat(191))),
                ..Default::default()
            },
        )
        .await;

    assert!(matches!(result, Err(AuthError::Validation(_))), "got: {result:?}");
}
