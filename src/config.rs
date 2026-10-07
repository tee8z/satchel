//! The TOML configuration file. Amounts are in sats here and in msat at runtime.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) server: Server,
    pub(crate) lnd: Lnd,
    #[serde(default)]
    pub(crate) limits: Limits,
    #[serde(default)]
    pub(crate) faucet: Faucet,
    #[serde(default)]
    pub(crate) rate_limits: RateLimits,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Server {
    /// Private HTTP listener; put an HTTPS reverse proxy in front of it.
    pub(crate) bind_address: SocketAddr,
    /// Public origin. Lightning Addresses use its host: `alice@<host>`.
    pub(crate) public_url: String,
    pub(crate) database_path: PathBuf,
    /// Optional private listener for `/metrics` and `/healthz`.
    #[serde(default)]
    pub(crate) metrics_address: Option<SocketAddr>,
    /// Header holding the client address when a trusted proxy sets it,
    /// for example `x-forwarded-for` (the right-most entry is used).
    #[serde(default)]
    pub(crate) client_ip_header: Option<String>,
    /// File with the operator's argon2 password hash (`koerier-wallet hash-password`).
    /// `KOERIER_WALLET_ADMIN_PASSWORD_HASH` overrides it. No hash, no admin pages.
    #[serde(default)]
    pub(crate) admin_password_hash_file: Option<PathBuf>,
    /// Usernames nobody may register, on top of the built-in list.
    #[serde(default)]
    pub(crate) reserved_usernames: Vec<String>,
    #[serde(default = "default_session_days")]
    pub(crate) session_days: u32,
    /// Allow LNURL requests to loopback and private addresses (local regtest only).
    #[serde(default)]
    pub(crate) allow_private_lnurl_hosts: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lnd {
    pub(crate) rest_host: SocketAddr,
    pub(crate) tls_cert_path: PathBuf,
    pub(crate) macaroon_path: PathBuf,
    #[serde(default = "default_request_timeout")]
    pub(crate) request_timeout_secs: u64,
    #[serde(default = "default_payment_timeout")]
    pub(crate) payment_timeout_secs: u32,
    /// Refuse to start unless LND reports exactly this network (for example `signet`).
    #[serde(default)]
    pub(crate) expected_network: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Limits {
    pub(crate) max_balance_sat: u64,
    pub(crate) max_payment_sat: u64,
    pub(crate) min_receive_sat: u64,
    pub(crate) max_receive_sat: u64,
    pub(crate) invoice_expiry_secs: u32,
    /// Routing fee budget in parts per million of the amount ...
    pub(crate) fee_limit_ppm: u64,
    /// ... but never less than this many sats.
    pub(crate) min_fee_limit_sat: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_balance_sat: 1_000_000,
            max_payment_sat: 250_000,
            min_receive_sat: 1,
            max_receive_sat: 250_000,
            invoice_expiry_secs: 3600,
            fee_limit_ppm: 10_000,
            min_fee_limit_sat: 10,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Faucet {
    pub(crate) enabled: bool,
    pub(crate) amount_sat: u64,
    /// Rolling 24-hour limits.
    pub(crate) per_account_daily_sat: u64,
    pub(crate) global_daily_sat: u64,
}

impl Default for Faucet {
    fn default() -> Self {
        Self {
            enabled: false,
            amount_sat: 10_000,
            per_account_daily_sat: 20_000,
            global_daily_sat: 500_000,
        }
    }
}

/// Attempts allowed per client address or account in each window.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct RateLimits {
    pub(crate) login_per_ip_per_minute: u32,
    pub(crate) login_per_account_per_hour: u32,
    pub(crate) signup_per_ip_per_hour: u32,
    pub(crate) lnurl_per_ip_per_minute: u32,
    pub(crate) lnurl_per_account_per_minute: u32,
    pub(crate) send_per_account_per_minute: u32,
    pub(crate) receive_per_account_per_minute: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            login_per_ip_per_minute: 10,
            login_per_account_per_hour: 30,
            signup_per_ip_per_hour: 5,
            lnurl_per_ip_per_minute: 60,
            lnurl_per_account_per_minute: 30,
            send_per_account_per_minute: 10,
            receive_per_account_per_minute: 20,
        }
    }
}

fn default_session_days() -> u32 {
    14
}
fn default_request_timeout() -> u64 {
    10
}
fn default_payment_timeout() -> u32 {
    60
}

/// Largest msat amount that survives a round trip through a JSON number.
const MAX_SAFE_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

impl Config {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("cannot read configuration file")?;
        let config: Self = toml::from_str(&text).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        public_origin(&self.server.public_url)?;
        let limits = &self.limits;
        let msat = |sats: u64| sats.checked_mul(1000).filter(|value| *value <= MAX_SAFE_JSON_INTEGER);
        for (name, value) in [
            ("max_balance_sat", limits.max_balance_sat),
            ("max_payment_sat", limits.max_payment_sat),
            ("min_receive_sat", limits.min_receive_sat),
            ("max_receive_sat", limits.max_receive_sat),
        ] {
            if value == 0 || msat(value).is_none() {
                bail!("limits.{name} must be positive and below 2^53 msat");
            }
        }
        if limits.min_receive_sat > limits.max_receive_sat {
            bail!("limits.min_receive_sat exceeds limits.max_receive_sat");
        }
        if limits.invoice_expiry_secs == 0 || limits.fee_limit_ppm > 1_000_000 {
            bail!("limits.invoice_expiry_secs must be positive and fee_limit_ppm at most 1000000");
        }
        if self.faucet.enabled
            && (self.faucet.amount_sat == 0
                || msat(self.faucet.amount_sat).is_none()
                || self.faucet.amount_sat > self.faucet.per_account_daily_sat
                || self.faucet.per_account_daily_sat > self.faucet.global_daily_sat
                || msat(self.faucet.global_daily_sat).is_none())
        {
            bail!("faucet amounts must satisfy 0 < amount_sat <= per_account_daily_sat <= global_daily_sat");
        }
        if self.lnd.request_timeout_secs == 0 || self.lnd.payment_timeout_secs == 0 || self.server.session_days == 0 {
            bail!("timeouts and session_days must be positive");
        }
        Ok(())
    }
}

/// The public origin must be HTTPS, except plain HTTP on a loopback host for local development.
pub(crate) fn public_origin(public_url: &str) -> Result<Url> {
    let url = Url::parse(public_url).context("invalid server.public_url")?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("server.public_url must be an HTTPS origin without credentials, path, query, or fragment");
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules() {
        assert!(public_origin("https://wallet.example.org").is_ok());
        assert!(public_origin("http://127.0.0.1:8095").is_ok());
        assert!(public_origin("http://wallet.example.org").is_err());
        assert!(public_origin("https://wallet.example.org/path").is_err());
        assert!(public_origin("https://user@wallet.example.org").is_err());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../example/config.toml.example");
        let config: Config = toml::from_str(text).unwrap();
        config.validate().unwrap();
        assert!(!config.faucet.enabled || config.faucet.amount_sat > 0);
    }
}
