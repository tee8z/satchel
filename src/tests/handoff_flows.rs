//! Flows that start in another app: deep links into Send, handoff sign-in,
//! and the address lookup API.

use axum::body::{Body, to_bytes};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, AUTHORIZATION,
    CONTENT_TYPE, ORIGIN, WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tower::ServiceExt;
use url::form_urlencoded;

use super::web_flows::{Reply, event_json, field, get, post, read, solved_pow};
use super::{Harness, ORIGIN as SITE, harness, harness_with};
use crate::auth;
use crate::db::Account;
use crate::lnd::DecodedInvoice;
use crate::nostr::Event;
use crate::nostr::tests::{http_auth, sign, signed_event};
use crate::util::now;
use crate::web;

const APP: &str = "https://app.example.org";
const HANDOFF: &str = "https://wallet.example.org/auth/nostr/handoff";
const ADDRESS_API: &str = "https://wallet.example.org/api/v1/address";

fn encode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// An account with a real password ("correct horse battery").
async fn password_account(h: &Harness, name: &str) -> Account {
    let hash = auth::hash_password("correct horse battery").unwrap();
    h.wallet.db.create_account(name, Some(&hash), None).await.unwrap()
}

/// A session cookie for an account, as a login sets it.
async fn session_for(h: &Harness, account: &Account) -> String {
    let cookie = h.app.start_session(Some(account.id)).await.unwrap();
    cookie.to_str().unwrap().split(';').next().unwrap().to_owned()
}

/// An account that signs in with the Nostr key `[secret; 32]`.
async fn nostr_account(h: &Harness, name: &str, secret: u8) -> Account {
    let pubkey = sign(secret, now(), Vec::new()).pubkey;
    h.wallet.db.create_account(name, None, Some(&pubkey)).await.unwrap()
}

/// A Satchel whose operator trusts `APP` for handoffs.
async fn trusted() -> Harness {
    harness_with(|config| config.server.handoff_origins = vec![APP.to_owned()]).await
}

fn handoff_event(secret: u8, created_at: i64, name: Option<&str>) -> Event {
    let mut tags = vec![
        vec!["u".to_owned(), HANDOFF.to_owned()],
        vec!["method".to_owned(), "POST".to_owned()],
    ];
    if let Some(name) = name {
        tags.push(vec!["name".to_owned(), name.to_owned()]);
    }
    sign(secret, created_at, tags)
}

fn handoff_form(event: &Event, next: &str) -> String {
    form_urlencoded::Serializer::new(String::new())
        .append_pair("event", &event_json(event).to_string())
        .append_pair("next", next)
        .finish()
}

async fn hand_off(h: &Harness, event: &Event, next: &str, origin: Option<&str>) -> Reply {
    post(&h.app, "/auth/nostr/handoff", None, &handoff_form(event, next), origin).await
}

/// Follows the redirect to the step that sees this site's cookies.
async fn resume(h: &Harness, started: &Reply, session: Option<&str>) -> Reply {
    assert_eq!(
        started.location.as_deref(),
        Some("/auth/nostr/handoff/continue"),
        "{}",
        started.body
    );
    let handoff = started.cookie.as_deref().unwrap();
    assert!(handoff.starts_with("__Host-handoff="), "{handoff}");
    let cookies = match session {
        Some(session) => format!("{session}; {handoff}"),
        None => handoff.to_owned(),
    };
    get(&h.app, "/auth/nostr/handoff/continue", Some(&cookies)).await
}

#[tokio::test]
async fn a_deep_link_survives_the_login_and_pays_only_after_a_tap() {
    let h = harness().await;
    let alice = password_account(&h, "alice").await;
    h.fund(&alice, 10_000).await;
    let (bolt11, _) = h.lnd.external_invoice(2_100_000, None);
    let link = format!("/launch/lightning/lightning:{}", bolt11.to_uppercase());

    let signed_out = get(&h.app, &link, None).await;
    assert_eq!(signed_out.status, StatusCode::SEE_OTHER);
    let login_url = signed_out.location.unwrap();
    assert_eq!(login_url, format!("/login?next={}", encode(&link)));
    let login_page = get(&h.app, &login_url, None).await;
    assert_eq!(field(&login_page.body, "next"), link);
    assert!(
        login_page.body.contains(&format!("data-next=\"{link}\"")),
        "the Nostr login returns there too"
    );
    let form = format!("username=alice&password=correct+horse+battery&next={}", encode(&link));
    let login = post(&h.app, "/login", None, &form, Some(SITE)).await;
    assert_eq!(login.location.as_deref(), Some(link.as_str()), "{}", login.body);
    let cookie = login.cookie.unwrap();

    let page = get(&h.app, &link, Some(&cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("2,100 sats"), "{}", page.body);
    assert!(page.body.contains("external"), "the description shows");
    assert!(page.body.contains(&bolt11), "the field holds the invoice");
    assert!(page.body.contains("Review payment"));
    assert!(page.body.contains("data-scan=\"send-to\""), "Send offers the scanner");
    assert!(h.lnd.lock().sends.is_empty(), "opening a link never pays");

    let csrf = field(&page.body, "csrf");
    let key = field(&page.body, "key");
    let form = format!("csrf={csrf}&key={key}&destination={bolt11}&amount_sat=&comment=");
    let paid = post(&h.app, "/wallet/send", Some(&cookie), &form, Some(SITE)).await;
    assert!(paid.body.contains("Sent 2,100 sats"), "{}", paid.body);
    assert_eq!(h.lnd.lock().sends.len(), 1);
}

#[tokio::test]
async fn deep_links_prefill_addresses_and_show_send_errors() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let cookie = session_for(&h, &alice).await;
    let address = get(
        &h.app,
        "/launch/lightning/LIGHTNING:Bob@Wallet.Example.org",
        Some(&cookie),
    )
    .await;
    assert_eq!(address.status, StatusCode::OK);
    assert!(address.body.contains("bob@wallet.example.org"), "{}", address.body);
    assert!(address.body.contains("Enter the amount below."));

    let expired = "lntbs5u1pexpired";
    h.lnd.lock().external.insert(
        expired.to_owned(),
        DecodedInvoice {
            payment_hash: "ab".repeat(32),
            num_msat: 500_000,
            timestamp: u64::try_from(now() - 7200).unwrap(),
            expiry: 3600,
            ..DecodedInvoice::default()
        },
    );
    for (target, message) in [
        (
            "lntb10u1ptestnet",
            "That invoice is for testnet, but this wallet runs on signet.",
        ),
        ("LNBCRT10U1PREGTEST", "That invoice is for regtest"),
        ("lnbc10u1pmainnet", "That is a mainnet invoice."),
        ("lntbs10u1punknown", "Could not read that invoice."),
        (expired, "That invoice has expired."),
        ("not-a-thing", "does not look like a Lightning payment request"),
    ] {
        let page = get(&h.app, &format!("/launch/lightning/{target}"), Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(page.body.contains(message), "{target}: {}", page.body);
        assert!(page.body.contains("Review payment"), "no review button for {target}");
    }
    assert!(h.lnd.lock().sends.is_empty());
}

#[tokio::test]
async fn next_never_leaves_the_site() {
    let h = harness().await;
    password_account(&h, "alice").await;
    let overlong = format!("/{}", "a".repeat(3000));
    for next in [
        "//evil.example",
        "https://evil.example",
        "/\\evil.example",
        "/\t/evil.example",
        overlong.as_str(),
    ] {
        let page = get(&h.app, &format!("/login?next={}", encode(next)), None).await;
        assert_eq!(field(&page.body, "next"), "/wallet", "{next:?}");
        let form = format!("username=alice&password=correct+horse+battery&next={}", encode(next));
        let login = post(&h.app, "/login", None, &form, Some(SITE)).await;
        assert_eq!(login.location.as_deref(), Some("/wallet"), "{next:?}");
    }
}

async fn post_json(h: &Harness, uri: &str, body: &Value) -> Reply {
    let request = Request::post(uri)
        .header(CONTENT_TYPE, "application/json")
        .header(ORIGIN, SITE)
        .body(Body::from(body.to_string()))
        .unwrap();
    read(web::router(h.app.clone()).oneshot(request).await.unwrap()).await
}

#[tokio::test]
async fn the_nostr_extension_login_returns_to_next() {
    let h = harness().await;
    nostr_account(&h, "nora", 3).await;
    for (next, expected) in [
        (
            "/launch/lightning/bob@wallet.example.org",
            "/launch/lightning/bob@wallet.example.org",
        ),
        ("//evil.example", "/wallet"),
    ] {
        let challenge = post_json(&h, "/auth/nostr/challenge", &json!({})).await;
        let challenge: Value = serde_json::from_str(&challenge.body).unwrap();
        let url = challenge["url"].as_str().unwrap();
        let event = signed_event(3, url, challenge["challenge"].as_str().unwrap(), now());
        let login = post_json(
            &h,
            "/auth/nostr",
            &json!({ "mode": "login", "event": event_json(&event), "next": next }),
        )
        .await;
        let body: Value = serde_json::from_str(&login.body).unwrap();
        assert_eq!(body["redirect"], expected, "{next}");
    }
}

#[tokio::test]
async fn a_trusted_app_hands_over_a_signed_out_browser() {
    let h = trusted().await;
    nostr_account(&h, "alice", 5).await;
    let next = "/wallet";
    let event = handoff_event(5, now(), None);
    let started = hand_off(&h, &event, next, Some(APP)).await;
    let done = resume(&h, &started, None).await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{}", done.body);
    assert_eq!(done.location.as_deref(), Some(next));
    let cookie = done.cookie.unwrap();
    assert!(cookie.starts_with("__Host-session="), "{cookie}");
    let wallet = get(&h.app, "/wallet", Some(&cookie)).await;
    assert!(wallet.body.contains("alice@wallet.example.org"));

    // The same signed event works once.
    let replay = hand_off(&h, &event, next, Some(APP)).await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
    assert!(replay.body.contains("already used"), "{}", replay.body);
    assert!(replay.cookie.is_none());

    // Already in that wallet: straight to `next`, keeping the session.
    let again = hand_off(&h, &handoff_event(5, now() + 1, None), "/settings", Some(APP)).await;
    let done = resume(&h, &again, Some(&cookie)).await;
    assert_eq!(done.location.as_deref(), Some("/settings"));
    assert!(
        done.cookie.is_some_and(|cookie| cookie.starts_with("__Host-handoff=")),
        "no new session, only the handoff cookie cleared"
    );

    // Every other form post from the app is still refused.
    let signup = "username=mallory&password=correct+horse+battery&confirm=correct+horse+battery";
    assert_eq!(
        post(&h.app, "/signup", None, signup, Some(APP)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&h.app, "/auth/nostr/handoff/confirm", None, "token=x", Some(APP))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // Without the handoff cookie, the redirect target does nothing.
    let bare = get(&h.app, "/auth/nostr/handoff/continue", None).await;
    assert_eq!(bare.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn handoffs_with_bad_events_are_refused() {
    let h = trusted().await;
    nostr_account(&h, "alice", 5).await;
    let tags = |url: &str, method: &str| {
        vec![
            vec!["u".to_owned(), url.to_owned()],
            vec!["method".to_owned(), method.to_owned()],
        ]
    };
    let wrong_url = sign(5, now(), tags("https://evil.example/auth/nostr/handoff", "POST"));
    let wrong_method = sign(5, now(), tags(HANDOFF, "GET"));
    let stale = handoff_event(5, now() - 600, None);
    let early = handoff_event(5, now() + 600, None);
    let mut forged = handoff_event(5, now(), None);
    forged.sig = handoff_event(6, now(), None).sig;
    let mut tampered = handoff_event(5, now(), None);
    tampered.content = "changed".into();
    let mut wrong_kind = handoff_event(5, now(), None);
    wrong_kind.kind = 1;
    for (event, message) in [
        (wrong_url, "for another page"),
        (wrong_method, "for another page"),
        (stale, "too far from now"),
        (early, "too far from now"),
        (forged, "The signature is invalid."),
        (tampered, "does not match its content"),
        (wrong_kind, "wrong kind"),
    ] {
        let reply = hand_off(&h, &event, "/wallet", Some(APP)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{message}");
        assert!(reply.body.contains(message), "{message}: {}", reply.body);
        assert!(reply.cookie.is_none(), "{message}");
    }
    let garbage = post(&h.app, "/auth/nostr/handoff", None, "event=not+json", Some(APP)).await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn frozen_wallets_cannot_be_handed_over() {
    let h = trusted().await;
    let alice = nostr_account(&h, "alice", 5).await;
    h.wallet.db.set_frozen(alice.id, true).await.unwrap();
    let refused = hand_off(&h, &handoff_event(5, now(), None), "/wallet", Some(APP)).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.body.contains("frozen"));
    assert!(refused.cookie.is_none());

    // Frozen between the handoff and the click: the button refuses too.
    h.wallet.db.set_frozen(alice.id, false).await.unwrap();
    let started = hand_off(&h, &handoff_event(5, now() + 1, None), "/wallet", None).await;
    let page = resume(&h, &started, None).await;
    let token = field(&page.body, "token");
    h.wallet.db.set_frozen(alice.id, true).await.unwrap();
    let confirm = post(
        &h.app,
        "/auth/nostr/handoff/confirm",
        None,
        &format!("token={token}"),
        Some(SITE),
    )
    .await;
    assert_eq!(confirm.status, StatusCode::FORBIDDEN);
    assert!(
        confirm
            .cookie
            .is_none_or(|cookie| !cookie.starts_with("__Host-session=")),
        "no session for a frozen wallet"
    );
}

#[tokio::test]
async fn other_sites_get_a_confirmation_page() {
    let h = trusted().await;
    nostr_account(&h, "alice", 5).await;
    for (offset, origin) in [(0, Some("https://evil.example")), (1, None)] {
        let started = hand_off(&h, &handoff_event(5, now() + offset, None), "/settings", origin).await;
        let page = resume(&h, &started, None).await;
        assert_eq!(page.status, StatusCode::OK, "{origin:?}: {}", page.body);
        assert!(page.body.contains("Continue as alice?"));
        assert!(page.body.contains("npub1"));
        assert!(page.cookie.is_none(), "nothing is signed in yet");
        let token = field(&page.body, "token");
        let form = format!("token={token}");
        // Only a page of this site can press the button.
        let forged = post(
            &h.app,
            "/auth/nostr/handoff/confirm",
            None,
            &form,
            Some("https://evil.example"),
        )
        .await;
        assert_eq!(forged.status, StatusCode::FORBIDDEN);
        let confirmed = post(&h.app, "/auth/nostr/handoff/confirm", None, &form, Some(SITE)).await;
        assert_eq!(confirmed.location.as_deref(), Some("/settings"));
        assert!(confirmed.cookie.unwrap().starts_with("__Host-session="));
        let again = post(&h.app, "/auth/nostr/handoff/confirm", None, &form, Some(SITE)).await;
        assert_eq!(again.status, StatusCode::BAD_REQUEST, "the button works once");
    }
}

#[tokio::test]
async fn a_handoff_for_another_wallet_asks_before_switching() {
    let h = trusted().await;
    nostr_account(&h, "alice", 5).await;
    let bob = h.account("bob").await;
    let bob_cookie = session_for(&h, &bob).await;
    let started = hand_off(&h, &handoff_event(5, now(), None), "/wallet", Some(APP)).await;
    let page = resume(&h, &started, Some(&bob_cookie)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("Continue as alice?"));
    assert!(
        page.body.contains("signed in here as <strong>bob</strong>"),
        "{}",
        page.body
    );
    let token = field(&page.body, "token");
    let confirmed = post(
        &h.app,
        "/auth/nostr/handoff/confirm",
        Some(&bob_cookie),
        &format!("token={token}"),
        Some(SITE),
    )
    .await;
    assert_eq!(confirmed.location.as_deref(), Some("/wallet"));
    let alice_cookie = confirmed.cookie.unwrap();
    let wallet = get(&h.app, "/wallet", Some(&alice_cookie)).await;
    assert!(wallet.body.contains("alice@wallet.example.org"));
    let old = get(&h.app, "/wallet", Some(&bob_cookie)).await;
    assert_eq!(old.location.as_deref(), Some("/login"), "bob's session ended");
}

#[tokio::test]
async fn a_new_key_creates_a_wallet_through_the_sign_up_form() {
    let h = trusted().await;
    let next = "/settings";
    let event = handoff_event(9, now(), Some(" Nora "));
    let page = hand_off(&h, &event, next, Some(APP)).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("Create your wallet"));
    assert!(page.body.contains("action=\"/signup\""));
    assert!(
        page.body.contains("value=\"nora\""),
        "the name tag prefills the username"
    );
    assert!(!page.body.contains("name=\"password\""), "no password needed");
    assert!(!page.body.contains(&event.pubkey), "the form never carries the key");
    assert!(page.cookie.is_none());
    let token = field(&page.body, "handoff");

    // Another site cannot submit it, and a made-up token does nothing.
    let form = |username: &str| format!("username={username}&handoff={token}");
    let forged = post(&h.app, "/signup", None, &form("nora"), Some(APP)).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let made_up = post(&h.app, "/signup", None, "username=nora&handoff=made-up", Some(SITE)).await;
    assert!(made_up.body.contains("expired"), "{}", made_up.body);
    assert!(made_up.cookie.is_none());
    // Sign-up errors keep the handoff form, token included.
    let reserved = post(&h.app, "/signup", None, &form("admin"), Some(SITE)).await;
    assert!(reserved.body.contains("reserved"));
    assert_eq!(field(&reserved.body, "handoff"), token);

    // Like every new wallet, it pays the proof of work.
    let unsolved = post(&h.app, "/signup", None, &form("nora"), Some(SITE)).await;
    assert!(unsolved.cookie.is_none(), "no wallet without the proof of work");
    assert_eq!(field(&unsolved.body, "handoff"), token);
    let (challenge, nonce) = solved_pow(&h.app).await;
    let solved = format!("{}&pow_challenge={challenge}&pow_nonce={nonce}", form("nora"));
    let created = post(&h.app, "/signup", None, &solved, Some(SITE)).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    assert_eq!(created.location.as_deref(), Some(next));
    assert!(created.cookie.unwrap().starts_with("__Host-session="));
    let nora = h.wallet.db.account_by_username("nora").await.unwrap().unwrap();
    assert_eq!(nora.nostr_pubkey.as_deref(), Some(event.pubkey.as_str()));
    assert!(nora.password_hash.is_none());
    let again = post(&h.app, "/signup", None, &form("nora2"), Some(SITE)).await;
    assert!(again.body.contains("expired"), "the token works once");

    // Next time the same key signs straight in.
    let started = hand_off(&h, &handoff_event(9, now() + 1, None), "/wallet", Some(APP)).await;
    let done = resume(&h, &started, None).await;
    assert_eq!(done.location.as_deref(), Some("/wallet"));

    // A suggested name that is taken is left out.
    let taken = hand_off(&h, &handoff_event(10, now(), Some("nora")), "/wallet", Some(APP)).await;
    assert!(taken.body.contains("Create your wallet"));
    assert!(!taken.body.contains("value=\"nora\""));
}

fn nostr_authorization(event: &Event) -> String {
    format!("Nostr {}", STANDARD.encode(event_json(event).to_string()))
}

async fn call_api(
    h: &Harness,
    method: Method,
    authorization: Option<&str>,
    origin: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().method(method).uri("/api/v1/address");
    if let Some(authorization) = authorization {
        request = request.header(AUTHORIZATION, authorization);
    }
    if let Some(origin) = origin {
        request = request
            .header(ORIGIN, origin)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "authorization");
    }
    let response = web::router(h.app.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, headers, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn apps_look_up_the_address_of_a_signing_key() {
    let h = trusted().await;
    let alice = nostr_account(&h, "alice", 5).await;
    let auth = nostr_authorization(&http_auth(5, ADDRESS_API, "GET", now()));
    let (status, headers, body) = call_api(&h, Method::GET, Some(&auth), Some(APP)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["lightning_address"], "alice@wallet.example.org");
    assert_eq!(body["username"], "alice");
    assert_eq!(headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), APP);
    assert!(headers.get("access-control-allow-credentials").is_none());

    // Unknown keys and frozen wallets look alike, and nothing is created.
    let stranger = http_auth(9, ADDRESS_API, "GET", now());
    let (status, _, body) = call_api(&h, Method::GET, Some(&nostr_authorization(&stranger)), Some(APP)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], "no_account");
    assert!(h.wallet.db.account_by_nostr(&stranger.pubkey).await.unwrap().is_none());
    h.wallet.db.set_frozen(alice.id, true).await.unwrap();
    let (status, headers, _) = call_api(&h, Method::GET, Some(&auth), Some("https://evil.example")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
        "unlisted origins get no CORS"
    );
}

#[tokio::test]
async fn the_address_api_needs_a_fresh_event_for_its_own_url() {
    let h = trusted().await;
    nostr_account(&h, "alice", 5).await;
    let mut forged = http_auth(5, ADDRESS_API, "GET", now());
    forged.sig = http_auth(6, ADDRESS_API, "GET", now()).sig;
    let events = [
        http_auth(5, "https://wallet.example.org/api/v1/address?x=1", "GET", now()),
        http_auth(5, "https://evil.example/api/v1/address", "GET", now()),
        http_auth(5, ADDRESS_API, "POST", now()),
        http_auth(5, ADDRESS_API, "GET", now() - 120),
        forged,
    ];
    let signed: Vec<String> = events.iter().map(nostr_authorization).collect();
    let mut headers: Vec<Option<&str>> = vec![
        None,
        Some("Bearer abc"),
        Some("Nostr not-base64!"),
        Some("Nostr bm90IGpzb24="),
    ];
    headers.extend(signed.iter().map(|value| Some(value.as_str())));
    for authorization in headers {
        let (status, headers, body) = call_api(&h, Method::GET, authorization, Some(APP)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}: {body}");
        assert_eq!(headers.get(WWW_AUTHENTICATE).unwrap(), "Nostr");
        assert_eq!(
            headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
            APP,
            "the app can read errors"
        );
    }
}

#[tokio::test]
async fn the_address_api_answers_preflights_for_listed_origins_and_is_rate_limited() {
    let h = trusted().await;
    let (status, headers, _) = call_api(&h, Method::OPTIONS, None, Some(APP)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), APP);
    assert_eq!(headers.get(ACCESS_CONTROL_ALLOW_METHODS).unwrap(), "GET");
    let allowed = headers.get(ACCESS_CONTROL_ALLOW_HEADERS).unwrap().to_str().unwrap();
    assert!(allowed.eq_ignore_ascii_case("authorization"), "{allowed}");
    let (_, headers, _) = call_api(&h, Method::OPTIONS, None, Some("https://evil.example")).await;
    assert!(headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    let unlisted = harness().await;
    let (_, headers, _) = call_api(&unlisted, Method::OPTIONS, None, Some(APP)).await;
    assert!(
        headers.get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(),
        "no origins by default"
    );

    let limited = harness_with(|config| config.rate_limits.lnurl_per_ip_per_minute = 2).await;
    let statuses = [
        call_api(&limited, Method::GET, None, None).await.0,
        call_api(&limited, Method::GET, None, None).await.0,
        call_api(&limited, Method::GET, None, None).await.0,
    ];
    assert_eq!(
        statuses,
        [
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
}

#[tokio::test]
async fn the_camera_is_allowed_for_this_site_only() {
    let h = harness().await;
    let response = web::router(h.app.clone())
        .oneshot(Request::get("/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let headers = response.headers();
    assert_eq!(
        headers.get("permissions-policy").unwrap(),
        "camera=(self), microphone=()"
    );
    let csp = headers.get("content-security-policy").unwrap().to_str().unwrap();
    assert!(csp.contains("default-src 'none'") && csp.contains("form-action 'self'"));
}

#[tokio::test]
async fn payment_handoffs_keep_the_current_wallet_or_offer_login_with_the_invoice() {
    let h = trusted().await;
    let alice = password_account(&h, "alice").await;
    let cookie = session_for(&h, &alice).await;
    nostr_account(&h, "other-wallet", 5).await;
    let (bolt11, _) = h.lnd.external_invoice(21_000, None);
    let next = format!("/launch/lightning/{bolt11}");
    // The cross-site form POST has no session cookie. The redirect GET has it.
    // Both an unknown app key and a key tied to another wallet must keep Alice.
    for secret in [9, 5] {
        let started = hand_off(&h, &handoff_event(secret, now(), None), &next, Some(APP)).await;
        assert_eq!(started.location.as_deref(), Some(next.as_str()));
        assert!(
            started.cookie.is_none(),
            "a payment handoff must not replace the session"
        );
        let page = get(&h.app, &next, Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK);
        assert!(page.body.contains(&bolt11));
        assert!(page.body.contains("Review payment"));
        assert!(!page.body.contains("Create your wallet"));
        assert!(
            get(&h.app, "/wallet", Some(&cookie))
                .await
                .body
                .contains("alice@wallet.example.org")
        );
    }
    let login = get(&h.app, &next, None).await;
    let page = get(&h.app, login.location.as_deref().unwrap(), None).await;
    assert_eq!(field(&page.body, "next"), next);
    assert!(page.body.contains("Log in with Nostr"));
    let form = format!("username=alice&password=wrong&next={}", encode(&next));
    let failed = post(&h.app, "/login", None, &form, Some(SITE)).await;
    assert_eq!(field(&failed.body, "next"), next);
    let form = form.replace("password=wrong", "password=correct+horse+battery");
    let logged_in = post(&h.app, "/login", None, &form, Some(SITE)).await;
    assert_eq!(logged_in.location.as_deref(), Some(next.as_str()));
    let loaded = get(&h.app, &next, logged_in.cookie.as_deref()).await;
    assert!(loaded.body.contains(&bolt11));
    assert!(h.lnd.lock().sends.is_empty());
}

#[tokio::test]
async fn choosing_signup_keeps_the_invoice_through_validation_and_creation() {
    let h = harness().await;
    let (bolt11, _) = h.lnd.external_invoice(21_000, None);
    let next = format!("/launch/lightning/{bolt11}");
    let signup_url = format!("/signup?next={}", encode(&next));
    let login = get(&h.app, &format!("/login?next={}", encode(&next)), None).await;
    assert!(login.body.contains(&format!("href=\"{signup_url}\"")));
    let signup = get(&h.app, &signup_url, None).await;
    assert_eq!(field(&signup.body, "next"), next);
    assert!(signup.body.contains(&format!("data-next=\"{next}\"")));
    let form = format!("username=alice&password=short&confirm=short&next={}", encode(&next));
    let retry = post(&h.app, "/signup", None, &form, Some(SITE)).await;
    assert_eq!(field(&retry.body, "next"), next);
    let form = format!(
        "{}&next={}",
        super::web_flows::sign_up_form(&h.app, "alice").await,
        encode(&next)
    );
    let created = post(&h.app, "/signup", None, &form, Some(SITE)).await;
    assert_eq!(created.location.as_deref(), Some(next.as_str()));
    let loaded = get(&h.app, &next, created.cookie.as_deref()).await;
    assert!(loaded.body.contains(&bolt11));
    assert!(h.lnd.lock().sends.is_empty());
}
