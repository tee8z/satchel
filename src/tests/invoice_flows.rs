//! Invoice amount rules across loading, editing, confirmation, and receiving.
use axum::http::StatusCode;
use serde_json::Value;

use super::web_flows::{field, get, post, sign_up};
use super::{Harness, ORIGIN as SITE, harness, harness_with};
use crate::lnd::PaymentStatus;

fn amount_input(page: &str) -> &str {
    page.split("id=\"send-amount\"")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap()
}

#[tokio::test]
async fn fixed_invoice_amounts_are_loaded_locked_and_paid_exactly_once() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    let account = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    h.fund(&account, 10_000).await;
    let mut balance = 10_000_000;
    for (index, (msat, sats)) in [(2_100_000, "2100"), (2_100_123, "2100.123"), (1, "0.001")]
        .into_iter()
        .enumerate()
    {
        let (bolt11, _) = h.lnd.external_invoice(msat, None);
        let page = get(&h.app, &format!("/launch/lightning/{bolt11}"), Some(&cookie)).await;
        let input = amount_input(&page.body);
        assert!(input.contains(&format!("value=\"{sats}\"")), "{input}");
        assert!(input.contains("readonly"));
        assert!(
            !input.contains("name=\"amount_sat\""),
            "fixed amounts are not submitted as overrides"
        );
        let csrf = field(&page.body, "csrf");
        let key = field(&page.body, "key");
        let form = format!("csrf={csrf}&key={key}&destination={bolt11}&amount_sat=999");
        let decoded = post(&h.app, "/wallet/send/amount", Some(&cookie), &form, Some(SITE)).await;
        let decoded: Value = serde_json::from_str(&decoded.body).unwrap();
        assert_eq!(decoded["amount_sat"], sats);
        let review = post(&h.app, "/wallet/send/review", Some(&cookie), &form, Some(SITE)).await;
        assert!(review.body.contains("Confirm and send"), "{}", review.body);
        assert_eq!(field(&review.body, "amount_sat"), "");
        assert_eq!(h.wallet.db.balance(account.id).await.unwrap(), balance);
        assert_eq!(h.lnd.lock().sends.len(), index);
        let confirm = form.replace("amount_sat=999", "amount_sat=");
        let edited = post(&h.app, "/wallet/send/edit", Some(&cookie), &confirm, Some(SITE)).await;
        assert!(amount_input(&edited.body).contains("readonly"));
        assert!(amount_input(&edited.body).contains(&format!("value=\"{sats}\"")));
        for _ in 0..2 {
            let sent = post(&h.app, "/wallet/send", Some(&cookie), &confirm, Some(SITE)).await;
            assert!(sent.body.contains("Sent "), "{}", sent.body);
        }
        balance -= msat as i64;
        assert_eq!(h.wallet.db.balance(account.id).await.unwrap(), balance);
        assert_eq!(h.lnd.lock().sends.len(), index + 1);
        assert_eq!(h.lnd.lock().sends[index].amount_msat, None);
    }
    // An insufficient balance returns to a form with the authoritative amount.
    let (bolt11, _) = h.lnd.external_invoice(20_000_000, None);
    let page = get(&h.app, "/wallet/send", Some(&cookie)).await;
    let form = format!(
        "csrf={}&key={}&destination={bolt11}&amount_sat=1",
        field(&page.body, "csrf"),
        field(&page.body, "key")
    );
    let failed = post(&h.app, "/wallet/send", Some(&cookie), &form, Some(SITE)).await;
    assert!(failed.body.contains("Not enough sats"));
    assert!(amount_input(&failed.body).contains("value=\"20000\""));
    assert!(amount_input(&failed.body).contains("readonly"));
    assert_eq!(h.wallet.db.balance(account.id).await.unwrap(), balance);
}

#[tokio::test]
async fn amountless_invoices_require_positive_whole_sats_and_decode_never_pays() {
    let h = harness_with(|config| config.rate_limits.send_per_account_per_minute = 100).await;
    let cookie = sign_up(&h.app, "alice").await;
    let account = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    h.fund(&account, 100).await;
    h.account("bob").await;
    let (bolt11, _) = h.lnd.external_invoice(0, None);
    let page = get(&h.app, &format!("/launch/lightning/{bolt11}"), Some(&cookie)).await;
    assert!(!amount_input(&page.body).contains("readonly"));
    let csrf = field(&page.body, "csrf");
    let key = field(&page.body, "key");
    for destination in [&bolt11, "bob@wallet.example.org"] {
        let base = format!("csrf={csrf}&key={key}&destination={destination}");
        let decoded = post(&h.app, "/wallet/send/amount", Some(&cookie), &base, Some(SITE)).await;
        assert_eq!(
            serde_json::from_str::<Value>(&decoded.body).unwrap()["amount_sat"],
            Value::Null
        );
        for amount in ["1.001", "0.5", "-1", "18446744073709551615"] {
            for path in ["/wallet/send/review", "/wallet/send"] {
                let reply = post(
                    &h.app,
                    path,
                    Some(&cookie),
                    &format!("{base}&amount_sat={amount}"),
                    Some(SITE),
                )
                .await;
                assert!(
                    reply.body.contains("Enter a whole number of sats."),
                    "{amount}: {}",
                    reply.body
                );
            }
        }
    }
    let base = format!("csrf={csrf}&key={key}&destination={bolt11}");
    for amount in ["", "0"] {
        let review = post(
            &h.app,
            "/wallet/send/review",
            Some(&cookie),
            &format!("{base}&amount_sat={amount}"),
            Some(SITE),
        )
        .await;
        assert!(!review.body.contains("Confirm and send"));
    }
    assert!(h.lnd.lock().sends.is_empty());
    let form = format!("{base}&amount_sat=21");
    let review = post(&h.app, "/wallet/send/review", Some(&cookie), &form, Some(SITE)).await;
    assert!(review.body.contains("Confirm and send"), "{}", review.body);
    let sent = post(&h.app, "/wallet/send", Some(&cookie), &form, Some(SITE)).await;
    assert!(sent.body.contains("Sent 21 sats"));
    assert_eq!(h.lnd.lock().sends[0].amount_msat, Some(21_000));
    assert_eq!(h.sats(&account).await, 79);
    let forged = base.replace(&csrf, "forged");
    assert_eq!(
        post(&h.app, "/wallet/send/amount", Some(&cookie), &forged, Some(SITE))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(&h.app, "/wallet/send/amount", None, &base, Some(SITE))
            .await
            .location
            .as_deref(),
        Some("/login")
    );
}

#[tokio::test]
async fn receive_inputs_require_whole_sats_and_generated_invoices_round_up() {
    let h = harness().await;
    let cookie = sign_up(&h.app, "alice").await;
    let account = h.wallet.db.account_by_username("alice").await.unwrap().unwrap();
    let page = get(&h.app, "/wallet/receive", Some(&cookie)).await;
    let csrf = field(&page.body, "csrf");
    for amount in ["1.001", "0.5", "-1", "18446744073709551615"] {
        let reply = post(
            &h.app,
            "/wallet/receive",
            Some(&cookie),
            &format!("csrf={csrf}&amount_sat={amount}"),
            Some(SITE),
        )
        .await;
        assert!(reply.body.contains("Enter a whole number of sats."));
    }
    assert!(h.lnd.lock().invoices.is_empty());
    let mut balance = 0;
    for (requested, expected) in [(1, 1000), (1999, 2000), (2000, 2000)] {
        let invoice = h
            .wallet
            .create_invoice(&account, requested, "rounding", false)
            .await
            .unwrap();
        assert_eq!(invoice.amount_msat, expected);
        assert_eq!(
            h.lnd.lock().invoices[&invoice.payment_hash].amount_msat,
            expected as u64
        );
        assert_eq!(
            h.wallet
                .db
                .invoice(&invoice.payment_hash)
                .await
                .unwrap()
                .unwrap()
                .amount_msat,
            expected
        );
        let paid = h.lnd.pay(&invoice.payment_hash);
        h.wallet.credit_settled(&paid).await.unwrap();
        h.wallet.credit_settled(&paid).await.unwrap();
        balance += expected;
        assert_eq!(h.wallet.db.balance(account.id).await.unwrap(), balance);
    }
    let count = h.lnd.lock().invoices.len();
    for invalid in [0, 500_000_001, u64::MAX] {
        assert!(h.wallet.create_invoice(&account, invalid, "", false).await.is_err());
    }
    assert_eq!(h.lnd.lock().invoices.len(), count);
    assert_eq!(h.wallet.db.balance(account.id).await.unwrap(), balance);
}

#[tokio::test]
async fn rounding_cannot_exceed_balance_room_and_lnurl_keeps_exact_amounts() {
    let h = harness_with(|config| {
        config.limits.max_balance_sat = 5;
        config.limits.max_receive_sat = 5;
    })
    .await;
    let account = h.account("alice").await;
    h.fund(&account, 5).await;
    let (bolt11, _) = h.lnd.external_invoice(1999, None);
    h.lnd.lock().send_result = Some(PaymentStatus::Succeeded { fee_msat: 0 });
    h.wallet
        .pay(&account, Harness::pay_request(&bolt11, None))
        .await
        .unwrap();
    assert_eq!(h.wallet.db.balance(account.id).await.unwrap(), 3001);
    assert_eq!(h.wallet.receivable(&account).await.unwrap(), Some((1000, 1000)));
    let count = h.lnd.lock().invoices.len();
    assert!(h.wallet.create_invoice(&account, 1001, "", false).await.is_err());
    let fractional = get(&h.app, "/lnurlp/alice/callback?amount=1001", None).await;
    let fractional: Value = serde_json::from_str(&fractional.body).unwrap();
    assert_eq!(fractional["status"], "ERROR");
    assert!(fractional["reason"].as_str().unwrap().contains("whole number of sats"));
    assert_eq!(h.lnd.lock().invoices.len(), count);
    let params = get(&h.app, "/.well-known/lnurlp/alice", None).await;
    assert_eq!(
        serde_json::from_str::<Value>(&params.body).unwrap()["maxSendable"],
        1000
    );
    let exact = get(&h.app, "/lnurlp/alice/callback?amount=1000", None).await;
    let bolt11 = serde_json::from_str::<Value>(&exact.body).unwrap()["pr"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(h.wallet.decode(&bolt11).await.unwrap().num_msat, 1000);
    assert_eq!(h.wallet.db.balance(account.id).await.unwrap(), 3001);
}
