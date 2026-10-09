//! Every password HTTP flow shares the same bounded workers, and a request
//! refused because they are all busy spends no attempt and no proof of work.

use axum::http::StatusCode;

use super::web_flows::{Reply, field, get, post, sign_up, sign_up_form};
use super::{ORIGIN, harness, harness_with};
use crate::auth;
use crate::password_work::PasswordPermit;
use crate::web::Shared;

/// Takes every password worker, as long-running password jobs would.
fn occupy_workers(app: &Shared) -> Vec<PasswordPermit> {
    std::iter::from_fn(|| app.passwords.admit().ok()).collect()
}

/// The form posted to `action`, shown again with the busy message.
fn assert_busy_form(reply: &Reply, action: &str) {
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{action}");
    assert_eq!(reply.retry_after.as_deref(), Some("1"), "{action}");
    assert!(reply.body.contains(&format!("action=\"{action}\"")), "{action}");
    assert!(
        reply.body.contains("The wallet is busy. Try again in a moment."),
        "{action}"
    );
}

#[tokio::test]
async fn every_password_form_is_shown_again_while_workers_are_busy() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    let settings = get(&h.app, "/settings", Some(&cookie)).await;
    let csrf = field(&settings.body, "csrf");
    let signup = sign_up_form(&h.app, "bob").await;
    let workers = occupy_workers(&h.app);
    let change =
        format!("csrf={csrf}&current=correct+horse+battery&new=a+different+password&confirm=a+different+password");
    for (path, cookie, form) in [
        ("/login", None, "username=unknown&password=wrong"),
        ("/admin/login", None, "password=operator+password"),
        ("/signup", None, signup.as_str()),
        ("/settings/password", Some(cookie.as_str()), change.as_str()),
    ] {
        let reply = post(&h.app, path, cookie, form, Some(ORIGIN)).await;
        assert_busy_form(&reply, path);
    }
    assert!(h.wallet.db.account_by_username("bob").await.unwrap().is_none());
    assert_eq!(get(&h.app, "/wallet", Some(&cookie)).await.status, StatusCode::OK);
    // The admission that found every worker taken, then the four forms.
    assert!(
        h.app
            .passwords
            .metrics()
            .contains("satchel_password_jobs_rejected_total 5\n")
    );
    drop(workers);
    let login = post(
        &h.app,
        "/login",
        None,
        "username=alice&password=correct+horse+battery",
        Some(ORIGIN),
    )
    .await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    assert!(login.cookie.is_some());
}

#[tokio::test]
async fn busy_signup_keeps_its_challenge_and_the_networks_signup_allowance() {
    let h = harness_with(|config| config.rate_limits.signup_per_ip_per_hour = 1).await;
    let form = sign_up_form(&h.app, "bob").await;
    let workers = occupy_workers(&h.app);
    let busy = post(&h.app, "/signup", None, &form, Some(ORIGIN)).await;
    assert_busy_form(&busy, "/signup");
    assert!(busy.body.contains("value=\"bob\""), "{}", busy.body);
    assert!(!busy.body.contains("correct horse battery"));
    assert!(h.wallet.db.account_by_username("bob").await.unwrap().is_none());
    drop(workers);
    // The same solved challenge and the network's only sign-up this hour.
    let created = post(&h.app, "/signup", None, &form, Some(ORIGIN)).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    assert!(h.wallet.db.account_by_username("bob").await.unwrap().is_some());
    let form = sign_up_form(&h.app, "carol").await;
    let over = post(&h.app, "/signup", None, &form, Some(ORIGIN)).await;
    assert!(over.body.contains("Too many new wallets"), "{}", over.body);
}

#[tokio::test]
async fn busy_logins_spend_no_login_attempt() {
    let h = harness_with(|config| {
        config.rate_limits.login_per_ip_per_minute = 1;
        config.rate_limits.login_per_account_per_hour = 1;
    })
    .await;
    sign_up(&h.app, "alice").await;
    let form = "username=alice&password=correct+horse+battery&next=%2Fsettings";
    let operator = "password=operator+password";
    let workers = occupy_workers(&h.app);
    let busy = post(&h.app, "/login", None, form, Some(ORIGIN)).await;
    assert_busy_form(&busy, "/login");
    assert!(busy.body.contains("value=\"alice\""), "{}", busy.body);
    assert_eq!(field(&busy.body, "next"), "/settings");
    assert!(!busy.body.contains("correct horse battery"));
    let busy = post(&h.app, "/admin/login", None, operator, Some(ORIGIN)).await;
    assert_busy_form(&busy, "/admin/login");
    assert!(!busy.body.contains("operator password"));
    drop(workers);
    // Each limit allows one attempt, which the busy refusals left unused.
    let login = post(&h.app, "/login", None, form, Some(ORIGIN)).await;
    assert_eq!(login.status, StatusCode::SEE_OTHER, "{}", login.body);
    assert_eq!(login.location.as_deref(), Some("/settings"));
    let operator_login = post(&h.app, "/admin/login", None, operator, Some(ORIGIN)).await;
    assert_eq!(operator_login.status, StatusCode::SEE_OTHER, "{}", operator_login.body);
    let over = post(&h.app, "/login", None, form, Some(ORIGIN)).await;
    assert!(over.body.contains("Too many login attempts"), "{}", over.body);
}

#[tokio::test]
async fn a_password_change_spends_an_attempt_only_when_it_checks_a_password() {
    let h = harness_with(|config| config.rate_limits.login_per_account_per_hour = 2).await;
    let cookie = sign_up(&h.app, "alice").await;
    let csrf = field(&get(&h.app, "/settings", Some(&cookie)).await.body, "csrf");
    let change = |current: &str, new: &str| format!("csrf={csrf}&current={current}&new={new}&confirm={new}");
    let short = change("correct+horse+battery", "short");
    let refused = post(&h.app, "/settings/password", Some(&cookie), &short, Some(ORIGIN)).await;
    assert!(refused.body.contains("Use at least 10 characters"), "{}", refused.body);
    let valid = change("correct+horse+battery", "a+different+password");
    let workers = occupy_workers(&h.app);
    let busy = post(&h.app, "/settings/password", Some(&cookie), &valid, Some(ORIGIN)).await;
    assert_busy_form(&busy, "/settings/password");
    assert!(!busy.body.contains("a different password"));
    drop(workers);
    // The two attempts the limit allows go to the two password checks.
    let wrong = change("not+the+password", "a+different+password");
    let refused = post(&h.app, "/settings/password", Some(&cookie), &wrong, Some(ORIGIN)).await;
    assert!(
        refused.body.contains("The current password is wrong."),
        "{}",
        refused.body
    );
    let saved = post(&h.app, "/settings/password", Some(&cookie), &valid, Some(ORIGIN)).await;
    assert!(saved.body.contains("Password saved."), "{}", saved.body);
    let over = post(&h.app, "/settings/password", Some(&cookie), &valid, Some(ORIGIN)).await;
    assert!(over.body.contains("Too many attempts."), "{}", over.body);
    let account = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    let hash = account.password_hash.unwrap();
    assert!(auth::verify_password(&hash, "a different password"));
    assert!(!auth::verify_password(&hash, "correct horse battery"));
}
