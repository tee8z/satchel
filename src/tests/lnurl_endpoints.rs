//! LNURL-pay discovery and callbacks over HTTP, and crediting through the invoice stream.

use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use super::{harness, harness_with};
use crate::util::sha256;
use crate::web;
use crate::web::lnurlp_parse_callback_query as parse_callback_query;

async fn get_json(app: &crate::web::Shared, uri: &str) -> (StatusCode, Value, Option<String>) {
    let response = web::router(app.clone())
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let cors = response
        .headers()
        .get("access-control-allow-origin")
        .map(|value| value.to_str().unwrap().to_owned());
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap(), cors)
}

#[tokio::test]
async fn discovery_describes_each_account() {
    let h = harness().await;
    h.account("alice").await;
    let (status, params, cors) = get_json(&h.app, "/.well-known/lnurlp/alice").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cors.as_deref(), Some("*"));
    assert_eq!(params["tag"], "payRequest");
    assert_eq!(params["callback"], "https://wallet.example.org/lnurlp/alice/callback");
    assert_eq!(params["minSendable"], 1_000);
    assert_eq!(params["maxSendable"], 500_000_000);
    assert_eq!(params["commentAllowed"], 140);
    let metadata: Vec<[String; 2]> = serde_json::from_str(params["metadata"].as_str().unwrap()).unwrap();
    assert!(metadata.contains(&["text/identifier".to_owned(), "alice@wallet.example.org".to_owned()]));
    let (_, unknown, _) = get_json(&h.app, "/.well-known/lnurlp/nobody").await;
    assert_eq!(unknown["status"], "ERROR");
}

#[tokio::test]
async fn callbacks_create_invoices_that_commit_to_the_metadata() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let (_, params, _) = get_json(&h.app, "/.well-known/lnurlp/alice").await;
    let metadata = params["metadata"].as_str().unwrap().to_owned();
    let (status, reply, _) = get_json(
        &h.app,
        "/lnurlp/alice/callback?amount=21000&comment=good%20luck&nonce=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reply["routes"], serde_json::json!([]));
    let bolt11 = reply["pr"].as_str().unwrap();
    let (payment_hash, invoice) = h
        .lnd
        .lock()
        .invoices
        .iter()
        .find(|(_, invoice)| invoice.bolt11 == bolt11)
        .map(|(hash, invoice)| (hash.clone(), invoice.clone()))
        .unwrap();
    assert_eq!(invoice.amount_msat, 21_000);
    assert_eq!(invoice.description_hash, Some(sha256(metadata.as_bytes())));
    let stored = h.wallet.db.invoice(&payment_hash).await.unwrap().unwrap();
    assert_eq!(
        (stored.account_id, stored.source.as_str(), stored.memo.as_str()),
        (alice.id, "lnurl", "good luck")
    );

    for bad in [
        "/lnurlp/alice/callback",
        "/lnurlp/alice/callback?amount=",
        "/lnurlp/alice/callback?amount=0",
        "/lnurlp/alice/callback?amount=abc",
        "/lnurlp/alice/callback?amount=1000&amount=2000",
        "/lnurlp/alice/callback?amount=500000001",
        "/lnurlp/nobody/callback?amount=1000",
    ] {
        let (_, reply, _) = get_json(&h.app, bad).await;
        assert_eq!(reply["status"], "ERROR", "{bad} should fail");
    }
    let long = format!("/lnurlp/alice/callback?amount=1000&comment={}", "x".repeat(141));
    assert_eq!(get_json(&h.app, &long).await.1["status"], "ERROR");

    h.wallet.db.set_frozen(alice.id, true).await.unwrap();
    assert_eq!(get_json(&h.app, "/.well-known/lnurlp/alice").await.1["status"], "ERROR");
    assert_eq!(
        get_json(&h.app, "/lnurlp/alice/callback?amount=1000").await.1["status"],
        "ERROR"
    );
}

#[test]
fn callback_queries_parse_strictly() {
    assert_eq!(
        parse_callback_query(Some("amount=1999")).unwrap(),
        (1999, String::new())
    );
    assert_eq!(
        parse_callback_query(Some("comment=hi&amount=5")).unwrap(),
        (5, "hi".to_owned())
    );
    assert!(parse_callback_query(None).is_err());
    assert!(parse_callback_query(Some("amount=-1")).is_err());
    assert!(parse_callback_query(Some("amount=1.5")).is_err());
}

#[tokio::test]
async fn lnurl_requests_are_rate_limited_per_account() {
    let h = harness_with(|config| config.rate_limits.lnurl_per_account_per_minute = 2).await;
    h.account("alice").await;
    for _ in 0..2 {
        assert!(
            get_json(&h.app, "/lnurlp/alice/callback?amount=1000")
                .await
                .1
                .get("pr")
                .is_some()
        );
    }
    let (_, limited, _) = get_json(&h.app, "/lnurlp/alice/callback?amount=1000").await;
    assert_eq!(limited["status"], "ERROR");
}

#[tokio::test]
async fn the_invoice_stream_credits_payments_and_advances_its_cursor() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let (_, reply, _) = get_json(&h.app, "/lnurlp/alice/callback?amount=5000000").await;
    let bolt11 = reply["pr"].as_str().unwrap().to_owned();
    let payment_hash = h
        .lnd
        .lock()
        .invoices
        .iter()
        .find(|(_, invoice)| invoice.bolt11 == bolt11)
        .map(|(hash, _)| hash.clone())
        .unwrap();
    tokio::spawn(h.wallet.clone().follow_invoices());
    let update = h.lnd.pay(&payment_hash);
    h.lnd.stream.send(update.clone()).unwrap();
    // A replayed update after a reconnect must not credit twice.
    h.lnd.stream.send(update).unwrap();
    for _ in 0..100 {
        if h.sats(&alice).await == 5_000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.sats(&alice).await, 5_000);
    assert_eq!(h.wallet.db.cursor("lnd_invoice_settle_index").await.unwrap(), 1);

    // Reconciliation finds payments the stream missed.
    let invoice = h.wallet.create_invoice(&alice, 1_000_000, "", false).await.unwrap();
    h.lnd.pay(&invoice.payment_hash);
    h.wallet.reconcile().await;
    assert_eq!(h.sats(&alice).await, 6_000);
}

#[tokio::test]
async fn reconciliation_health_requires_a_complete_pass_and_credits_without_a_stream() {
    use std::sync::atomic::Ordering::Relaxed;
    let h = harness().await;
    let alice = h.account("alice").await;
    let invoice = h.wallet.create_invoice(&alice, 1_000_000, "", false).await.unwrap();
    h.lnd.pay(&invoice.payment_hash);
    h.lnd.lock().fail_lookup = true;
    h.wallet.reconcile().await;
    assert_eq!(h.wallet.metrics.reconciliation_last_success.load(Relaxed), 0);
    assert_eq!(h.wallet.metrics.reconciliation_failures.load(Relaxed), 1);
    assert!(h.wallet.metrics.reconciliation_last_attempt.load(Relaxed) > 0);
    assert_eq!(h.sats(&alice).await, 0);

    h.lnd.lock().fail_lookup = false;
    h.wallet.reconcile().await;
    h.wallet.reconcile().await;
    assert_eq!(h.sats(&alice).await, 1_000);
    assert_eq!(h.wallet.metrics.invoice_stream_state(), "disconnected");
    let success = h.wallet.metrics.reconciliation_last_success.load(Relaxed);
    assert!(success > 0);

    h.lnd.lock().fail_balances = true;
    h.wallet.reconcile().await;
    assert_eq!(h.wallet.metrics.reconciliation_last_success.load(Relaxed), success);
    assert_eq!(h.wallet.metrics.reconciliation_failures.load(Relaxed), 2);
    assert_eq!(h.sats(&alice).await, 1_000);
}
