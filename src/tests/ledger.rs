//! Ledger invariants: exactly-once credits, no negative balances, idempotent
//! requests, refunds, reconciliation, internal transfers, and the faucet.

use std::sync::Arc;

use crate::error::WalletError;
use crate::lnd::{Lightning, PaymentStatus, require_test_network};
use crate::util::{now, random_token, sha256};

use super::{Harness, MockLnd, harness, harness_with};

#[tokio::test]
async fn startup_refuses_mainnet_nodes() {
    let mainnet = MockLnd::new("mainnet");
    let error = require_test_network(mainnet.as_ref() as &dyn Lightning, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("never on mainnet"), "{error}");
    let signet = MockLnd::new("signet");
    let (_, network) = require_test_network(signet.as_ref() as &dyn Lightning, Some("signet"))
        .await
        .unwrap();
    assert_eq!(network, "signet");
    assert!(
        require_test_network(signet.as_ref() as &dyn Lightning, Some("testnet"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_settled_invoice_credits_exactly_once() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let invoice = h
        .wallet
        .create_invoice(&alice, 21_000_000, "coffee", false)
        .await
        .unwrap();
    let update = h.lnd.pay(&invoice.payment_hash);
    for _ in 0..3 {
        h.wallet.credit_settled(&update).await.unwrap();
    }
    assert!(
        h.wallet
            .db
            .settle_invoice(&invoice.payment_hash, 21_000_000)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.sats(&alice).await, 21_000);
    let history = h.wallet.db.history(alice.id, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].memo, "coffee");
    // Invoices this server did not create are ignored.
    let mut stranger = update.clone();
    stranger.r_hash = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [9u8; 32]);
    h.wallet.credit_settled(&stranger).await.unwrap();
    assert_eq!(h.sats(&alice).await, 21_000);
}

#[tokio::test]
async fn the_ledger_is_append_only_and_never_negative() {
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 1_000).await;
    let payment_id = h.wallet.db.history(alice.id, 1).await.unwrap()[0].id;
    let update = sqlx::query("UPDATE ledger SET amount_msat = 5")
        .execute(&h.wallet.db.write)
        .await;
    assert!(update.unwrap_err().to_string().contains("append-only"));
    let delete = sqlx::query("DELETE FROM ledger").execute(&h.wallet.db.write).await;
    assert!(delete.unwrap_err().to_string().contains("append-only"));
    let overdraw = sqlx::query(
        "INSERT INTO ledger (account_id, amount_msat, kind, payment_id, idempotency_key, created_at) \
         VALUES (?, -1000001, 'debit', ?, 'overdraw', 0)",
    )
    .bind(alice.id)
    .bind(payment_id)
    .execute(&h.wallet.db.write)
    .await;
    assert!(matches!(
        WalletError::from(overdraw.unwrap_err()),
        WalletError::InsufficientBalance
    ));
    let duplicate = sqlx::query(
        "INSERT INTO ledger (account_id, amount_msat, kind, payment_id, idempotency_key, created_at) \
         VALUES (?, 1, 'credit', ?, ?, 0)",
    )
    .bind(alice.id)
    .bind(payment_id)
    .bind(format!("payment:{payment_id}:credit"))
    .execute(&h.wallet.db.write)
    .await;
    assert!(
        duplicate
            .unwrap_err()
            .as_database_error()
            .unwrap()
            .is_unique_violation()
    );
    assert_eq!(h.sats(&alice).await, 1_000);
}

#[tokio::test]
async fn sending_more_than_the_balance_is_refused_without_touching_lnd() {
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 1_000).await;
    let (bolt11, _) = h.lnd.external_invoice(2_000_000, None);
    let error = h
        .wallet
        .pay(&alice, Harness::pay_request(&bolt11, None))
        .await
        .unwrap_err();
    assert!(matches!(error, WalletError::InsufficientBalance), "{error:?}");
    assert!(h.lnd.lock().sends.is_empty());
    assert_eq!(h.sats(&alice).await, 1_000);
}

#[tokio::test]
async fn fees_are_capped_and_the_unused_budget_comes_back() {
    let h = harness_with(|config| {
        config.limits.fee_limit_ppm = 10_000;
        config.limits.min_fee_limit_sat = 10;
    })
    .await;
    let alice = h.account("alice").await;
    h.fund(&alice, 100_000).await;
    h.lnd.lock().send_result = Some(PaymentStatus::Succeeded { fee_msat: 2_000 });
    let (bolt11, _) = h.lnd.external_invoice(50_000_000, None);
    assert!(h.decoded(&bolt11).is_some());
    let payment = h.wallet.pay(&alice, Harness::pay_request(&bolt11, None)).await.unwrap();
    assert_eq!(payment.status, "succeeded");
    assert_eq!(payment.fee_msat, 2_000);
    // The budget was 1% (500 sats); 2 sats were used.
    assert_eq!(h.lnd.lock().sends[0].fee_limit_msat, 500_000);
    assert_eq!(h.sats(&alice).await, 100_000 - 50_000 - 2);

    h.lnd.lock().send_result = Some(PaymentStatus::Failed {
        reason: "No route to the recipient was found.".into(),
    });
    let (bolt11, _) = h.lnd.external_invoice(10_000_000, None);
    let failed = h.wallet.pay(&alice, Harness::pay_request(&bolt11, None)).await.unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.failure.as_deref(), Some("No route to the recipient was found."));
    assert_eq!(h.sats(&alice).await, 100_000 - 50_000 - 2);
}

#[tokio::test]
async fn a_repeated_request_key_pays_once() {
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 10_000).await;
    let (bolt11, _) = h.lnd.external_invoice(1_000_000, None);
    let request = Harness::pay_request(&bolt11, None);
    let first = h.wallet.pay(&alice, request.clone()).await.unwrap();
    let second = h.wallet.pay(&alice, request).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(h.lnd.lock().sends.len(), 1);
    assert_eq!(h.sats(&alice).await, 9_000);
    // A new key for the same invoice is refused while the first payment stands.
    let again = h
        .wallet
        .pay(&alice, Harness::pay_request(&bolt11, None))
        .await
        .unwrap_err();
    assert!(matches!(again, WalletError::AlreadyPaid), "{again:?}");
}

#[tokio::test]
async fn concurrent_sends_never_overdraw() {
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 100_000).await;
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let (bolt11, _) = h.lnd.external_invoice(10_000_000, None);
        let wallet = Arc::clone(&h.wallet);
        let alice = alice.clone();
        tasks.push(tokio::spawn(async move {
            wallet.pay(&alice, Harness::pay_request(&bolt11, None)).await
        }));
    }
    let mut paid = 0;
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(payment) if payment.status == "succeeded" => paid += 1,
            Err(WalletError::InsufficientBalance) => refused += 1,
            other => panic!("unexpected result {other:?}"),
        }
    }
    assert_eq!((paid, refused), (10, 10));
    assert_eq!(h.sats(&alice).await, 0);
    assert_eq!(h.lnd.lock().sends.len(), 10);
}

#[tokio::test]
async fn unknown_outcomes_wait_for_reconciliation() {
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 10_000).await;
    h.lnd.lock().send_result = None;
    let (bolt11, _) = h.lnd.external_invoice(4_000_000, None);
    let pending = h.wallet.pay(&alice, Harness::pay_request(&bolt11, None)).await.unwrap();
    assert_eq!(pending.status, "pending");
    assert_eq!(h.sats(&alice).await, 6_000);
    // Still in flight at LND: nothing changes.
    h.lnd.lock().track_result = PaymentStatus::InFlight;
    h.wallet.reconcile_sends(now() + 1).await;
    assert_eq!(
        h.wallet.db.payment(alice.id, pending.id).await.unwrap().unwrap().status,
        "pending"
    );
    // LND finished it: the payment succeeds once.
    h.lnd.lock().track_result = PaymentStatus::Succeeded { fee_msat: 0 };
    h.wallet.reconcile_sends(now() + 1).await;
    h.wallet.reconcile_sends(now() + 1).await;
    assert_eq!(
        h.wallet.db.payment(alice.id, pending.id).await.unwrap().unwrap().status,
        "succeeded"
    );
    assert_eq!(h.sats(&alice).await, 6_000);

    // A payment LND never saw is refunded.
    let (bolt11, _) = h.lnd.external_invoice(3_000_000, None);
    let lost = h.wallet.pay(&alice, Harness::pay_request(&bolt11, None)).await.unwrap();
    assert_eq!(h.sats(&alice).await, 3_000);
    h.lnd.lock().track_result = PaymentStatus::NotFound;
    h.wallet.reconcile_sends(now() + 1).await;
    let lost = h.wallet.db.payment(alice.id, lost.id).await.unwrap().unwrap();
    assert_eq!(lost.status, "failed");
    assert_eq!(h.sats(&alice).await, 6_000);
}

#[tokio::test]
async fn paying_another_accounts_invoice_stays_inside_the_ledger() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let bob = h.account("bob").await;
    h.fund(&alice, 50_000).await;
    let invoice = h
        .wallet
        .create_invoice(&bob, 20_000_000, "ticket", false)
        .await
        .unwrap();
    let payment = h
        .wallet
        .pay(&alice, Harness::pay_request(&invoice.bolt11, None))
        .await
        .unwrap();
    assert_eq!(payment.status, "succeeded");
    assert_eq!(payment.kind, "internal");
    assert_eq!(payment.counterparty, "bob@wallet.example.org");
    assert!(
        h.lnd.lock().sends.is_empty(),
        "no Lightning payment for an internal invoice"
    );
    assert_eq!(h.lnd.lock().canceled, vec![invoice.payment_hash.clone()]);
    assert_eq!(h.sats(&alice).await, 30_000);
    assert_eq!(h.sats(&bob).await, 20_000);
    assert_eq!(
        h.wallet.db.invoice(&invoice.payment_hash).await.unwrap().unwrap().state,
        "settled"
    );
    // A late Lightning settlement report for the same hash cannot credit it again.
    let mut late = h.lnd.pay(&invoice.payment_hash);
    late.amt_paid_msat = 20_000_000;
    h.wallet.credit_settled(&late).await.unwrap();
    assert_eq!(h.sats(&bob).await, 20_000);
    // Paying it a second time is refused.
    let again = h.wallet.pay(&alice, Harness::pay_request(&invoice.bolt11, None)).await;
    assert!(again.is_err());
    assert_eq!(h.sats(&alice).await, 30_000);
}

#[tokio::test]
async fn a_frozen_account_cannot_be_paid_internally() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let bob = h.account("bob").await;
    h.fund(&alice, 10_000).await;
    let invoice = h.wallet.create_invoice(&bob, 1_000_000, "", false).await.unwrap();
    h.wallet.db.set_frozen(bob.id, true).await.unwrap();
    let refused = h
        .wallet
        .pay(&alice, Harness::pay_request(&invoice.bolt11, None))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("cannot receive"), "{refused}");
    assert!(h.lnd.lock().canceled.is_empty(), "the invoice stays open in LND");
    let by_address = h
        .wallet
        .pay(&alice, Harness::pay_request("bob@wallet.example.org", Some(1_000)))
        .await
        .unwrap_err();
    assert!(by_address.to_string().contains("cannot receive"), "{by_address}");
    assert_eq!((h.sats(&alice).await, h.sats(&bob).await), (10_000, 0));
}

#[tokio::test]
async fn an_address_on_this_server_is_paid_directly() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let bob = h.account("bob").await;
    h.fund(&alice, 10_000).await;
    let payment = h
        .wallet
        .pay(&alice, Harness::pay_request("Bob@Wallet.Example.org", Some(2_500)))
        .await
        .unwrap();
    assert_eq!(
        (payment.status.as_str(), payment.kind.as_str()),
        ("succeeded", "internal")
    );
    assert_eq!(h.sats(&alice).await, 7_500);
    assert_eq!(h.sats(&bob).await, 2_500);
    assert!(h.lnd.lock().sends.is_empty());
    let incoming = h.wallet.db.history(bob.id, 1).await.unwrap();
    assert_eq!(
        (incoming[0].direction.as_str(), incoming[0].counterparty.as_str()),
        ("in", "alice@wallet.example.org")
    );
    // Unknown names, yourself, and amounts over the balance are refused.
    assert!(
        h.wallet
            .pay(&alice, Harness::pay_request("carol@wallet.example.org", Some(1)))
            .await
            .is_err()
    );
    assert!(
        h.wallet
            .pay(&alice, Harness::pay_request("alice@wallet.example.org", Some(1)))
            .await
            .is_err()
    );
    let too_much = h
        .wallet
        .pay(&alice, Harness::pay_request("bob@wallet.example.org", Some(8_000)))
        .await
        .unwrap_err();
    assert!(matches!(too_much, WalletError::InsufficientBalance));
    assert_eq!(h.sats(&alice).await, 7_500);
}

#[tokio::test]
async fn own_invoices_mainnet_invoices_and_frozen_accounts_are_refused() {
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 10_000).await;
    let own = h.wallet.create_invoice(&alice, 1_000_000, "", false).await.unwrap();
    assert!(
        h.wallet
            .pay(&alice, Harness::pay_request(&own.bolt11, None))
            .await
            .is_err()
    );
    let mainnet = h
        .wallet
        .pay(&alice, Harness::pay_request("lnbc10u1pmainnet", None))
        .await
        .unwrap_err();
    assert!(mainnet.to_string().contains("mainnet"));
    h.wallet.db.set_frozen(alice.id, true).await.unwrap();
    let frozen = h.wallet.db.account(alice.id).await.unwrap().unwrap();
    let (bolt11, _) = h.lnd.external_invoice(1_000, None);
    assert!(matches!(
        h.wallet.pay(&frozen, Harness::pay_request(&bolt11, None)).await,
        Err(WalletError::Frozen)
    ));
    assert!(h.wallet.create_invoice(&frozen, 1_000, "", false).await.is_err());
    assert!(matches!(
        h.wallet.faucet(&frozen, &random_token()).await,
        Err(WalletError::Frozen)
    ));
}

#[tokio::test]
async fn the_balance_cap_limits_what_can_be_received() {
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 500_000).await;
    h.fund(&alice, 500_000).await;
    assert_eq!(h.wallet.receivable(&alice).await.unwrap(), None);
    assert!(h.wallet.create_invoice(&alice, 1_000, "", false).await.is_err());
}

#[tokio::test]
async fn the_faucet_keeps_to_its_limits_and_the_node_balance() {
    let h = harness().await;
    let alice = h.account("alice").await;
    let bob = h.account("bob").await;
    let key = random_token();
    h.wallet.faucet(&alice, &key).await.unwrap();
    // The same click again is the same grant.
    h.wallet.faucet(&alice, &key).await.unwrap();
    assert_eq!(h.sats(&alice).await, 10_000);
    h.wallet.faucet(&alice, &random_token()).await.unwrap();
    let daily = h.wallet.faucet(&alice, &random_token()).await.unwrap_err();
    assert!(daily.to_string().contains("today's faucet allowance"), "{daily}");
    h.wallet.faucet(&bob, &random_token()).await.unwrap();
    let global = h.wallet.faucet(&bob, &random_token()).await.unwrap_err();
    assert!(global.to_string().contains("today's sats"), "{global}");
    assert_eq!((h.sats(&alice).await, h.sats(&bob).await), (20_000, 10_000));

    let h = harness().await;
    let carol = h.account("carol").await;
    h.lnd.lock().channel_local_msat = 5_000_000;
    let empty = h.wallet.faucet(&carol, &random_token()).await.unwrap_err();
    assert!(empty.to_string().contains("empty"), "{empty}");
}

#[tokio::test]
async fn lightning_address_payments_check_the_invoice_they_get() {
    // The LNURL client refuses private hosts before any request, so an
    // external address on a loopback name never reaches the network.
    let h = harness().await;
    let alice = h.account("alice").await;
    h.fund(&alice, 1_000).await;
    let error = h
        .wallet
        .pay(
            &alice,
            Harness::pay_request("lnurlp://127.0.0.1/.well-known/lnurlp/x", Some(10)),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("private network"), "{error}");
    assert_eq!(h.sats(&alice).await, 1_000);
    // An invoice whose description hash does not match is detectable.
    let metadata = crate::lnurl::metadata("bob", "elsewhere.example");
    let (bolt11, _) = h.lnd.external_invoice(10_000, Some(sha256(b"something else")));
    let decoded = h.decoded(&bolt11).unwrap();
    assert_ne!(decoded.description_hash, hex::encode(sha256(metadata.as_bytes())));
}
