//! Every password HTTP flow shares the same bounded blocking executor.

use std::sync::mpsc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::web_flows::{field, get, post, sign_up, sign_up_form};
use super::{ORIGIN, harness};
use crate::web::Reject;

#[tokio::test]
async fn all_password_routes_refuse_work_when_workers_are_full() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    let settings = get(&h.app, "/settings", Some(&cookie)).await;
    let csrf = field(&settings.body, "csrf");
    let signup = sign_up_form(&h.app, "bob").await;
    let mut releases = vec![];
    let mut jobs = vec![];
    for _ in 0..2 {
        let app = h.app.clone();
        let (release, wait) = mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        jobs.push(tokio::spawn(async move {
            app.passwords
                .run(move || {
                    started.send(()).unwrap();
                    let _ = wait.recv();
                })
                .await
                .unwrap();
        }));
        tokio::time::timeout(Duration::from_secs(5), ready)
            .await
            .unwrap()
            .unwrap();
        releases.push(release);
    }
    let change =
        format!("csrf={csrf}&current=correct+horse+battery&new=a+different+password&confirm=a+different+password");
    for (path, cookie, form) in [
        ("/login", None, "username=unknown&password=wrong"),
        ("/admin/login", None, "password=operator+password"),
        ("/signup", None, signup.as_str()),
        ("/settings/password", Some(cookie.as_str()), change.as_str()),
    ] {
        let reply = post(&h.app, path, cookie, form, Some(ORIGIN)).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {}", reply.body);
    }
    assert!(h.wallet.db.account_by_username("bob").await.unwrap().is_none());
    assert_eq!(get(&h.app, "/wallet", Some(&cookie)).await.status, StatusCode::OK);
    assert!(
        h.app
            .passwords
            .metrics()
            .contains("satchel_password_jobs_rejected_total 4\n")
    );
    assert_eq!(Reject::Busy.into_response().headers()["retry-after"], "1");
    for release in releases {
        release.send(()).unwrap();
    }
    for job in jobs {
        job.await.unwrap();
    }
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
