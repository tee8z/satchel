//! The TOML configuration file and `SATCHEL_<SECTION>__<KEY>` environment
//! overrides. Amounts are in sats here and in msat at runtime.

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
    /// File with the operator's argon2 password hash (`satchel hash-password`).
    /// `SATCHEL_ADMIN_PASSWORD_HASH` overrides it. No hash, no admin pages.
    #[serde(default)]
    pub(crate) admin_password_hash_file: Option<PathBuf>,
    /// Optional separate HTTPS origin for the operator pages, for example a
    /// VPN-only name. When set, `/admin` answers only on this host.
    #[serde(default)]
    pub(crate) operator_url: Option<String>,
    /// Usernames nobody may register, on top of the built-in list.
    #[serde(default)]
    pub(crate) reserved_usernames: Vec<String>,
    #[serde(default = "default_session_days")]
    pub(crate) session_days: u32,
    /// Allow LNURL requests to loopback and private addresses (local regtest only).
    #[serde(default)]
    pub(crate) allow_private_lnurl_hosts: bool,
    /// HTTPS origins of apps that hand signed-in users over (`POST /auth/nostr/handoff`)
    /// and may call `GET /api/v1/address` from the browser. Their handoffs skip
    /// the confirmation page; nothing else is skipped.
    #[serde(default)]
    pub(crate) handoff_origins: Vec<String>,
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

/// Prefix of environment variables that override single settings.
const ENV_PREFIX: &str = "SATCHEL_";

/// Applies `SATCHEL_<SECTION>__<KEY>=<value>` overrides, for example
/// `SATCHEL_SERVER__PUBLIC_URL` or `SATCHEL_FAUCET__ENABLED=true`. Values are
/// read as TOML (numbers, booleans, arrays) and otherwise as plain strings.
/// Variables without the double underscore, such as `SATCHEL_CONFIG`, are not settings.
fn apply_env(table: &mut toml::Table, vars: impl IntoIterator<Item = (String, String)>) -> Result<()> {
    for (name, raw) in vars {
        let Some((section, key)) = name.strip_prefix(ENV_PREFIX).and_then(|rest| rest.split_once("__")) else {
            continue;
        };
        if section.is_empty() || key.is_empty() {
            bail!("environment variable {name} must look like {ENV_PREFIX}<SECTION>__<KEY>");
        }
        let value = toml::from_str::<toml::Table>(&format!("value = {raw}"))
            .ok()
            .and_then(|mut parsed| parsed.remove("value"))
            .unwrap_or(toml::Value::String(raw));
        let section = section.to_ascii_lowercase();
        let entry = table
            .entry(section.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(settings) = entry.as_table_mut() else {
            bail!("environment variable {name}: [{section}] is not a table");
        };
        settings.insert(key.to_ascii_lowercase(), value);
    }
    Ok(())
}

impl Config {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("cannot read configuration file")?;
        // `std::env::vars` panics on non-UTF-8 variables; such variables are never settings.
        let vars =
            std::env::vars_os().filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)));
        Self::parse(&text, vars)
    }

    /// The TOML file with environment overrides applied, then validated.
    pub(crate) fn parse(text: &str, vars: impl IntoIterator<Item = (String, String)>) -> Result<Self> {
        let mut table: toml::Table = toml::from_str(text).context("invalid TOML configuration")?;
        apply_env(&mut table, vars)?;
        let config: Self = table.try_into().context("invalid configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        public_origin(&self.server.public_url)?;
        if let Some(operator_url) = &self.server.operator_url {
            public_origin(operator_url).context("invalid server.operator_url")?;
        }
        for origin in &self.server.handoff_origins {
            if public_origin(origin).is_err() {
                bail!("server.handoff_origins entries must be HTTPS origins without a path: {origin:?}");
            }
        }
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

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn environment_overrides_single_settings() {
        let text = include_str!("../example/config.toml.example");
        let config = Config::parse(
            text,
            vars(&[
                ("SATCHEL_SERVER__PUBLIC_URL", "https://pay.example.net"),
                ("SATCHEL_SERVER__BIND_ADDRESS", "127.0.0.1:9000"),
                ("SATCHEL_FAUCET__ENABLED", "false"),
                ("SATCHEL_LIMITS__MAX_BALANCE_SAT", "50_000"),
                ("SATCHEL_LND__EXPECTED_NETWORK", "regtest"),
                ("SATCHEL_CONFIG", "/ignored.toml"),
                ("OTHER__SETTING", "ignored"),
            ]),
        )
        .unwrap();
        assert_eq!(config.server.public_url, "https://pay.example.net");
        assert_eq!(config.server.bind_address.port(), 9000);
        assert!(!config.faucet.enabled);
        assert_eq!(config.limits.max_balance_sat, 50_000);
        assert_eq!(config.lnd.expected_network.as_deref(), Some("regtest"));
        // Unknown keys and invalid values are refused, as in the file.
        assert!(Config::parse(text, vars(&[("SATCHEL_SERVER__NO_SUCH_KEY", "1")])).is_err());
        assert!(Config::parse(text, vars(&[("SATCHEL_SERVER__PUBLIC_URL", "http://example.org")])).is_err());
        assert!(Config::parse(text, vars(&[("SATCHEL___KEY", "1")])).is_err());
        let operator = vars(&[("SATCHEL_SERVER__OPERATOR_URL", "https://wallet-admin.example.org:9443")]);
        assert!(Config::parse(text, operator).unwrap().server.operator_url.is_some());
        let insecure = vars(&[("SATCHEL_SERVER__OPERATOR_URL", "http://wallet-admin.example.org")]);
        assert!(Config::parse(text, insecure).is_err());
    }

    #[test]
    fn handoff_origins_must_be_https_origins() {
        let text = include_str!("../example/config.toml.example");
        let origins = vars(&[(
            "SATCHEL_SERVER__HANDOFF_ORIGINS",
            r#"["https://app.example.org", "https://other.example.net:8443/"]"#,
        )]);
        assert_eq!(Config::parse(text, origins).unwrap().server.handoff_origins.len(), 2);
        for bad in [
            r#"["http://app.example.org"]"#,
            r#"["https://app.example.org/path"]"#,
            r#"["https://user@app.example.org"]"#,
            r#"["app.example.org"]"#,
            r#"["*"]"#,
            r#""https://app.example.org""#,
        ] {
            let origins = vars(&[("SATCHEL_SERVER__HANDOFF_ORIGINS", bad)]);
            assert!(Config::parse(text, origins).is_err(), "{bad} must be refused");
        }
    }
}
