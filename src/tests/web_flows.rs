//! Browser flows over HTTP: sign-up, login, CSRF and origin checks, the
//! wallet page, Nostr login, the operator pages, and security headers.

use axum::body::{Body, to_bytes};
use axum::http::header::{CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE};
use axum::http::{Request, Response, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{ORIGIN as SITE, harness, harness_with};
use crate::nostr::tests::signed_event;
use crate::pow;
use crate::util::now;
use crate::web::{self, Shared};

pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) location: Option<String>,
    pub(super) cookie: Option<String>,
    pub(super) body: String,
}

pub(super) async fn read(response: Response<Body>) -> Reply {
    let status = response.status();
    let header = |name| {
        response
            .headers()
            .get(name)
            .map(|value: &axum::http::HeaderValue| value.to_str().unwrap().to_owned())
    };
    let location = header(LOCATION);
    let cookie = header(SET_COOKIE).map(|cookie| cookie.split(';').next().unwrap().to_owned());
    let body = to_bytes(response.into_body(), 1 << 22).await.unwrap();
    Reply {
        status,
        location,
        cookie,
        body: String::from_utf8(body.to_vec()).unwrap(),
    }
}

pub(super) async fn get(app: &Shared, uri: &str, cookie: Option<&str>) -> Reply {
    let mut request = Request::get(uri);
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    read(
        web::router(app.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await
}

pub(super) async fn post(app: &Shared, uri: &str, cookie: Option<&str>, form: &str, origin: Option<&str>) -> Reply {
    let mut request = Request::post(uri).header(CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    if let Some(origin) = origin {
        request = request.header(ORIGIN, origin);
    }
    read(
        web::router(app.clone())
            .oneshot(request.body(Body::from(form.to_owned())).unwrap())
            .await
            .unwrap(),
    )
    .await
}

pub(super) async fn post_json(app: &Shared, uri: &str, body: &Value) -> Reply {
    let request = Request::post(uri)
        .header(CONTENT_TYPE, "application/json")
        .header(ORIGIN, SITE)
        .body(Body::from(body.to_string()))
        .unwrap();
    read(web::router(app.clone()).oneshot(request).await.unwrap()).await
}

/// The value of the first `name="<field>" value="..."` in a page.
pub(super) fn field(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = html.find(&marker).unwrap_or_else(|| panic!("no {name} field")) + marker.len();
    html[start..].split('"').next().unwrap().to_owned()
}

/// A proof-of-work challenge from the server and the nonce that solves it.
pub(super) async fn solved_pow(app: &Shared) -> (String, u64) {
    let reply = post_json(app, "/auth/pow", &json!({})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let issued: Value = serde_json::from_str(&reply.body).unwrap();
    let challenge = issued["challenge"].as_str().unwrap().to_owned();
    let nonce = pow::solve(&challenge);
    (challenge, nonce)
}

/// The sign-up form's fields, with a solved proof of work.
pub(super) async fn sign_up_form(app: &Shared, username: &str) -> String {
    let (challenge, nonce) = solved_pow(app).await;
    format!(
        "username={username}&password=correct+horse+battery&confirm=correct+horse+battery\
         &pow_challenge={challenge}&pow_nonce={nonce}"
    )
}

pub(super) async fn sign_up(app: &Shared, username: &str) -> String {
    let form = sign_up_form(app, username).await;
    let reply = post(app, "/signup", None, &form, Some(SITE)).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location.as_deref(), Some("/wallet"));
    let cookie = reply.cookie.unwrap();
    assert!(cookie.starts_with("__Host-session="), "{cookie}");
    cookie
}

#[tokio::test]
async fn every_page_warns_that_this_is_a_test_network() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    for (uri, cookie) in [
        ("/", None),
        ("/login", None),
        ("/signup", None),
        ("/no-such-page", None),
        ("/admin/login", None),
        ("/wallet", Some(cookie.as_str())),
        ("/settings", Some(cookie.as_str())),
    ] {
        let reply = get(&h.app, uri, cookie).await;
        assert!(
            reply.body.contains("Test network only (signet)"),
            "{uri} has no banner: {}",
            reply.status
        );
    }
}

#[tokio::test]
async fn deployment_name_is_display_only_and_recovery_is_optional() {
    let default = harness().await;
    assert!(
        !get(&default.app, "/", None)
            .await
            .body
            .contains("Recover 5day4cast entries")
    );
    let h = harness_with(|config| {
        config.server.network_name = Some("Mutinynet".into());
        config.server.recovery_url = Some("/recover/".into());
    })
    .await;
    let cookie = sign_up(&h.app, "alice").await;
    for (uri, cookie) in [
        ("/", None),
        ("/login", None),
        ("/wallet", Some(cookie.as_str())),
        ("/settings", Some(cookie.as_str())),
    ] {
        let page = get(&h.app, uri, cookie).await;
        assert!(page.body.contains("Mutinynet"), "{uri}");
        assert!(page.body.contains("href=\"/recover/\""), "{uri}");
        assert!(!page.body.contains("only (signet)"), "{uri}");
    }
    assert_eq!(h.wallet.network, "signet");
    assert!(h.wallet.decode("lnbc100n1mainnet").await.is_err());
}

#[tokio::test]
async fn review_does_not_pay_and_edit_preserves_input() {
    let h = harness_with(|config| {
        config.limits.fee_limit_ppm = 10_000;
        config.limits.min_fee_limit_sat = 10;
    })
    .await;
    let cookie = sign_up(&h.app, "alice").await;
    let account = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    h.fund(&account, 2000).await;
    let page = get(&h.app, "/wallet/send", Some(&cookie)).await;
    let csrf = field(&page.body, "csrf");
    let key = field(&page.body, "key");
    let (invoice, _) = h.lnd.external_invoice(1_000_000, None);
    let form = format!("csrf={csrf}&key={key}&destination={invoice}&amount_sat=&comment=hello");
    let review = post(&h.app, "/wallet/send/review", Some(&cookie), &form, Some(SITE)).await;
    assert_eq!(review.status, StatusCode::OK);
    assert!(review.body.contains("Confirm and send"));
    assert!(review.body.contains("1,010 sats"), "fee-inclusive total is shown");
    assert_eq!(h.sats(&account).await, 2000);
    assert!(h.lnd.lock().sends.is_empty());
    let edit = post(&h.app, "/wallet/send/edit", Some(&cookie), &form, Some(SITE)).await;
    assert!(edit.body.contains(&invoice));
    assert!(edit.body.contains("value=\"hello\""));
    assert!(!edit.body.contains("Confirm and send"));
    assert!(h.lnd.lock().sends.is_empty());
    let form = format!("{form}&max_fee_msat=5000");
    let sent = post(&h.app, "/wallet/send", Some(&cookie), &form, Some(SITE)).await;
    assert!(sent.body.contains("Sent 1,000 sats"));
    assert_eq!(h.sats(&account).await, 1000);
    assert_eq!(
        h.lnd.lock().sends[0].fee_limit_msat,
        5000,
        "confirmation cannot raise the reviewed fee allowance"
    );
    post(&h.app, "/wallet/send", Some(&cookie), &form, Some(SITE)).await;
    assert_eq!(h.lnd.lock().sends.len(), 1, "confirm retry cannot pay twice");
    let bad = form.replace(&csrf, "invalid");
    assert_eq!(
        post(&h.app, "/wallet/send/review", Some(&cookie), &bad, Some(SITE))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn sign_up_log_in_and_use_the_wallet() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "Alice").await;
    let wallet = get(&h.app, "/wallet", Some(&cookie)).await;
    assert_eq!(wallet.status, StatusCode::OK);
    assert!(wallet.body.contains("alice@wallet.example.org"));
    assert!(!wallet.body.contains("id=\"send-to\""));
    let receive = get(&h.app, "/wallet/receive", Some(&cookie)).await;
    assert!(receive.body.contains("LNURL1"));
    let csrf = field(&wallet.body, "csrf");

    // Receive: an invoice with a QR code, then its status.
    let reply = post(
        &h.app,
        "/wallet/receive",
        Some(&cookie),
        &format!("csrf={csrf}&amount_sat=2100&memo=hi"),
        Some(SITE),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("<svg class=\"qr\""));
    assert!(reply.body.contains("Waiting for payment"));
    let hash = h.lnd.lock().invoices.keys().next().unwrap().clone();
    let update = h.lnd.pay(&hash);
    h.wallet.credit_settled(&update).await.unwrap();
    let status = get(&h.app, &format!("/wallet/invoice/{hash}"), Some(&cookie)).await;
    assert!(status.body.contains("Paid: 2,100 sats received."));
    assert!(status.body.contains("hx-swap-oob"));

    // Send to another account by address, through htmx.
    sign_up(&h.app, "bob").await;
    let send = get(&h.app, "/wallet/send", Some(&cookie)).await;
    let key = field(&send.body, "key");
    let request = Request::post("/wallet/send")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(COOKIE, &cookie)
        .header(ORIGIN, SITE)
        .header("hx-request", "true")
        .body(Body::from(format!(
            "csrf={csrf}&key={key}&destination=bob%40wallet.example.org&amount_sat=100&comment="
        )))
        .unwrap();
    let reply = read(web::router(h.app.clone()).oneshot(request).await.unwrap()).await;
    assert!(
        reply.body.contains("Sent 100 sats to bob@wallet.example.org"),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("<html"), "htmx gets a fragment");
    assert!(
        reply.body.contains("id=\"balance\" hx-swap-oob=\"true\">2,000 "),
        "the balance updates out of band"
    );

    // Log out, then back in.
    post(&h.app, "/logout", Some(&cookie), &format!("csrf={csrf}"), Some(SITE)).await;
    assert_eq!(
        get(&h.app, "/wallet", Some(&cookie)).await.location.as_deref(),
        Some("/login")
    );
    let wrong = post(
        &h.app,
        "/login",
        None,
        "username=alice&password=wrong+password",
        Some(SITE),
    )
    .await;
    assert!(wrong.body.contains("Wrong username or password."));
    let right = post(
        &h.app,
        "/login",
        None,
        "username=alice&password=correct+horse+battery",
        Some(SITE),
    )
    .await;
    assert_eq!(right.location.as_deref(), Some("/wallet"));
}

#[tokio::test]
async fn cross_site_and_forged_requests_are_refused() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    let form = "username=mallory&password=correct+horse+battery&confirm=correct+horse+battery";
    assert_eq!(
        post(&h.app, "/signup", None, form, None).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&h.app, "/signup", None, form, Some("https://evil.example"))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let forged = post(
        &h.app,
        "/wallet/receive",
        Some(&cookie),
        "csrf=guess&amount_sat=1",
        Some(SITE),
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let taken = post(
        &h.app,
        "/signup",
        None,
        "username=alice&password=correct+horse+battery&confirm=correct+horse+battery",
        Some(SITE),
    )
    .await;
    assert!(taken.body.contains("That username is taken."));
    let reserved = post(
        &h.app,
        "/signup",
        None,
        "username=admin&password=correct+horse+battery&confirm=correct+horse+battery",
        Some(SITE),
    )
    .await;
    assert!(reserved.body.contains("reserved"));
}

#[tokio::test]
async fn responses_carry_security_headers() {
    let h = harness().await;
    let response = web::router(h.app.clone())
        .oneshot(Request::get("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let headers = response.headers();
    let csp = headers.get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe"));
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    let page = read(response).await.body;
    let css = page.split("href=\"/assets/").nth(1).unwrap().split('"').next().unwrap();
    let asset = web::router(h.app.clone())
        .oneshot(Request::get(format!("/assets/{css}")).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::OK);
    assert!(
        asset
            .headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("immutable")
    );
}

#[tokio::test]
async fn healthz_reports_the_database_and_invoice_stream() {
    let h = harness().await;
    let reply = get(&h.app, "/healthz", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body: Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["database"], true);
    assert_eq!(body["invoice_stream"], false);
}

#[tokio::test]
async fn nostr_sign_up_log_in_and_link() {
    let h = harness().await;
    let challenge = post_json(&h.app, "/auth/nostr/challenge", &json!({})).await;
    let challenge: Value = serde_json::from_str(&challenge.body).unwrap();
    let url = challenge["url"].as_str().unwrap();
    assert_eq!(url, "https://wallet.example.org/auth/nostr");
    let event = signed_event(3, url, challenge["challenge"].as_str().unwrap(), now());
    let (pow_challenge, pow_nonce) = solved_pow(&h.app).await;
    let signup = post_json(
        &h.app,
        "/auth/nostr",
        &json!({
            "mode": "signup",
            "username": "nora",
            "event": event_json(&event),
            "pow_challenge": pow_challenge,
            "pow_nonce": pow_nonce.to_string(),
        }),
    )
    .await;
    assert!(signup.body.contains("/wallet"), "{}", signup.body);
    assert!(signup.cookie.unwrap().starts_with("__Host-session="));
    // Replaying the same signed event fails: the challenge is used up.
    let replay = post_json(
        &h.app,
        "/auth/nostr",
        &json!({ "mode": "login", "event": event_json(&event) }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);

    let fresh: Value =
        serde_json::from_str(&post_json(&h.app, "/auth/nostr/challenge", &json!({})).await.body).unwrap();
    let event = signed_event(3, url, fresh["challenge"].as_str().unwrap(), now());
    let login = post_json(
        &h.app,
        "/auth/nostr",
        &json!({ "mode": "login", "event": event_json(&event) }),
    )
    .await;
    assert!(login.body.contains("/wallet"), "{}", login.body);

    // A password account links a different key from its settings page.
    let cookie = sign_up(&h.app, "paula").await;
    let settings = get(&h.app, "/settings", Some(&cookie)).await;
    let csrf = settings
        .body
        .split("data-csrf=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let fresh: Value =
        serde_json::from_str(&post_json(&h.app, "/auth/nostr/challenge", &json!({})).await.body).unwrap();
    let event = signed_event(4, url, fresh["challenge"].as_str().unwrap(), now());
    let request = Request::post("/auth/nostr")
        .header(CONTENT_TYPE, "application/json")
        .header(ORIGIN, SITE)
        .header(COOKIE, &cookie)
        .body(Body::from(
            json!({ "mode": "link", "csrf": csrf, "event": event_json(&event) }).to_string(),
        ))
        .unwrap();
    let linked = read(web::router(h.app.clone()).oneshot(request).await.unwrap()).await;
    assert!(linked.body.contains("/settings"), "{}", linked.body);
    assert!(get(&h.app, "/settings", Some(&cookie)).await.body.contains("npub1"));
}

pub(super) fn event_json(event: &crate::nostr::Event) -> Value {
    json!({
        "id": event.id,
        "pubkey": event.pubkey,
        "created_at": event.created_at,
        "kind": event.kind,
        "tags": event.tags,
        "content": event.content,
        "sig": event.sig,
    })
}

#[tokio::test]
async fn the_operator_pages_need_their_own_password() {
    let h = harness().await;
    let member = sign_up(&h.app, "alice").await;
    assert_eq!(
        get(&h.app, "/admin", Some(&member)).await.location.as_deref(),
        Some("/admin/login")
    );
    let wrong = post(&h.app, "/admin/login", None, "password=nope", Some(SITE)).await;
    assert!(wrong.body.contains("Wrong password."));
    let login = post(&h.app, "/admin/login", None, "password=operator+password", Some(SITE)).await;
    assert_eq!(login.location.as_deref(), Some("/admin"));
    let operator = login.cookie.unwrap();
    assert!(operator.starts_with("__Host-operator="));
    let page = get(&h.app, "/admin", Some(&operator)).await;
    assert!(page.body.contains("Liabilities"));
    assert!(page.body.contains("alice"));
    // Node balances come from the reconciler, never from a call while the page loads.
    assert!(page.body.contains("not read yet"));
    h.wallet.reconcile().await;
    let page = get(&h.app, "/admin", Some(&operator)).await;
    assert!(page.body.contains("Node channel balance"), "{}", page.body);
    let csrf = field(&page.body, "csrf");
    let account = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    let credit = post(
        &h.app,
        &format!("/admin/accounts/{}/credit", account.id),
        Some(&operator),
        &format!("csrf={csrf}&amount_sat=5000&note=welcome"),
        Some(SITE),
    )
    .await;
    assert_eq!(credit.location.as_deref(), Some("/admin?done=credit"));
    assert_eq!(h.sats(&account).await, 5_000);
    let freeze = post(
        &h.app,
        &format!("/admin/accounts/{}/freeze", account.id),
        Some(&operator),
        &format!("csrf={csrf}&frozen=1"),
        Some(SITE),
    )
    .await;
    assert_eq!(freeze.location.as_deref(), Some("/admin?done=freeze"));
    assert!(h.wallet.db.account(account.id).await.unwrap().unwrap().frozen);
    // A member session cannot use operator forms.
    let member_try = post(
        &h.app,
        &format!("/admin/accounts/{}/credit", account.id),
        Some(&member),
        &format!("csrf={csrf}&amount_sat=5000"),
        Some(SITE),
    )
    .await;
    assert_eq!(member_try.location.as_deref(), Some("/admin/login"));
}

const OPERATOR_SITE: &str = "https://wallet-admin.example.org:9443";
const OPERATOR_HOST: &str = "wallet-admin.example.org:9443";
const PUBLIC_HOST: &str = "wallet.example.org";

async fn send(app: &Shared, request: Request<Body>) -> Reply {
    read(web::router(app.clone()).oneshot(request).await.unwrap()).await
}

fn on_host(uri: &str, host: &str, cookie: Option<&str>) -> Request<Body> {
    let mut request = Request::get(uri).header(HOST, host);
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    request.body(Body::empty()).unwrap()
}

fn form_on_host(uri: &str, host: &str, origin: &str, cookie: Option<&str>, form: &str) -> Request<Body> {
    let mut request = Request::post(uri)
        .header(HOST, host)
        .header(ORIGIN, origin)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    request.body(Body::from(form.to_owned())).unwrap()
}

#[tokio::test]
async fn the_operator_pages_can_live_on_their_own_host() {
    let h = harness_with(|config| config.server.operator_url = Some(OPERATOR_SITE.to_owned())).await;
    let password = "password=operator+password";
    // The public name has no operator pages.
    let public = send(&h.app, on_host("/admin/login", PUBLIC_HOST, None)).await;
    assert_eq!(public.status, StatusCode::NOT_FOUND);
    let public_login = form_on_host("/admin/login", PUBLIC_HOST, SITE, None, password);
    assert_eq!(send(&h.app, public_login).await.status, StatusCode::NOT_FOUND);
    // The operator name serves them, and its forms post from its own origin.
    let login = send(
        &h.app,
        form_on_host("/admin/login", OPERATOR_HOST, OPERATOR_SITE, None, password),
    )
    .await;
    assert_eq!(login.location.as_deref(), Some("/admin"), "{}", login.body);
    let operator = login.cookie.unwrap();
    let page = send(&h.app, on_host("/admin", OPERATOR_HOST, Some(&operator))).await;
    assert_eq!(page.status, StatusCode::OK);
    // A valid operator session does nothing on the public name.
    let elsewhere = send(&h.app, on_host("/admin", PUBLIC_HOST, Some(&operator))).await;
    assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
    // The operator origin cannot post to member pages.
    let signup = form_on_host(
        "/signup",
        OPERATOR_HOST,
        OPERATOR_SITE,
        None,
        "username=mallory&password=correct+horse+battery&confirm=correct+horse+battery",
    );
    assert_eq!(send(&h.app, signup).await.status, StatusCode::FORBIDDEN);
    // Logging out ends the session.
    let csrf = field(&page.body, "csrf");
    let logout = send(
        &h.app,
        form_on_host(
            "/admin/logout",
            OPERATOR_HOST,
            OPERATOR_SITE,
            Some(&operator),
            &format!("csrf={csrf}"),
        ),
    )
    .await;
    assert_eq!(logout.location.as_deref(), Some("/admin/login"));
    assert!(logout.cookie.unwrap().ends_with('='), "the cookie is cleared");
    let after = send(&h.app, on_host("/admin", OPERATOR_HOST, Some(&operator))).await;
    assert_eq!(after.location.as_deref(), Some("/admin/login"));
}

#[tokio::test]
async fn wallet_home_links_to_focused_send_and_receive_pages() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    let home = get(&h.app, "/wallet", Some(&cookie)).await;
    assert!(home.body.contains("href=\"/wallet/send\""));
    assert!(home.body.contains("href=\"/wallet/receive\""));
    assert!(home.body.contains("id=\"history\""));
    assert!(!home.body.contains("id=\"send-to\""));
    assert!(!home.body.contains("id=\"receive-amount\""));
    for (path, present, absent) in [
        ("/wallet/send", "id=\"send-to\"", "id=\"receive-amount\""),
        ("/wallet/receive", "id=\"receive-amount\"", "id=\"send-to\""),
    ] {
        let page = get(&h.app, path, Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(page.body.contains(present));
        assert!(!page.body.contains(absent));
        assert!(!page.body.contains("id=\"history\""));
        assert!(page.body.contains("Back to wallet"));
        assert_eq!(get(&h.app, path, None).await.location.as_deref(), Some("/login"));
    }
    let receive = post(
        &h.app,
        "/wallet/receive",
        Some(&cookie),
        &format!("csrf={}&amount_sat=invalid", field(&home.body, "csrf")),
        Some(SITE),
    )
    .await;
    assert!(receive.body.contains("Enter a whole number of sats."));
    assert!(!receive.body.contains("id=\"send-to\""));

    // A plain form response must keep the faucet result visible.
    let faucet = post(
        &h.app,
        "/wallet/faucet",
        Some(&cookie),
        &format!("csrf={}&key={}", field(&home.body, "csrf"), field(&home.body, "key")),
        Some(SITE),
    )
    .await;
    assert!(faucet.body.contains("class=\"faucet-drawer\" open"));
}
