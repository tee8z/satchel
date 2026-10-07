//! Abuse protections over HTTP: proof of work on every account creation,
//! global sign-up caps, client address grouping, operator blocks, freezing,
//! the per-address faucet cap, and the open-invoice cap.

use axum::body::Body;
use axum::http::header::{CONTENT_TYPE, COOKIE, ORIGIN};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::web_flows::{Reply, event_json, field, get, post, post_json, read, sign_up, sign_up_form, solved_pow};
use super::{Harness, ORIGIN as SITE, harness, harness_with};
use crate::blocklist::Cidr;
use crate::error::WalletError;
use crate::nostr::tests::signed_event;
use crate::util::{now, random_token};
use crate::web::{self, Shared};

const FORWARDED: &str = "x-forwarded-for";

/// A harness whose clients are told apart by `X-Forwarded-For`.
async fn behind_proxy(adjust: impl FnOnce(&mut crate::config::Config)) -> Harness {
    harness_with(|config| {
        config.server.client_ip_header = Some(FORWARDED.to_owned());
        adjust(config);
    })
    .await
}

async fn get_from(app: &Shared, uri: &str, client: &str) -> Reply {
    let request = Request::get(uri).header(FORWARDED, client).body(Body::empty()).unwrap();
    read(web::router(app.clone()).oneshot(request).await.unwrap()).await
}

async fn post_from(app: &Shared, uri: &str, client: &str, cookie: Option<&str>, form: &str) -> Reply {
    let mut request = Request::post(uri)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(ORIGIN, SITE)
        .header(FORWARDED, client);
    if let Some(cookie) = cookie {
        request = request.header(COOKIE, cookie);
    }
    read(
        web::router(app.clone())
            .oneshot(request.body(Body::from(form.to_owned())).unwrap())
            .await
            .unwrap(),
    )
    .await
}

const PASSWORDS: &str = "password=correct+horse+battery&confirm=correct+horse+battery";

async fn operator(app: &Shared) -> (String, String) {
    let login = post(app, "/admin/login", None, "password=operator+password", Some(SITE)).await;
    let cookie = login.cookie.expect("operator cookie");
    let page = get(app, "/admin", Some(&cookie)).await;
    let csrf = field(&page.body, "csrf");
    (cookie, csrf)
}

fn lnurl_status(reply: &Reply) -> Value {
    serde_json::from_str::<Value>(&reply.body).unwrap()["status"].clone()
}

/// A login event signed over a fresh challenge.
async fn nostr_event(app: &Shared, secret: u8) -> crate::nostr::Event {
    let issued: Value = serde_json::from_str(&post_json(app, "/auth/nostr/challenge", &json!({})).await.body).unwrap();
    signed_event(
        secret,
        issued["url"].as_str().unwrap(),
        issued["challenge"].as_str().unwrap(),
        now(),
    )
}

#[tokio::test]
async fn sign_up_needs_a_proof_of_work() {
    let h = harness().await;
    let page = get(&h.app, "/signup", None).await;
    assert!(page.body.contains("name=\"pow_challenge\""));
    assert!(page.body.contains("data-pow-worker=\"/assets/pow-worker."));

    let missing = post(
        &h.app,
        "/signup",
        None,
        &format!("username=alice&{PASSWORDS}"),
        Some(SITE),
    )
    .await;
    assert_eq!(missing.status, StatusCode::OK);
    assert!(
        missing.body.contains("did not finish the sign-up check"),
        "{}",
        missing.body
    );
    assert!(h.wallet.db.account_by_username("alice").await.unwrap().is_none());

    // Below the smallest solution, no nonce solves the challenge.
    let (challenge, nonce) = solved_pow(&h.app).await;
    if nonce > 0 {
        let form = format!("username=alice&{PASSWORDS}&pow_challenge={challenge}&pow_nonce=0");
        let refused = post(&h.app, "/signup", None, &form, Some(SITE)).await;
        assert!(refused.body.contains("did not pass"), "{}", refused.body);
    }

    // The real solution works once; the same solution cannot create a second account.
    let form = format!("username=alice&{PASSWORDS}&pow_challenge={challenge}&pow_nonce={nonce}");
    let created = post(&h.app, "/signup", None, &form, Some(SITE)).await;
    assert_eq!(created.location.as_deref(), Some("/wallet"), "{}", created.body);
    let form = format!("username=bob&{PASSWORDS}&pow_challenge={challenge}&pow_nonce={nonce}");
    let reused = post(&h.app, "/signup", None, &form, Some(SITE)).await;
    assert!(reused.body.contains("already used"), "{}", reused.body);

    // A taken name is refused before the proof of work is spent.
    let form = sign_up_form(&h.app, "alice").await;
    let taken = post(&h.app, "/signup", None, &form, Some(SITE)).await;
    assert!(taken.body.contains("That username is taken."));
    let retry = form.replace("username=alice", "username=carol");
    let carol = post(&h.app, "/signup", None, &retry, Some(SITE)).await;
    assert_eq!(carol.location.as_deref(), Some("/wallet"), "{}", carol.body);

    let account = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    let summaries = h.wallet.db.account_summaries("", 10).await.unwrap();
    let alice = summaries.iter().find(|summary| summary.id == account.id).unwrap();
    assert_eq!(
        alice.signup_client.as_deref(),
        Some("0.0.0.0"),
        "the sign-up address is recorded"
    );
}

#[tokio::test]
async fn nostr_sign_up_needs_a_proof_of_work_too() {
    let h = harness().await;
    let event = nostr_event(&h.app, 5).await;
    let refused = post_json(
        &h.app,
        "/auth/nostr",
        &json!({ "mode": "signup", "username": "nina", "event": event_json(&event) }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(h.wallet.db.account_by_username("nina").await.unwrap().is_none());
    // Logging in with an unknown key never creates a wallet.
    let event = nostr_event(&h.app, 5).await;
    let login = post_json(
        &h.app,
        "/auth/nostr",
        &json!({ "mode": "login", "event": event_json(&event) }),
    )
    .await;
    assert_eq!(login.status, StatusCode::NOT_FOUND);
    let event = nostr_event(&h.app, 5).await;
    let (challenge, nonce) = solved_pow(&h.app).await;
    let created = post_json(
        &h.app,
        "/auth/nostr",
        &json!({
            "mode": "signup",
            "username": "nina",
            "event": event_json(&event),
            "pow_challenge": challenge,
            "pow_nonce": nonce.to_string(),
        }),
    )
    .await;
    assert!(created.body.contains("/wallet"), "{}", created.body);
}

#[tokio::test]
async fn difficulty_rises_with_sign_ups_across_the_service() {
    let h = harness_with(|config| config.pow.step_signups = 2).await;
    let difficulty = |reply: Reply| serde_json::from_str::<Value>(&reply.body).unwrap()["difficulty"].clone();
    assert_eq!(difficulty(post_json(&h.app, "/auth/pow", &json!({})).await), 4);
    // A solution issued while it was quiet ...
    let (early, early_nonce) = solved_pow(&h.app).await;
    sign_up(&h.app, "alice").await;
    sign_up(&h.app, "bob").await;
    assert_eq!(difficulty(post_json(&h.app, "/auth/pow", &json!({})).await), 5);
    // ... is too easy once two accounts were created in the last hour.
    let form = format!("username=carol&{PASSWORDS}&pow_challenge={early}&pow_nonce={early_nonce}");
    let refused = post(&h.app, "/signup", None, &form, Some(SITE)).await;
    assert!(refused.body.contains("got harder"), "{}", refused.body);
    sign_up(&h.app, "carol").await;
    let metrics = metrics(&h.app).await;
    assert!(metrics.contains("satchel_pow_difficulty_bits 5\n"), "{metrics}");
    assert!(
        metrics.contains("satchel_pow_checks_total{outcome=\"verified\"} 3\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("satchel_pow_checks_total{outcome=\"rejected\"} 1\n"),
        "{metrics}"
    );
}

#[tokio::test]
async fn sign_ups_have_a_global_hourly_cap() {
    let h = behind_proxy(|config| config.rate_limits.signups_global_per_hour = 2).await;
    for (name, client) in [("alice", "198.51.100.1"), ("bob", "198.51.100.2")] {
        let form = sign_up_form(&h.app, name).await;
        let reply = post_from(&h.app, "/signup", client, None, &form).await;
        assert_eq!(reply.location.as_deref(), Some("/wallet"), "{}", reply.body);
    }
    let form = sign_up_form(&h.app, "carol").await;
    let capped = post_from(&h.app, "/signup", "198.51.100.3", None, &form).await;
    assert!(capped.body.contains("Many wallets were created"), "{}", capped.body);
    assert!(
        metrics(&h.app)
            .await
            .contains("satchel_rate_limited_total{scope=\"signup-global\"} 1\n")
    );
}

#[tokio::test]
async fn ipv6_clients_share_a_limit_per_prefix() {
    let h = behind_proxy(|config| config.rate_limits.lnurl_per_ip_per_minute = 1).await;
    h.account("alice").await;
    let uri = "/.well-known/lnurlp/alice";
    assert_eq!(get_from(&h.app, uri, "2001:db8:1:2::1").await.status, StatusCode::OK);
    let same_56 = get_from(&h.app, uri, "2001:db8:1:3:abcd::9").await;
    assert_eq!(lnurl_status(&same_56), "ERROR", "same /56: {}", same_56.body);
    let next_56 = get_from(&h.app, uri, "2001:db8:1:100::1").await;
    assert!(next_56.body.contains("payRequest"), "{}", next_56.body);
    assert!(
        metrics(&h.app)
            .await
            .contains("satchel_rate_limited_total{scope=\"lnurl-ip\"} 1\n")
    );

    let wider = behind_proxy(|config| {
        config.rate_limits.lnurl_per_ip_per_minute = 1;
        config.rate_limits.ipv6_prefix_len = 64;
    })
    .await;
    wider.account("alice").await;
    assert_eq!(
        get_from(&wider.app, uri, "2001:db8:1:2::1").await.status,
        StatusCode::OK
    );
    let other_64 = get_from(&wider.app, uri, "2001:db8:1:3::1").await;
    assert!(other_64.body.contains("payRequest"), "{}", other_64.body);
}

#[tokio::test]
async fn operator_blocks_refuse_every_public_route() {
    let h = behind_proxy(|_| {}).await;
    h.account("alice").await;
    let (cookie, csrf) = operator(&h.app).await;
    let blocked = "203.0.113.50";
    let neighbour = "203.0.113.200";
    let elsewhere = "198.51.100.7";

    for bad in ["nonsense", "0.0.0.0/0", "2001::/8"] {
        let reply = post(
            &h.app,
            "/admin/blocks",
            Some(&cookie),
            &format!("csrf={csrf}&cidr={bad}&reason=x"),
            Some(SITE),
        )
        .await;
        assert_eq!(reply.location.as_deref(), Some("/admin?error=cidr"), "{bad}");
    }
    let added = post(
        &h.app,
        "/admin/blocks",
        Some(&cookie),
        &format!("csrf={csrf}&cidr=203.0.113.0%2F24&reason=spam+sign-ups&hours="),
        Some(SITE),
    )
    .await;
    assert_eq!(added.location.as_deref(), Some("/admin?done=block"));

    // HTML routes answer with a 403 page; LNURL routes with a LUD-06 error.
    let page = get_from(&h.app, "/signup", blocked).await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    assert!(
        page.body.contains("blocked requests from your network"),
        "{}",
        page.body
    );
    assert!(page.body.contains("Test network only"));
    assert!(!page.body.contains("spam sign-ups"), "operator reasons stay private");
    assert_eq!(
        get_from(&h.app, "/signup", neighbour).await.status,
        StatusCode::FORBIDDEN
    );
    let form = sign_up_form(&h.app, "mallory").await;
    assert_eq!(
        post_from(&h.app, "/signup", blocked, None, &form).await.status,
        StatusCode::FORBIDDEN
    );
    for uri in ["/.well-known/lnurlp/alice", "/lnurlp/alice/callback?amount=1000"] {
        let reply = get_from(&h.app, uri, blocked).await;
        assert_eq!(lnurl_status(&reply), "ERROR", "{uri}: {}", reply.body);
        assert!(reply.body.contains("blocked"));
    }
    assert_eq!(get_from(&h.app, "/signup", elsewhere).await.status, StatusCode::OK);
    // The operator pages, static files and the health check stay reachable.
    assert_eq!(get_from(&h.app, "/admin/login", blocked).await.status, StatusCode::OK);
    assert_eq!(get_from(&h.app, "/healthz", blocked).await.status, StatusCode::OK);
    let operator_page = get(&h.app, "/admin", Some(&cookie)).await;
    assert!(operator_page.body.contains("203.0.113.0/24"));
    assert!(operator_page.body.contains("spam sign-ups"));
    assert!(metrics(&h.app).await.contains("satchel_blocked_requests_total 5\n"));

    // Removing the block lets the network back in.
    let id = h.wallet.db.blocks().await.unwrap()[0].id;
    let removed = post(
        &h.app,
        &format!("/admin/blocks/{id}/remove"),
        Some(&cookie),
        &format!("csrf={csrf}"),
        Some(SITE),
    )
    .await;
    assert_eq!(removed.location.as_deref(), Some("/admin?done=unblock"));
    assert_eq!(get_from(&h.app, "/signup", blocked).await.status, StatusCode::OK);

    // IPv6 blocks cover their prefix; expired blocks refuse nothing.
    let v6 = Cidr::parse("2001:db8:5::/56").unwrap();
    h.wallet.db.add_block(&v6, "test", Some(now() + 3600)).await.unwrap();
    let old = Cidr::parse("192.0.2.0/24").unwrap();
    h.wallet.db.add_block(&old, "test", Some(now() - 1)).await.unwrap();
    h.app.blocks.reload(&h.wallet.db).await.unwrap();
    assert_eq!(
        get_from(&h.app, "/", "2001:db8:5:ff::1").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(get_from(&h.app, "/", "2001:db8:5:100::1").await.status, StatusCode::OK);
    assert_eq!(get_from(&h.app, "/", "192.0.2.1").await.status, StatusCode::OK);
    // The block form also takes an expiry in hours.
    let timed = post(
        &h.app,
        "/admin/blocks",
        Some(&cookie),
        &format!("csrf={csrf}&cidr=198.51.100.0%2F24&reason=flood&hours=2"),
        Some(SITE),
    )
    .await;
    assert_eq!(timed.location.as_deref(), Some("/admin?done=block"));
    let block = h
        .wallet
        .db
        .blocks()
        .await
        .unwrap()
        .into_iter()
        .find(|block| block.cidr == "198.51.100.0/24")
        .unwrap();
    assert!(block.expires_at.unwrap() > now() + 7_000);
    let bad_hours = post(
        &h.app,
        "/admin/blocks",
        Some(&cookie),
        &format!("csrf={csrf}&cidr=198.51.100.0%2F24&reason=flood&hours=soon"),
        Some(SITE),
    )
    .await;
    assert_eq!(bad_hours.location.as_deref(), Some("/admin?error=hours"));
}

#[tokio::test]
async fn the_operator_sees_the_busiest_addresses_and_blocks_them_in_one_click() {
    let h = behind_proxy(|_| {}).await;
    for name in ["alice", "bob", "carol"] {
        let form = sign_up_form(&h.app, name).await;
        let reply = post_from(&h.app, "/signup", "203.0.113.9", None, &form).await;
        assert_eq!(reply.location.as_deref(), Some("/wallet"), "{}", reply.body);
    }
    let form = sign_up_form(&h.app, "dave").await;
    post_from(&h.app, "/signup", "2001:db8:7:42::1", None, &form).await;
    let alice = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    for _ in 0..2 {
        get_from(&h.app, "/lnurlp/alice/callback?amount=1000", "198.51.100.3").await;
    }
    h.wallet.faucet(&alice, &random_token(), "203.0.113.9").await.unwrap();

    let (cookie, csrf) = operator(&h.app).await;
    let page = get(&h.app, "/admin", Some(&cookie)).await;
    assert!(page.body.contains("Busiest addresses (24 h)"));
    assert!(page.body.contains("Block 203.0.113.0/24"), "{}", page.body);
    assert!(page.body.contains("Block 2001:db8:7::/56"), "{}", page.body);
    assert!(page.body.contains("Block 198.51.100.0/24"), "LNURL invoices");
    // The account list shows where each account signed up.
    assert!(page.body.contains("<code>203.0.113.9</code>"));
    let search = get(&h.app, "/admin?q=2001%3Adb8%3A7", Some(&cookie)).await;
    assert!(
        search.body.contains("dave") && !search.body.contains(">alice<"),
        "{}",
        search.body
    );

    let quick = post(
        &h.app,
        "/admin/blocks",
        Some(&cookie),
        &format!("csrf={csrf}&cidr=203.0.113.0%2F24&reason=Busy%3A+3+sign-ups+in+24+h&hours=24"),
        Some(SITE),
    )
    .await;
    assert_eq!(quick.location.as_deref(), Some("/admin?done=block"));
    let page = get(&h.app, "/admin", Some(&cookie)).await;
    assert!(!page.body.contains("Block 203.0.113.0/24"), "already blocked");
    let block = &h.wallet.db.blocks().await.unwrap()[0];
    assert!(block.expires_at.is_some_and(|at| at > now() + 23 * 3600));
    assert_eq!(
        get_from(&h.app, "/", "203.0.113.77").await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_frozen_account_is_shut_out_everywhere() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    let alice = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    let pubkey = signed_event(9, "https://wallet.example.org/auth/nostr", "x", now()).pubkey;
    h.wallet.db.set_nostr(alice.id, Some(&pubkey)).await.unwrap();
    h.fund(&alice, 5_000).await;
    let open = h.wallet.create_invoice(&alice, 1_000_000, "", false).await.unwrap();

    let (operator, csrf) = operator(&h.app).await;
    let frozen = post(
        &h.app,
        &format!("/admin/accounts/{}/freeze", alice.id),
        Some(&operator),
        &format!("csrf={csrf}&frozen=1"),
        Some(SITE),
    )
    .await;
    assert_eq!(frozen.location.as_deref(), Some("/admin?done=freeze"));

    // Signed out, and no way back in.
    assert_eq!(
        get(&h.app, "/wallet", Some(&cookie)).await.location.as_deref(),
        Some("/login")
    );
    let password = post(
        &h.app,
        "/login",
        None,
        "username=alice&password=correct+horse+battery",
        Some(SITE),
    )
    .await;
    assert!(password.body.contains("frozen"), "{}", password.body);
    assert!(password.cookie.is_none());
    let event = nostr_event(&h.app, 9).await;
    let nostr = post_json(
        &h.app,
        "/auth/nostr",
        &json!({ "mode": "login", "event": event_json(&event) }),
    )
    .await;
    assert_eq!(nostr.status, StatusCode::FORBIDDEN, "{}", nostr.body);

    // Nothing more arrives: open invoices are canceled, LNURL refuses.
    assert!(h.lnd.lock().canceled.contains(&open.payment_hash));
    assert_eq!(
        h.wallet.db.invoice(&open.payment_hash).await.unwrap().unwrap().state,
        "canceled"
    );
    let lnurl = get(&h.app, "/.well-known/lnurlp/alice", None).await;
    assert_eq!(lnurl_status(&lnurl), "ERROR");
    let callback = get(&h.app, "/lnurlp/alice/callback?amount=1000", None).await;
    assert_eq!(lnurl_status(&callback), "ERROR");

    // And nothing leaves: invoices, sends, the faucet, and payments to it.
    let alice = h.wallet.db.account(alice.id).await.unwrap().unwrap();
    assert!(matches!(
        h.wallet.create_invoice(&alice, 1_000, "", false).await,
        Err(WalletError::Frozen)
    ));
    let (bolt11, _) = h.lnd.external_invoice(1_000, None);
    assert!(matches!(
        h.wallet.pay(&alice, Harness::pay_request(&bolt11, None)).await,
        Err(WalletError::Frozen)
    ));
    assert!(matches!(
        h.wallet.faucet(&alice, &random_token(), "203.0.113.1").await,
        Err(WalletError::Frozen)
    ));
    let bob = h.account("bob").await;
    h.fund(&bob, 1_000).await;
    let to_alice = h
        .wallet
        .pay(&bob, Harness::pay_request("alice@wallet.example.org", Some(10)))
        .await;
    assert!(to_alice.is_err());
    assert_eq!(h.sats(&alice).await, 5_000);
}

#[tokio::test]
async fn the_faucet_has_a_crowd_sized_cap_per_address() {
    let h = harness_with(|config| {
        config.faucet.per_account_daily_sat = 10_000;
        config.faucet.per_address_daily_sat = 20_000;
        config.faucet.global_daily_sat = 100_000;
    })
    .await;
    let venue = "203.0.113.9";
    let (alice, bob, carol) = (
        h.account("alice").await,
        h.account("bob").await,
        h.account("carol").await,
    );
    h.wallet.faucet(&alice, &random_token(), venue).await.unwrap();
    h.wallet.faucet(&bob, &random_token(), venue).await.unwrap();
    let capped = h.wallet.faucet(&carol, &random_token(), venue).await.unwrap_err();
    assert!(capped.to_string().contains("Your network"), "{capped}");
    // Another network still gets its share, and per-account caps still apply.
    h.wallet
        .faucet(&carol, &random_token(), "2001:db8:9::/56")
        .await
        .unwrap();
    let again = h
        .wallet
        .faucet(&carol, &random_token(), "198.51.100.1")
        .await
        .unwrap_err();
    assert!(again.to_string().contains("today's faucet allowance"), "{again}");
    assert_eq!(h.sats(&carol).await, 10_000);
    assert!(
        metrics(&h.app)
            .await
            .contains("satchel_faucet_paid_msat_total 30000000\n")
    );
}

#[tokio::test]
async fn accounts_hold_a_limited_number_of_open_invoices() {
    let h = harness_with(|config| config.limits.max_open_invoices = 2).await;
    let alice = h.account("alice").await;
    let first = h.wallet.create_invoice(&alice, 1_000, "", false).await.unwrap();
    h.wallet.create_invoice(&alice, 1_000, "", false).await.unwrap();
    assert!(matches!(
        h.wallet.create_invoice(&alice, 1_000, "", false).await,
        Err(WalletError::TooManyInvoices)
    ));
    // Strangers asking through the address cannot use up the owner's own allowance, and vice versa.
    for _ in 0..2 {
        let reply = get(&h.app, "/lnurlp/alice/callback?amount=1000", None).await;
        assert!(reply.body.contains("\"pr\""), "{}", reply.body);
    }
    let full = get(&h.app, "/lnurlp/alice/callback?amount=1000", None).await;
    assert_eq!(lnurl_status(&full), "ERROR");
    assert!(full.body.contains("too many unpaid invoices"), "{}", full.body);
    // A paid invoice frees its place.
    let update = h.lnd.pay(&first.payment_hash);
    h.wallet.credit_settled(&update).await.unwrap();
    h.wallet.create_invoice(&alice, 1_000, "", false).await.unwrap();
}

async fn metrics(app: &Shared) -> String {
    let response = web::metrics_router(app.clone())
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    read(response).await.body
}
