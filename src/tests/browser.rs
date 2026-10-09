//! Local, fake-money fixture for checking the wallet in a browser.
//! Run `cargo test browser_fixture -- --ignored --nocapture`, then log in at
//! http://127.0.0.1:18097 with preview / preview-only-password.

#[tokio::test]
#[ignore = "serves the browser fixture until interrupted"]
async fn browser_fixture() {
    let h = super::harness_with(|config| {
        config.server.public_url = "http://127.0.0.1:18097".into();
        config.server.network_name = Some("Mutinynet".into());
        config.rate_limits.send_per_account_per_minute = 100;
        config.server.recovery_url = Some("/recover/".into());
    })
    .await;
    let hash = crate::auth::hash_password("preview-only-password").unwrap();
    let account = h.wallet.db.create_account("preview", Some(&hash), None).await.unwrap();
    h.account("friend").await;
    h.fund(&account, 12500).await;
    let (fixed, _) = h.lnd.external_invoice(2_100_000, None);
    let (amountless, _) = h.lnd.external_invoice(0, None);
    let (fractional, _) = h.lnd.external_invoice(21_123, None);
    let invoices = serde_json::json!({ "fixed": fixed, "amountless": amountless, "fractional": fractional });
    let router = crate::web::router(h.app.clone()).route(
        "/fixture/invoices",
        axum::routing::get(move || {
            let mut fixture = invoices.clone();
            async move {
                let event = crate::nostr::tests::sign(
                    9,
                    crate::util::now(),
                    vec![
                        vec!["u".into(), "http://127.0.0.1:18097/auth/nostr/handoff".into()],
                        vec!["method".into(), "POST".into()],
                        vec!["nonce".into(), crate::util::random_token()],
                    ],
                );
                fixture["handoff"] = super::web_flows::event_json(&event);
                axum::Json(fixture)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18097").await.unwrap();
    println!("Browser fixture: http://127.0.0.1:18097 (fake Lightning node)");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}
