//! Integration tests: the wallet, ledger, LNURL endpoints, and web flows
//! against a real SQLite database and an in-memory LND.

mod handoff_flows;
mod ledger;
mod lnurl_endpoints;
mod mock;
mod web_flows;

use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use url::Url;

use crate::auth;
use crate::config::Config;
use crate::db::{Account, test_db};
use crate::lnd::DecodedInvoice;
use crate::util::random_token;
use crate::wallet::{PayRequest, Wallet};
use crate::web::{App, Shared};
pub(crate) use mock::MockLnd;

pub(crate) const ORIGIN: &str = "https://wallet.example.org";

pub(crate) fn test_config() -> Config {
    let config: Config = toml::from_str(
        r#"
        [server]
        bind_address = "127.0.0.1:0"
        public_url = "https://wallet.example.org"
        database_path = "unused.db"

        [lnd]
        rest_host = "127.0.0.1:8080"
        tls_cert_path = "tls.cert"
        macaroon_path = "wallet.macaroon"

        [limits]
        max_balance_sat = 1_000_000
        max_payment_sat = 500_000
        min_receive_sat = 1
        max_receive_sat = 500_000
        fee_limit_ppm = 0
        min_fee_limit_sat = 0

        [faucet]
        enabled = true
        amount_sat = 10_000
        per_account_daily_sat = 20_000
        global_daily_sat = 30_000
        "#,
    )
    .unwrap();
    config.validate().unwrap();
    config
}

pub(crate) struct Harness {
    pub(crate) wallet: Arc<Wallet>,
    pub(crate) lnd: Arc<MockLnd>,
    pub(crate) app: Shared,
    _dir: TempDir,
}

pub(crate) async fn harness() -> Harness {
    harness_with(|_| {}).await
}

pub(crate) async fn harness_with(adjust: impl FnOnce(&mut Config)) -> Harness {
    let mut config = test_config();
    adjust(&mut config);
    config.validate().unwrap();
    let (db, dir) = test_db().await;
    let lnd = MockLnd::new("signet");
    let mut wallet = Wallet::new(db, lnd.clone(), "signet".into(), &config, Url::parse(ORIGIN).unwrap());
    wallet.send_wait = Duration::from_secs(5);
    let wallet = Arc::new(wallet);
    let admin = auth::hash_password("operator password").unwrap();
    let app = Arc::new(App::new(Arc::clone(&wallet), &config, Some(admin)).unwrap());
    Harness {
        wallet,
        lnd,
        app,
        _dir: dir,
    }
}

impl Harness {
    pub(crate) async fn account(&self, name: &str) -> Account {
        self.wallet
            .db
            .create_account(name, Some("not-a-real-hash"), None)
            .await
            .unwrap()
    }

    /// Credits sats as if someone paid the account's invoice over Lightning.
    pub(crate) async fn fund(&self, account: &Account, sats: u64) {
        let invoice = self
            .wallet
            .create_invoice(account, sats * 1000, "funding", false)
            .await
            .unwrap();
        let update = self.lnd.pay(&invoice.payment_hash);
        self.wallet.credit_settled(&update).await.unwrap();
    }

    /// Balance in whole sats.
    pub(crate) async fn sats(&self, account: &Account) -> i64 {
        self.wallet.db.balance(account.id).await.unwrap() / 1000
    }

    pub(crate) fn pay_request(destination: &str, amount_sat: Option<u64>) -> PayRequest {
        PayRequest {
            request_key: random_token(),
            destination: destination.to_owned(),
            amount_msat: amount_sat.map(|sats| sats * 1000),
            comment: String::new(),
        }
    }

    pub(crate) fn decoded(&self, bolt11: &str) -> Option<DecodedInvoice> {
        self.lnd.lock().external.get(bolt11).cloned()
    }
}
