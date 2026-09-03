use std::sync::Arc;

use auth::{AuthError, AuthService, LoginInput, RegisterInput, VerifyRegistrationInput};
use mailer::CaptureMailer;
use testcontainers_modules::{
    postgres::Postgres,
    testcontainers::{runners::AsyncRunner, ImageExt},
};

struct Harness {
    service: AuthService,
    mail: CaptureMailer,
    pool: db::PgPool,
    _container: testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
}

async fn harness() -> Harness {
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

    let mail = CaptureMailer::new();
    let service = AuthService::new(pool.clone(), Arc::new(mail.clone()));

    Harness {
        service,
        mail,
        pool,
        _container: container,
    }
}

fn register_input(email: &str, username: &str) -> RegisterInput {
    RegisterInput {
        email: email.to_string(),
        username: username.to_string(),
        password: "correct horse battery staple".to_string(),
        display_name: "Test User".to_string(),
    }
}

/// Pulls the code out of the most recent captured mail. Tests read it here
/// rather than from a return value because the service deliberately never
/// hands the code back to its caller — only the mailbox owner should have it.
fn last_code(mail: &CaptureMailer) -> String {
    let message = mail.last().expect("a verification mail was sent");
    message
        .body
        .split_whitespace()
        .find(|word| word.len() == 8 && word.chars().all(|c| c.is_ascii_digit()))
        .expect("the mail carries an 8-digit code")
        .to_string()
}

async fn pending_count(pool: &db::PgPool, email: &str) -> i64 {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM pending_registration WHERE email = $1")
            .bind(email)
            .fetch_one(pool)
            .await
            .expect("count query runs");
    count
}

async fn account_count(pool: &db::PgPool, email: &str) -> i64 {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM account WHERE email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .expect("count query runs");
    count
}

/// Registers and verifies in one step, for tests whose subject is something
/// other than the registration flow itself.
async fn register_and_verify(h: &Harness, email: &str, username: &str) -> auth::AccountSummary {
    h.service
        .register(register_input(email, username))
        .await
        .expect("registration starts");
    let code = last_code(&h.mail);
    h.service
        .verify_registration(VerifyRegistrationInput {
            email: email.to_string(),
            code,
        })
        .await
        .expect("verification succeeds")
}

#[tokio::test]
async fn register_creates_no_account_until_the_code_comes_back() {
    let h = harness().await;

    h.service
        .register(register_input("alice@example.com", "alice"))
        .await
        .expect("registration starts");

    // The whole point of requiring proof: an unproven address leaves no account.
    assert_eq!(account_count(&h.pool, "alice@example.com").await, 0);
    assert_eq!(pending_count(&h.pool, "alice@example.com").await, 1);

    // And no session can exist for an account that does not.
    let premature_login = h
        .service
        .login(LoginInput {
            email: "alice@example.com".to_string(),
            password: "correct horse battery staple".to_string(),
        })
        .await;
    assert!(matches!(premature_login, Err(AuthError::InvalidCredentials)));
}

#[tokio::test]
async fn verifying_promotes_the_registration_into_a_usable_account() {
    let h = harness().await;

    let account = register_and_verify(&h, "bob@example.com", "bob").await;

    assert_eq!(account.email, "bob@example.com");
    assert_eq!(account.username, "bob");
    assert!(
        account.email_verified_at.is_some(),
        "an account created by verification is verified by construction"
    );

    // The pending row is gone, not merely marked.
    assert_eq!(pending_count(&h.pool, "bob@example.com").await, 0);
    assert_eq!(account_count(&h.pool, "bob@example.com").await, 1);

    // The password captured at registration carried through to the account.
    let (session, token) = h
        .service
        .login(LoginInput {
            email: "bob@example.com".to_string(),
            password: "correct horse battery staple".to_string(),
        })
        .await
        .expect("login succeeds after verification");
    assert!(!token.is_empty());
    assert!(session.expires_at > session.created_at);
}

#[tokio::test]
async fn a_code_works_once() {
    let h = harness().await;

    h.service
        .register(register_input("carol@example.com", "carol"))
        .await
        .expect("registration starts");
    let code = last_code(&h.mail);

    h.service
        .verify_registration(VerifyRegistrationInput {
            email: "carol@example.com".to_string(),
            code: code.clone(),
        })
        .await
        .expect("first verification succeeds");

    let replay = h
        .service
        .verify_registration(VerifyRegistrationInput {
            email: "carol@example.com".to_string(),
            code,
        })
        .await;

    assert!(matches!(replay, Err(AuthError::InvalidVerificationCode)));
}

#[tokio::test]
async fn an_expired_code_is_refused() {
    let h = harness().await;

    h.service
        .register(register_input("dave@example.com", "dave"))
        .await
        .expect("registration starts");
    let code = last_code(&h.mail);

    // Reach into the row rather than sleeping fifteen minutes.
    sqlx::query(
        "UPDATE pending_registration SET expires_at = now() - interval '1 second' WHERE email = $1",
    )
    .bind("dave@example.com")
    .execute(&h.pool)
    .await
    .expect("expiry update runs");

    let result = h
        .service
        .verify_registration(VerifyRegistrationInput {
            email: "dave@example.com".to_string(),
            code,
        })
        .await;

    assert!(matches!(result, Err(AuthError::InvalidVerificationCode)));
    assert_eq!(account_count(&h.pool, "dave@example.com").await, 0);
}

#[tokio::test]
async fn the_fifth_wrong_guess_destroys_the_registration() {
    let h = harness().await;

    h.service
        .register(register_input("erin@example.com", "erin"))
        .await
        .expect("registration starts");
    let real_code = last_code(&h.mail);

    // Deliberately not the real code, and distinct each time. The generated
    // code is eight digits, so a "9999xxxx" guess can collide only in the
    // vanishingly unlikely case the real one starts with 9999 — and the
    // assertion below on the real code would catch that if it ever happened.
    let wrong = |n: u32| VerifyRegistrationInput {
        email: "erin@example.com".to_string(),
        code: format!("9999{n:04}"),
    };

    for attempt in 1..=4 {
        let result = h.service.verify_registration(wrong(attempt)).await;
        assert!(
            matches!(result, Err(AuthError::InvalidVerificationCode)),
            "guess {attempt} should be refused"
        );
        assert_eq!(
            pending_count(&h.pool, "erin@example.com").await,
            1,
            "the registration survives guess {attempt}"
        );
    }

    let fifth = h.service.verify_registration(wrong(5)).await;
    assert!(matches!(fifth, Err(AuthError::InvalidVerificationCode)));

    // Burned outright: leaving a dead row behind would keep the username
    // reserved for a registration that can never complete.
    assert_eq!(pending_count(&h.pool, "erin@example.com").await, 0);

    // Even the genuine code is worthless now.
    let with_real_code = h
        .service
        .verify_registration(VerifyRegistrationInput {
            email: "erin@example.com".to_string(),
            code: real_code,
        })
        .await;
    assert!(matches!(
        with_real_code,
        Err(AuthError::InvalidVerificationCode)
    ));
}

#[tokio::test]
async fn resending_invalidates_the_previous_code() {
    let h = harness().await;

    h.service
        .register(register_input("frank@example.com", "frank"))
        .await
        .expect("registration starts");
    let first_code = last_code(&h.mail);

    // The per-address cooldown withholds a second mail inside its window, so
    // wind the row back far enough for a resend to be allowed. This exercises
    // the cooldown rather than working around it: the next test asserts the
    // window holds when it has not passed.
    sqlx::query(
        "UPDATE pending_registration SET expires_at = now() + interval '5 minutes' WHERE email = $1",
    )
    .bind("frank@example.com")
    .execute(&h.pool)
    .await
    .expect("cooldown wind-back runs");

    h.service
        .resend_verification_code("frank@example.com")
        .await
        .expect("resend succeeds");
    let second_code = last_code(&h.mail);

    assert_ne!(first_code, second_code, "a resend issues a fresh code");

    let with_old = h
        .service
        .verify_registration(VerifyRegistrationInput {
            email: "frank@example.com".to_string(),
            code: first_code,
        })
        .await;
    assert!(matches!(with_old, Err(AuthError::InvalidVerificationCode)));

    h.service
        .verify_registration(VerifyRegistrationInput {
            email: "frank@example.com".to_string(),
            code: second_code,
        })
        .await
        .expect("the newest code still works");
}

#[tokio::test]
async fn resending_inside_the_cooldown_sends_nothing_and_keeps_the_live_code() {
    let h = harness().await;

    h.service
        .register(register_input("grace@example.com", "grace"))
        .await
        .expect("registration starts");
    let original_code = last_code(&h.mail);
    assert_eq!(h.mail.sent().len(), 1);

    // Hammering resend must not amplify into mail, or the endpoint is a spam
    // cannon pointable at any inbox.
    for _ in 0..5 {
        h.service
            .resend_verification_code("grace@example.com")
            .await
            .expect("resend is accepted");
    }
    assert_eq!(
        h.mail.sent().len(),
        1,
        "the cooldown withholds the extra mail"
    );

    // And it must not rotate the code either — otherwise an attacker could
    // invalidate a victim's in-flight code faster than they can type it.
    h.service
        .verify_registration(VerifyRegistrationInput {
            email: "grace@example.com".to_string(),
            code: original_code,
        })
        .await
        .expect("the original code still works");
}

#[tokio::test]
async fn registering_a_known_address_looks_identical_and_sends_no_mail() {
    let h = harness().await;

    register_and_verify(&h, "heidi@example.com", "heidi").await;
    h.mail.clear();

    // Same response as a fresh address: no error, nothing to distinguish the
    // two. Reporting the clash would make this an enumeration oracle.
    h.service
        .register(register_input("heidi@example.com", "heidi_two"))
        .await
        .expect("registration reports success either way");

    assert!(
        h.mail.sent().is_empty(),
        "no mail goes to an address that already has an account"
    );
    assert_eq!(
        pending_count(&h.pool, "heidi@example.com").await,
        0,
        "and no pending registration is created for it"
    );
}

#[tokio::test]
async fn a_pending_registration_reserves_its_username() {
    let h = harness().await;

    h.service
        .register(register_input("ivan@example.com", "shared_name"))
        .await
        .expect("first registration starts");

    let second = h
        .service
        .register(register_input("judy@example.com", "shared_name"))
        .await;

    // Usernames are public, so this one is reported plainly rather than
    // silently — the caller can pick another name instead of waiting for a
    // mail that would never arrive.
    assert!(matches!(second, Err(AuthError::UsernameTaken)));

    // And the first registration is untouched: claiming someone's pending
    // username must not destroy their signup.
    assert_eq!(pending_count(&h.pool, "ivan@example.com").await, 1);
    let code = last_code(&h.mail);
    h.service
        .verify_registration(VerifyRegistrationInput {
            email: "ivan@example.com".to_string(),
            code,
        })
        .await
        .expect("the original registration still completes");
}

#[tokio::test]
async fn losing_the_username_race_reports_it_distinctly() {
    let h = harness().await;

    // The reservation on `pending_registration.username` stops a second
    // *registration* claiming the name, but nothing stops an existing account
    // renaming into it: `update_account` only checks `account`. That is the
    // real path to this error, and it is why the promoting transaction
    // re-checks instead of trusting the reservation it took at the start.
    let lena = register_and_verify(&h, "lena@example.com", "lena").await;

    h.service
        .register(register_input("ken@example.com", "raced_name"))
        .await
        .expect("ken's registration starts");
    let ken_code = last_code(&h.mail);

    h.service
        .update_account(
            lena.id,
            auth::UpdateAccountInput {
                username: Some("raced_name".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("lena renames into the reserved name");

    // Ken proved his address correctly and still cannot have the name. That
    // is a different situation from picking a taken name up front, and the
    // error says so rather than surfacing a raw constraint violation.
    let ken_result = h
        .service
        .verify_registration(VerifyRegistrationInput {
            email: "ken@example.com".to_string(),
            code: ken_code,
        })
        .await;

    assert!(matches!(
        ken_result,
        Err(AuthError::UsernameTakenDuringVerification)
    ));

    // And no half-built account was left behind by the failed promotion.
    assert_eq!(account_count(&h.pool, "ken@example.com").await, 0);
}

#[tokio::test]
async fn purging_removes_only_expired_registrations() {
    let h = harness().await;

    h.service
        .register(register_input("mallory@example.com", "mallory"))
        .await
        .expect("live registration starts");
    h.service
        .register(register_input("niaj@example.com", "niaj"))
        .await
        .expect("second registration starts");

    sqlx::query(
        "UPDATE pending_registration SET expires_at = now() - interval '1 minute' WHERE email = $1",
    )
    .bind("niaj@example.com")
    .execute(&h.pool)
    .await
    .expect("expiry update runs");

    let purged = h
        .service
        .purge_expired_registrations()
        .await
        .expect("purge runs");

    assert_eq!(purged, 1);
    assert_eq!(pending_count(&h.pool, "mallory@example.com").await, 1);
    assert_eq!(pending_count(&h.pool, "niaj@example.com").await, 0);
}

#[tokio::test]
async fn an_abandoned_registration_does_not_strand_its_username() {
    let h = harness().await;

    h.service
        .register(register_input("olivia@example.com", "shared"))
        .await
        .expect("first registration starts");

    // Olivia walks away and her window closes. The availability check ignores
    // expired rows, so the name reads as free — and the INSERT has to agree,
    // or the check is a liar and the name is stranded until someone runs SQL
    // by hand. Purging on a timer would narrow that window; it would not close
    // it, because a row can expire between the check and the insert.
    sqlx::query(
        "UPDATE pending_registration SET expires_at = now() - interval '1 minute' WHERE email = $1",
    )
    .bind("olivia@example.com")
    .execute(&h.pool)
    .await
    .expect("expiry update runs");

    h.service
        .register(register_input("peggy@example.com", "shared"))
        .await
        .expect("the abandoned name is available again");

    assert_eq!(pending_count(&h.pool, "olivia@example.com").await, 0);
    assert_eq!(pending_count(&h.pool, "peggy@example.com").await, 1);
}

#[tokio::test]
async fn an_expired_row_never_lets_anyone_clear_a_live_registration() {
    let h = harness().await;

    h.service
        .register(register_input("quentin@example.com", "contested"))
        .await
        .expect("live registration starts");

    // Same name, still in flight. Sweeping expired rows by username must not
    // become a way to evict a stranger who got there first.
    let result = h
        .service
        .register(register_input("rupert@example.com", "contested"))
        .await;

    assert!(matches!(result, Err(AuthError::UsernameTaken)));
    assert_eq!(pending_count(&h.pool, "quentin@example.com").await, 1);
    assert_eq!(pending_count(&h.pool, "rupert@example.com").await, 0);
}

#[tokio::test]
async fn verifying_an_address_nobody_registered_is_refused_like_a_wrong_code() {
    let h = harness().await;

    let result = h
        .service
        .verify_registration(VerifyRegistrationInput {
            email: "nobody@example.com".to_string(),
            code: "12345678".to_string(),
        })
        .await;

    // Same error as a wrong code on purpose: otherwise this distinguishes
    // "nobody is registering that address" from "wrong code".
    assert!(matches!(result, Err(AuthError::InvalidVerificationCode)));
}

#[tokio::test]
async fn login_with_wrong_password_and_nonexistent_email_both_fail_the_same_way() {
    let h = harness().await;

    register_and_verify(&h, "olivia@example.com", "olivia").await;

    let wrong_password = h
        .service
        .login(LoginInput {
            email: "olivia@example.com".to_string(),
            password: "totally wrong password".to_string(),
        })
        .await;
    assert!(matches!(wrong_password, Err(AuthError::InvalidCredentials)));

    let nonexistent_email = h
        .service
        .login(LoginInput {
            email: "nobody@example.com".to_string(),
            password: "correct horse battery staple".to_string(),
        })
        .await;
    assert!(matches!(
        nonexistent_email,
        Err(AuthError::InvalidCredentials)
    ));
}

#[tokio::test]
async fn fifth_concurrent_login_is_rejected_with_session_limit_reached() {
    let h = harness().await;

    register_and_verify(&h, "peggy@example.com", "peggy").await;

    let login_input = || LoginInput {
        email: "peggy@example.com".to_string(),
        password: "correct horse battery staple".to_string(),
    };

    for attempt in 1..=4 {
        h.service
            .login(login_input())
            .await
            .unwrap_or_else(|err| panic!("login {attempt} should succeed: {err}"));
    }

    let fifth = h.service.login(login_input()).await;
    assert!(matches!(fifth, Err(AuthError::SessionLimitReached)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_logins_never_exceed_the_session_cap() {
    let h = harness().await;

    register_and_verify(&h, "ivan2@example.com", "ivan2").await;

    let login_input = || LoginInput {
        email: "ivan2@example.com".to_string(),
        password: "correct horse battery staple".to_string(),
    };

    // Fire more concurrent login attempts than the cap allows (but modest
    // enough not to starve the pool's own acquire timeout when the whole
    // workspace test suite is running several testcontainers at once).
    // Without the `SELECT ... FOR UPDATE` row lock in `login`, this is a
    // TOCTOU race: several attempts can read the same pre-insert
    // active-session count and all pass the check, pushing the account
    // above the cap.
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let service = h.service.clone();
            let input = login_input();
            tokio::spawn(async move { service.login(input).await })
        })
        .collect();

    let mut succeeded = 0;
    let mut limited = 0;
    for handle in handles {
        match handle.await.expect("login task does not panic") {
            Ok(_) => succeeded += 1,
            Err(AuthError::SessionLimitReached) => limited += 1,
            Err(other) => panic!("unexpected login error: {other:?}"),
        }
    }

    assert_eq!(
        succeeded, 4,
        "exactly MAX_CONCURRENT_SESSIONS logins should succeed under concurrency"
    );
    assert_eq!(limited, 2);
}

#[tokio::test]
async fn verify_session_accepts_a_fresh_token_and_rejects_revoked_or_unknown_ones() {
    let h = harness().await;

    register_and_verify(&h, "quentin@example.com", "quentin").await;

    let (session, raw_token) = h
        .service
        .login(LoginInput {
            email: "quentin@example.com".to_string(),
            password: "correct horse battery staple".to_string(),
        })
        .await
        .expect("login succeeds");

    let context = h
        .service
        .verify_session(&raw_token)
        .await
        .expect("fresh token verifies");
    assert_eq!(context.session_id, session.id);

    h.service
        .revoke_session(context.account_id, session.id)
        .await
        .expect("revoke succeeds");

    let revoked_result = h.service.verify_session(&raw_token).await;
    assert!(matches!(revoked_result, Err(AuthError::Unauthenticated)));

    let unknown_result = h.service.verify_session("not-a-real-token").await;
    assert!(matches!(unknown_result, Err(AuthError::Unauthenticated)));
}

#[tokio::test]
async fn revoke_session_does_not_let_one_account_revoke_another_accounts_session() {
    let h = harness().await;

    let grace = register_and_verify(&h, "grace2@example.com", "grace2").await;
    let heidi = register_and_verify(&h, "heidi2@example.com", "heidi2").await;

    let (grace_session, grace_raw_token) = h
        .service
        .login(LoginInput {
            email: "grace2@example.com".to_string(),
            password: "correct horse battery staple".to_string(),
        })
        .await
        .expect("grace logs in");

    // heidi attempts to revoke grace's session by id.
    let cross_account_result = h.service.revoke_session(heidi.id, grace_session.id).await;
    assert!(matches!(
        cross_account_result,
        Err(AuthError::SessionNotFound)
    ));

    // grace's session must be untouched by the attempted cross-account revoke.
    assert!(h.service.verify_session(&grace_raw_token).await.is_ok());

    // grace can revoke her own session, sanity-checking the happy path used
    // as the counter-example above.
    h.service
        .revoke_session(grace.id, grace_session.id)
        .await
        .expect("grace revokes her own session");
    assert!(matches!(
        h.service.verify_session(&grace_raw_token).await,
        Err(AuthError::Unauthenticated)
    ));
}
