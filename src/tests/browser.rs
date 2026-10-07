//! Local, fake-money fixture for checking the wallet in a browser.
//! Run `cargo test browser_fixture -- --ignored --nocapture`, then log in at
//! http://127.0.0.1:18097 with preview / preview-only-password.

#[tokio::test]
#[ignore = "serves the browser fixture until interrupted"]
async fn browser_fixture() {
    let h = super::harness_with(|config| {
        config.server.public_url = "http://127.0.0.1:18097".into();
        config.server.network_name = Some("Mutinynet".into());
        config.server.recovery_url = Some("/recover/".into());
    })
    .await;
    let hash = crate::auth::hash_password("preview-only-password").unwrap();
    let account = h.wallet.db.create_account("preview", Some(&hash), None).await.unwrap();
    h.account("friend").await;
    h.fund(&account, 12500).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:18097").await.unwrap();
    println!("Browser fixture: http://127.0.0.1:18097 (fake Lightning node)");
    axum::serve(
        listener,
        crate::web::router(h.app.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}
