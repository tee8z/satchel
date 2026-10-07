//! LNURL-pay (LUD-06) and Lightning Addresses (LUD-16): metadata for our own
//! accounts, parsing what people paste, and a guarded client for paying others.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bech32::{Bech32, Hrp};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::error::WalletError;

/// Longest payer comment accepted (LUD-12).
pub(crate) const COMMENT_ALLOWED: usize = 140;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// The metadata string our LNURL-pay endpoints serve; invoices commit to its SHA-256.
pub(crate) fn metadata(username: &str, domain: &str) -> String {
    let address = format!("{username}@{domain}");
    let text = format!("Test-network payment to {address}");
    serde_json::to_string(&[["text/plain", text.as_str()], ["text/identifier", address.as_str()]])
        .expect("string arrays serialize")
}

/// `LNURL1...` (uppercase, for compact QR codes) for a URL.
pub(crate) fn encode_lnurl(url: &str) -> String {
    bech32::encode::<Bech32>(Hrp::parse_unchecked("lnurl"), url.as_bytes())
        .map(|encoded| encoded.to_ascii_uppercase())
        .unwrap_or_default()
}

/// Something a person pasted into the send form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Destination {
    Invoice(String),
    Address { user: String, domain: String },
    Lnurl(Url),
}

impl Destination {
    /// The LNURL-pay URL for addresses and LNURLs.
    pub(crate) fn pay_url(&self) -> Option<Url> {
        match self {
            Self::Invoice(_) => None,
            Self::Address { user, domain } => Url::parse(&format!("https://{domain}/.well-known/lnurlp/{user}")).ok(),
            Self::Lnurl(url) => Some(url.clone()),
        }
    }
}

fn valid_domain(domain: &str) -> bool {
    let (host, port) = domain.split_once(':').unwrap_or((domain, ""));
    !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && host
            .split('.')
            .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'))
        && host
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-')
        && (port.is_empty() || port.parse::<u16>().is_ok())
}

pub(crate) fn parse_destination(input: &str) -> Result<Destination, WalletError> {
    let lower = input.trim().to_ascii_lowercase();
    let value = lower.strip_prefix("lightning:").unwrap_or(&lower).trim();
    if value.is_empty() {
        return Err(WalletError::invalid("Paste an invoice or a Lightning Address."));
    }
    if let Some((user, domain)) = value.split_once('@') {
        let user_ok = (1..=64).contains(&user.len())
            && user
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.+".contains(&byte));
        if !user_ok || !valid_domain(domain) {
            return Err(WalletError::invalid("That Lightning Address is not valid."));
        }
        return Ok(Destination::Address {
            user: user.to_owned(),
            domain: domain.to_owned(),
        });
    }
    if value.starts_with("lnurl1") {
        let (hrp, data) = bech32::decode(value).map_err(|_| WalletError::invalid("That LNURL is not valid."))?;
        let url = String::from_utf8(data)
            .ok()
            .filter(|_| hrp.to_lowercase() == "lnurl")
            .and_then(|text| Url::parse(&text).ok())
            .ok_or_else(|| WalletError::invalid("That LNURL is not valid."))?;
        return Ok(Destination::Lnurl(url));
    }
    if let Some(rest) = value.strip_prefix("lnurlp://") {
        let url =
            Url::parse(&format!("https://{rest}")).map_err(|_| WalletError::invalid("That LNURL is not valid."))?;
        return Ok(Destination::Lnurl(url));
    }
    if value.starts_with("ln") && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Ok(Destination::Invoice(value.to_owned()));
    }
    Err(WalletError::invalid(
        "That does not look like a Lightning invoice or address.",
    ))
}

/// Whether an address is reachable on the public internet (not loopback,
/// private, link-local, carrier-grade NAT, documentation, or reserved space).
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 192 && b == 0 && c == 0))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let [first, second, ..] = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || (first == 0x2001 && second == 0x0db8)
                || (first == 0x0064 && second == 0xff9b))
        }
    }
}

/// The LNURL-pay parameters another server returned.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PayParams {
    pub(crate) callback: String,
    pub(crate) min_sendable: u64,
    pub(crate) max_sendable: u64,
    pub(crate) metadata: String,
    pub(crate) tag: String,
    #[serde(default)]
    pub(crate) comment_allowed: usize,
}

/// Fetches LNURL documents from other servers without letting them reach
/// private networks: names resolve once, every address must be public, and
/// the connection is pinned to the checked address. No redirects or proxies.
#[derive(Debug, Clone)]
pub(crate) struct LnurlClient {
    pub(crate) timeout: Duration,
    pub(crate) allow_private: bool,
}

fn remote_error(value: &Value) -> Option<WalletError> {
    if value.get("status").and_then(Value::as_str) != Some("ERROR") {
        return None;
    }
    let reason: String = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("no reason given")
        .chars()
        .take(200)
        .collect();
    Some(WalletError::invalid(format!(
        "The recipient's server refused: {reason}"
    )))
}

impl LnurlClient {
    pub(crate) async fn pay_params(&self, url: &Url) -> Result<PayParams, WalletError> {
        let value = self.get_json(url).await?;
        if let Some(error) = remote_error(&value) {
            return Err(error);
        }
        let params: PayParams = serde_json::from_value(value)
            .map_err(|_| WalletError::invalid("The recipient's server sent an unexpected response."))?;
        if params.tag != "payRequest" {
            return Err(WalletError::invalid("That LNURL is not a payment request."));
        }
        if params.min_sendable > params.max_sendable {
            return Err(WalletError::invalid(
                "The recipient's server sent impossible amount limits.",
            ));
        }
        Ok(params)
    }

    pub(crate) async fn invoice(
        &self,
        params: &PayParams,
        amount_msat: u64,
        comment: Option<&str>,
    ) -> Result<String, WalletError> {
        let mut callback = Url::parse(&params.callback)
            .map_err(|_| WalletError::invalid("The recipient's server sent a bad callback URL."))?;
        callback
            .query_pairs_mut()
            .append_pair("amount", &amount_msat.to_string());
        if let Some(comment) = comment.filter(|comment| !comment.is_empty() && params.comment_allowed > 0) {
            let comment: String = comment.chars().take(params.comment_allowed).collect();
            callback.query_pairs_mut().append_pair("comment", &comment);
        }
        let value = self.get_json(&callback).await?;
        if let Some(error) = remote_error(&value) {
            return Err(error);
        }
        value
            .get("pr")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| WalletError::invalid("The recipient's server did not return an invoice."))
    }

    async fn get_json(&self, url: &Url) -> Result<Value, WalletError> {
        let unreachable = || WalletError::invalid("Could not reach the recipient's server.");
        if !(url.scheme() == "https" || (self.allow_private && url.scheme() == "http")) {
            return Err(WalletError::invalid("Lightning Address servers must use HTTPS."));
        }
        let host = url.host_str().ok_or_else(unreachable)?.to_owned();
        let port = url.port_or_known_default().unwrap_or(443);
        let lookup = host.trim_start_matches('[').trim_end_matches(']').to_owned();
        let addresses: Vec<SocketAddr> = tokio::time::timeout(self.timeout, tokio::net::lookup_host((lookup, port)))
            .await
            .map_err(|_| unreachable())?
            .map_err(|_| unreachable())?
            .collect();
        let Some(first) = addresses.first().copied() else {
            return Err(unreachable());
        };
        if !self.allow_private && addresses.iter().any(|address| !is_public(address.ip())) {
            return Err(WalletError::invalid(
                "That address points into a private network; this wallet will not contact it.",
            ));
        }
        let client = Client::builder()
            .tls_backend_native()
            .resolve(&host, first)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(self.timeout)
            .connect_timeout(self.timeout)
            .user_agent(concat!("satchel/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| WalletError::Internal(error.into()))?;
        let mut response = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| unreachable())?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unreachable())? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(WalletError::invalid("The recipient's server sent too much data."));
            }
            body.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&body).map_err(|_| {
            WalletError::invalid(format!(
                "The recipient's server sent an unexpected response (HTTP {}).",
                response.status().as_u16()
            ))
        })?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_names_the_address() {
        let metadata = metadata("alice", "wallet.example.org");
        let parsed: Vec<[String; 2]> = serde_json::from_str(&metadata).unwrap();
        assert_eq!(
            parsed[1],
            ["text/identifier".to_owned(), "alice@wallet.example.org".to_owned()]
        );
    }

    #[test]
    fn parses_what_people_paste() {
        assert_eq!(
            parse_destination(" lightning:LNTBS10U1PTEST ").unwrap(),
            Destination::Invoice("lntbs10u1ptest".into())
        );
        assert_eq!(
            parse_destination("Alice@Wallet.Example.org").unwrap(),
            Destination::Address {
                user: "alice".into(),
                domain: "wallet.example.org".into()
            }
        );
        let lnurl = encode_lnurl("https://wallet.example.org/.well-known/lnurlp/alice");
        assert!(lnurl.starts_with("LNURL1"));
        let parsed = parse_destination(&lnurl).unwrap();
        assert_eq!(
            parsed.pay_url().unwrap().as_str(),
            "https://wallet.example.org/.well-known/lnurlp/alice"
        );
        assert!(matches!(
            parse_destination("lnurlp://wallet.example.org/.well-known/lnurlp/bob").unwrap(),
            Destination::Lnurl(_)
        ));
        for bad in [
            "",
            "alice@",
            "@example.org",
            "alice@localhost",
            "alice@exa mple.org",
            "alice@x.org/path",
            "hello",
        ] {
            assert!(parse_destination(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn only_public_addresses_are_contacted() {
        for private in [
            "127.0.0.1",
            "10.0.0.10",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private} must be refused");
        }
        for public in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public} is public");
        }
    }

    #[tokio::test]
    async fn refuses_private_hosts_and_plain_http() {
        let client = LnurlClient {
            timeout: Duration::from_secs(2),
            allow_private: false,
        };
        let local = Url::parse("https://127.0.0.1/.well-known/lnurlp/alice").unwrap();
        assert!(
            client
                .pay_params(&local)
                .await
                .unwrap_err()
                .to_string()
                .contains("private network")
        );
        let http = Url::parse("http://example.org/.well-known/lnurlp/alice").unwrap();
        assert!(
            client
                .pay_params(&http)
                .await
                .unwrap_err()
                .to_string()
                .contains("HTTPS")
        );
    }
}
