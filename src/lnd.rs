//! LND through its REST proxy: only the calls this wallet needs, behind a
//! trait so the tests can run against an in-memory node.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Certificate, Client, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::config;

const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Networks this wallet agrees to run on. Anything else, mainnet included, stops startup.
pub(crate) const TEST_NETWORKS: [&str; 5] = ["testnet", "testnet4", "signet", "regtest", "simnet"];

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The node operations the wallet uses.
pub(crate) trait Lightning: Send + Sync {
    fn get_info(&self) -> BoxFuture<'_, NodeInfo>;
    fn add_invoice(&self, request: InvoiceRequest) -> BoxFuture<'_, AddedInvoice>;
    /// `None` when LND does not know the invoice.
    fn lookup_invoice(&self, payment_hash: String) -> BoxFuture<'_, Option<LndInvoice>>;
    fn cancel_invoice(&self, payment_hash: String) -> BoxFuture<'_, ()>;
    /// Decoding also checks the invoice belongs to the node's network.
    fn decode_invoice(&self, bolt11: String) -> BoxFuture<'_, DecodedInvoice>;
    /// `Ok` only with a final status or a rejection before sending;
    /// `Err` means the outcome is unknown and must be tracked later.
    fn send_payment(&self, request: SendRequest) -> BoxFuture<'_, PaymentStatus>;
    fn track_payment(&self, payment_hash: String) -> BoxFuture<'_, PaymentStatus>;
    /// Invoice updates, starting after the given settle index.
    fn subscribe_invoices(&self, settle_index: u64) -> BoxFuture<'_, Box<dyn InvoiceStream>>;
    fn balances(&self) -> BoxFuture<'_, NodeBalances>;
}

pub(crate) trait InvoiceStream: Send {
    /// The next update, or `None` when the stream ends.
    fn next(&mut self) -> BoxFuture<'_, Option<LndInvoice>>;
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct NodeInfo {
    #[serde(default)]
    pub(crate) alias: String,
    #[serde(default)]
    pub(crate) identity_pubkey: String,
    #[serde(default)]
    pub(crate) chains: Vec<Chain>,
    #[serde(default)]
    pub(crate) synced_to_chain: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Chain {
    #[serde(default)]
    pub(crate) chain: String,
    #[serde(default)]
    pub(crate) network: String,
}

#[derive(Debug, Clone)]
pub(crate) struct InvoiceRequest {
    pub(crate) amount_msat: u64,
    pub(crate) memo: String,
    /// LNURL invoices commit to the metadata hash instead of a memo.
    pub(crate) description_hash: Option<[u8; 32]>,
    pub(crate) expiry_secs: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct AddedInvoice {
    /// Lowercase hex.
    pub(crate) payment_hash: String,
    pub(crate) bolt11: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct LndInvoice {
    /// Base64, as the REST proxy encodes bytes.
    #[serde(default)]
    pub(crate) r_hash: String,
    #[serde(default)]
    pub(crate) state: String,
    #[serde(default, deserialize_with = "u64_from_string")]
    pub(crate) amt_paid_msat: u64,
    #[serde(default, deserialize_with = "u64_from_string")]
    pub(crate) settle_index: u64,
}

impl LndInvoice {
    pub(crate) fn payment_hash(&self) -> Option<String> {
        decode_bytes(&self.r_hash)
            .filter(|bytes| bytes.len() == 32)
            .map(hex::encode)
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.state == "SETTLED"
    }

    pub(crate) fn is_canceled(&self) -> bool {
        self.state == "CANCELED"
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct DecodedInvoice {
    #[serde(default)]
    pub(crate) destination: String,
    /// Lowercase hex.
    #[serde(default)]
    pub(crate) payment_hash: String,
    #[serde(default, deserialize_with = "u64_from_string")]
    pub(crate) num_msat: u64,
    #[serde(default, deserialize_with = "u64_from_string")]
    pub(crate) timestamp: u64,
    #[serde(default, deserialize_with = "u64_from_string")]
    pub(crate) expiry: u64,
    #[serde(default)]
    pub(crate) description: String,
    /// Lowercase hex, empty when the invoice has a plain description.
    #[serde(default)]
    pub(crate) description_hash: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SendRequest {
    pub(crate) bolt11: String,
    /// Only for invoices without an amount.
    pub(crate) amount_msat: Option<u64>,
    pub(crate) fee_limit_msat: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaymentStatus {
    Succeeded { fee_msat: u64 },
    Failed { reason: String },
    InFlight,
    NotFound,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NodeBalances {
    pub(crate) channel_local_msat: u64,
    pub(crate) onchain_confirmed_sat: u64,
}

/// An error LND returned, with its message (LND messages hold no credentials).
#[derive(Debug)]
pub(crate) struct LndError {
    pub(crate) status: u16,
    pub(crate) message: String,
}

impl fmt::Display for LndError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LND error (HTTP {}): {}", self.status, self.message)
    }
}

impl std::error::Error for LndError {}

/// Refuses every network except the test networks, with no override.
pub(crate) fn check_test_network(info: &NodeInfo, expected: Option<&str>) -> Result<String> {
    let [chain] = info.chains.as_slice() else {
        bail!("refusing to start: LND must report exactly one chain");
    };
    if chain.chain != "bitcoin" {
        bail!("refusing to start: LND reports chain {:?}, not bitcoin", chain.chain);
    }
    let network = chain.network.as_str();
    if !TEST_NETWORKS.contains(&network) {
        bail!(
            "refusing to start: LND reports network {network:?}. This wallet runs only on test networks ({}) and never on mainnet",
            TEST_NETWORKS.join(", ")
        );
    }
    if let Some(expected) = expected
        && expected != network
    {
        bail!("refusing to start: LND reports network {network:?} but lnd.expected_network is {expected:?}");
    }
    Ok(network.to_owned())
}

/// Asks the node for its network at startup and refuses anything but a test network.
pub(crate) async fn require_test_network(lnd: &dyn Lightning, expected: Option<&str>) -> Result<(NodeInfo, String)> {
    let info = lnd.get_info().await.context("cannot reach LND to check its network")?;
    let network = check_test_network(&info, expected)?;
    Ok((info, network))
}

/// `lnbc...` is a mainnet invoice; `lnbcrt...` is regtest.
pub(crate) fn is_mainnet_invoice(bolt11: &str) -> bool {
    let lower = bolt11.to_ascii_lowercase();
    lower.starts_with("lnbc") && !lower.starts_with("lnbcrt")
}

/// BOLT11 currency prefixes (after `ln`) and the networks they belong to.
/// testnet4 invoices use testnet's prefix.
const INVOICE_PREFIXES: [(&str, &str); 5] = [
    ("bc", "mainnet"),
    ("tb", "testnet"),
    ("tbs", "signet"),
    ("bcrt", "regtest"),
    ("sb", "simnet"),
];

/// The network an invoice's prefix names (`lntbs...` is signet), when it is a known one.
pub(crate) fn invoice_network(bolt11: &str) -> Option<&'static str> {
    let lower = bolt11.trim().to_ascii_lowercase();
    let currency: String = lower
        .strip_prefix("ln")?
        .chars()
        .take_while(char::is_ascii_lowercase)
        .collect();
    INVOICE_PREFIXES
        .iter()
        .find(|(prefix, _)| *prefix == currency)
        .map(|(_, network)| *network)
}

/// The other network an invoice belongs to, when its prefix is not the node's.
pub(crate) fn foreign_invoice_network(bolt11: &str, node_network: &str) -> Option<&'static str> {
    let node_network = if node_network == "testnet4" {
        "testnet"
    } else {
        node_network
    };
    invoice_network(bolt11).filter(|network| *network != node_network)
}

fn u64_from_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Text(String),
        Value(u64),
    }
    match Number::deserialize(deserializer)? {
        Number::Value(value) => Ok(value),
        Number::Text(text) if text.is_empty() => Ok(0),
        Number::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

fn decode_bytes(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).or_else(|_| URL_SAFE.decode(text)).ok()
}

fn hash_bytes(payment_hash: &str) -> Result<Vec<u8>> {
    let bytes = hex::decode(payment_hash).context("payment hash is not hex")?;
    if bytes.len() != 32 {
        bail!("payment hash must be 32 bytes");
    }
    Ok(bytes)
}

fn rpc_error(status: StatusCode, body: &[u8]) -> LndError {
    let value: Value = serde_json::from_slice(body).unwrap_or_default();
    let message = value
        .get("message")
        .or_else(|| value.get("error").and_then(|error| error.get("message")))
        .and_then(Value::as_str)
        .unwrap_or("unexpected LND response")
        .to_owned();
    LndError {
        status: status.as_u16(),
        message,
    }
}

async fn read_body(mut response: Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        bail!("LND response exceeds limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            bail!("LND response exceeds limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// One message from a streaming REST call.
#[derive(Debug)]
pub(crate) enum StreamItem<T> {
    Result(T),
    Error(String),
}

#[derive(Deserialize)]
struct RawItem<T> {
    result: Option<T>,
    error: Option<RawError>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    message: String,
}

/// Parses one line of a REST stream: `{"result": ...}` or `{"error": ...}`.
pub(crate) fn parse_stream_line<T: DeserializeOwned>(line: &[u8]) -> Result<Option<StreamItem<T>>> {
    let raw: RawItem<T> = serde_json::from_slice(line).context("malformed LND stream message")?;
    Ok(match (raw.result, raw.error) {
        (_, Some(error)) => Some(StreamItem::Error(error.message)),
        (Some(result), None) => Some(StreamItem::Result(result)),
        (None, None) => None,
    })
}

/// Newline-delimited JSON messages from a streaming response.
struct JsonLines {
    response: Response,
    buffer: Vec<u8>,
    done: bool,
}

impl JsonLines {
    fn new(response: Response) -> Self {
        Self {
            response,
            buffer: Vec::new(),
            done: false,
        }
    }

    async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<StreamItem<T>>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                if let Some(item) = parse_stream_line(line.trim_ascii())? {
                    return Ok(Some(item));
                }
                continue;
            }
            if self.done {
                let rest = std::mem::take(&mut self.buffer);
                let rest = rest.trim_ascii();
                return if rest.is_empty() {
                    Ok(None)
                } else {
                    parse_stream_line(rest)
                };
            }
            match self.response.chunk().await? {
                Some(chunk) => {
                    if self.buffer.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                        bail!("LND stream message exceeds limit");
                    }
                    self.buffer.extend_from_slice(&chunk);
                }
                None => self.done = true,
            }
        }
    }
}

#[derive(Deserialize)]
struct LndPayment {
    #[serde(default)]
    status: String,
    #[serde(default, deserialize_with = "u64_from_string")]
    fee_msat: u64,
    #[serde(default)]
    failure_reason: String,
}

impl LndPayment {
    fn status(&self) -> PaymentStatus {
        match self.status.as_str() {
            "SUCCEEDED" => PaymentStatus::Succeeded {
                fee_msat: self.fee_msat,
            },
            "FAILED" => PaymentStatus::Failed {
                reason: failure_text(&self.failure_reason).to_owned(),
            },
            _ => PaymentStatus::InFlight,
        }
    }
}

fn failure_text(reason: &str) -> &'static str {
    match reason {
        "FAILURE_REASON_TIMEOUT" => "Payment timed out.",
        "FAILURE_REASON_NO_ROUTE" => "No route to the recipient was found.",
        "FAILURE_REASON_INCORRECT_PAYMENT_DETAILS" => "The recipient rejected the payment details.",
        "FAILURE_REASON_INSUFFICIENT_BALANCE" => "The wallet's node lacks outbound liquidity for this payment.",
        "FAILURE_REASON_CANCELED" => "Payment was canceled.",
        _ => "Payment failed.",
    }
}

/// LND's REST proxy, authenticated with one macaroon and pinned to its TLS certificate.
#[derive(Debug)]
pub(crate) struct LndRest {
    client: Client,
    streams: Client,
    base: String,
    request_timeout: Duration,
    payment_timeout_secs: u32,
}

impl LndRest {
    pub(crate) fn new(config: &config::Lnd, credentials_dir: &Path) -> Result<Self> {
        let certificate =
            std::fs::read(credentials_dir.join(&config.tls_cert_path)).context("cannot read LND TLS certificate")?;
        let certificate = Certificate::from_pem(&certificate).context("invalid LND TLS certificate")?;
        let macaroon =
            std::fs::read(credentials_dir.join(&config.macaroon_path)).context("cannot read LND macaroon")?;
        if macaroon.is_empty() {
            bail!("LND macaroon must not be empty");
        }
        let mut header = HeaderValue::from_str(&hex::encode(macaroon))?;
        header.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("Grpc-Metadata-macaroon", header);
        let request_timeout = Duration::from_secs(config.request_timeout_secs);
        // LND serves a self-signed CA:true certificate. Native TLS accepts it as the
        // only trust anchor while still checking validity and the requested address.
        let builder = || {
            Client::builder()
                .tls_backend_native()
                .tls_certs_only([certificate.clone()])
                .default_headers(headers.clone())
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(request_timeout)
        };
        let client = builder()
            .timeout(request_timeout)
            .build()
            .context("cannot build LND HTTPS client")?;
        // Streams stay open for minutes or days: no overall deadline, but TCP keepalives.
        let streams = builder()
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .context("cannot build LND streaming client")?;
        Ok(Self {
            client,
            streams,
            base: format!("https://{}", config.rest_host),
            request_timeout,
            payment_timeout_secs: config.payment_timeout_secs,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn unary<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        let response = request.send().await?;
        let status = response.status();
        let body = read_body(response).await?;
        if !status.is_success() {
            return Err(rpc_error(status, &body).into());
        }
        serde_json::from_slice(&body).context("malformed LND response")
    }
}

/// Reads a payment stream until a final status. A rejection before any
/// update means LND never started the payment.
async fn follow_payment(response: Response, before_update: fn(String) -> PaymentStatus) -> Result<PaymentStatus> {
    let status = response.status();
    if !status.is_success() {
        let body = read_body(response).await?;
        return Ok(before_update(rpc_error(status, &body).message));
    }
    let mut lines = JsonLines::new(response);
    let mut updates = 0_u32;
    loop {
        match lines.next::<LndPayment>().await? {
            Some(StreamItem::Result(payment)) => match payment.status() {
                PaymentStatus::InFlight => updates += 1,
                done => return Ok(done),
            },
            Some(StreamItem::Error(message)) if updates == 0 => return Ok(before_update(message)),
            Some(StreamItem::Error(message)) => bail!("payment stream error after it started: {message}"),
            None => bail!("payment stream ended without a final status"),
        }
    }
}

/// The first status a track stream reports.
async fn first_payment_status(request: RequestBuilder) -> Result<PaymentStatus> {
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let body = read_body(response).await?;
        return Ok(not_initiated(rpc_error(status, &body).message));
    }
    let mut lines = JsonLines::new(response);
    match lines.next::<LndPayment>().await? {
        Some(StreamItem::Result(payment)) => Ok(payment.status()),
        Some(StreamItem::Error(message)) => Ok(not_initiated(message)),
        None => bail!("track stream ended without a status"),
    }
}

fn rejected(message: String) -> PaymentStatus {
    PaymentStatus::Failed {
        reason: format!("The node refused the payment: {message}"),
    }
}

fn not_initiated(message: String) -> PaymentStatus {
    if message.contains("isn't initiated") || message.contains("not found") {
        PaymentStatus::NotFound
    } else {
        // Unknown problem: report it as still in flight so nothing is refunded.
        tracing::warn!(%message, "cannot track payment");
        PaymentStatus::InFlight
    }
}

impl Lightning for LndRest {
    fn get_info(&self) -> BoxFuture<'_, NodeInfo> {
        Box::pin(async move { self.unary(self.client.get(self.url("/v1/getinfo"))).await })
    }

    fn add_invoice(&self, request: InvoiceRequest) -> BoxFuture<'_, AddedInvoice> {
        Box::pin(async move {
            let mut body = json!({
                "value_msat": request.amount_msat.to_string(),
                "expiry": request.expiry_secs.to_string(),
                "private": true,
            });
            match request.description_hash {
                Some(hash) => body["description_hash"] = json!(STANDARD.encode(hash)),
                None => body["memo"] = json!(request.memo),
            }
            #[derive(Deserialize)]
            struct Added {
                r_hash: String,
                payment_request: String,
            }
            let added: Added = self
                .unary(self.client.post(self.url("/v1/invoices")).json(&body))
                .await?;
            let hash = decode_bytes(&added.r_hash)
                .filter(|bytes| bytes.len() == 32)
                .context("LND returned an invalid payment hash")?;
            if added.payment_request.trim().is_empty() {
                bail!("LND returned an empty payment request");
            }
            Ok(AddedInvoice {
                payment_hash: hex::encode(hash),
                bolt11: added.payment_request,
            })
        })
    }

    fn lookup_invoice(&self, payment_hash: String) -> BoxFuture<'_, Option<LndInvoice>> {
        Box::pin(async move {
            hash_bytes(&payment_hash)?;
            let request = self.client.get(self.url(&format!("/v1/invoice/{payment_hash}")));
            match self.unary::<LndInvoice>(request).await {
                Ok(invoice) => Ok(Some(invoice)),
                Err(error) => {
                    let missing = error
                        .downcast_ref::<LndError>()
                        .is_some_and(|lnd| lnd.status == 404 || lnd.message.contains("unable to locate"));
                    if missing { Ok(None) } else { Err(error) }
                }
            }
        })
    }

    fn cancel_invoice(&self, payment_hash: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let body = json!({ "payment_hash": STANDARD.encode(hash_bytes(&payment_hash)?) });
            let _: Value = self
                .unary(self.client.post(self.url("/v2/invoices/cancel")).json(&body))
                .await?;
            Ok(())
        })
    }

    fn decode_invoice(&self, bolt11: String) -> BoxFuture<'_, DecodedInvoice> {
        Box::pin(async move {
            if bolt11.is_empty() || bolt11.len() > 4096 || !bolt11.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
                bail!("not a BOLT11 invoice");
            }
            self.unary(self.client.get(self.url(&format!("/v1/payreq/{bolt11}"))))
                .await
        })
    }

    fn send_payment(&self, request: SendRequest) -> BoxFuture<'_, PaymentStatus> {
        Box::pin(async move {
            let mut body = json!({
                "payment_request": request.bolt11,
                "fee_limit_msat": request.fee_limit_msat.to_string(),
                "timeout_seconds": self.payment_timeout_secs,
                // In-flight updates show that LND started the payment, which
                // separates a rejection (safe to refund) from a broken stream.
                "no_inflight_updates": false,
            });
            if let Some(amount) = request.amount_msat {
                body["amt_msat"] = json!(amount.to_string());
            }
            let response = self
                .streams
                .post(self.url("/v2/router/send"))
                .json(&body)
                .send()
                .await?;
            let deadline = Duration::from_secs(u64::from(self.payment_timeout_secs) + 30);
            tokio::time::timeout(deadline, follow_payment(response, rejected))
                .await
                .context("payment stream timed out")?
        })
    }

    fn track_payment(&self, payment_hash: String) -> BoxFuture<'_, PaymentStatus> {
        Box::pin(async move {
            let hash = URL_SAFE.encode(hash_bytes(&payment_hash)?);
            let request = self
                .streams
                .get(self.url(&format!("/v2/router/track/{hash}?no_inflight_updates=false")));
            match tokio::time::timeout(self.request_timeout, first_payment_status(request)).await {
                Ok(status) => status,
                Err(_) => Ok(PaymentStatus::InFlight),
            }
        })
    }

    fn subscribe_invoices(&self, settle_index: u64) -> BoxFuture<'_, Box<dyn InvoiceStream>> {
        Box::pin(async move {
            let response = self
                .streams
                .get(self.url(&format!("/v1/invoices/subscribe?settle_index={settle_index}")))
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                let body = read_body(response).await?;
                return Err(rpc_error(status, &body).into());
            }
            Ok(Box::new(RestInvoices(JsonLines::new(response))) as Box<dyn InvoiceStream>)
        })
    }

    fn balances(&self) -> BoxFuture<'_, NodeBalances> {
        Box::pin(async move {
            #[derive(Default, Deserialize)]
            struct Amount {
                #[serde(default, deserialize_with = "u64_from_string")]
                msat: u64,
            }
            #[derive(Deserialize)]
            struct Channels {
                #[serde(default)]
                local_balance: Option<Amount>,
            }
            #[derive(Deserialize)]
            struct Onchain {
                #[serde(default, deserialize_with = "u64_from_string")]
                confirmed_balance: u64,
            }
            let channels: Channels = self.unary(self.client.get(self.url("/v1/balance/channels"))).await?;
            let onchain: Onchain = self.unary(self.client.get(self.url("/v1/balance/blockchain"))).await?;
            Ok(NodeBalances {
                channel_local_msat: channels.local_balance.unwrap_or_default().msat,
                onchain_confirmed_sat: onchain.confirmed_balance,
            })
        })
    }
}

struct RestInvoices(JsonLines);

impl InvoiceStream for RestInvoices {
    fn next(&mut self) -> BoxFuture<'_, Option<LndInvoice>> {
        Box::pin(async move {
            match self.0.next::<LndInvoice>().await? {
                Some(StreamItem::Result(invoice)) => Ok(Some(invoice)),
                Some(StreamItem::Error(message)) => bail!("invoice stream error: {message}"),
                None => Ok(None),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(chain: &str, network: &str) -> NodeInfo {
        NodeInfo {
            chains: vec![Chain {
                chain: chain.to_owned(),
                network: network.to_owned(),
            }],
            ..NodeInfo::default()
        }
    }

    #[test]
    fn refuses_mainnet_and_unknown_networks() {
        let mainnet = check_test_network(&info("bitcoin", "mainnet"), None).unwrap_err();
        assert!(mainnet.to_string().contains("never on mainnet"));
        assert!(check_test_network(&info("bitcoin", "bitcoin"), None).is_err());
        assert!(check_test_network(&info("bitcoin", ""), None).is_err());
        assert!(check_test_network(&info("litecoin", "testnet"), None).is_err());
        assert!(check_test_network(&NodeInfo::default(), None).is_err());
        let two = NodeInfo {
            chains: vec![
                info("bitcoin", "signet").chains[0].clone(),
                info("bitcoin", "mainnet").chains[0].clone(),
            ],
            ..NodeInfo::default()
        };
        assert!(check_test_network(&two, None).is_err());
    }

    #[test]
    fn accepts_test_networks_and_checks_the_expected_one() {
        for network in TEST_NETWORKS {
            assert_eq!(check_test_network(&info("bitcoin", network), None).unwrap(), network);
        }
        assert!(check_test_network(&info("bitcoin", "signet"), Some("signet")).is_ok());
        assert!(check_test_network(&info("bitcoin", "regtest"), Some("signet")).is_err());
    }

    #[test]
    fn recognises_mainnet_invoices() {
        assert!(is_mainnet_invoice("lnbc10u1p..."));
        assert!(is_mainnet_invoice("LNBC1..."));
        assert!(!is_mainnet_invoice("lnbcrt10u1p..."));
        assert!(!is_mainnet_invoice("lntbs10u1p..."));
        assert!(!is_mainnet_invoice("lntb10u1p..."));
    }

    #[test]
    fn reads_the_network_from_invoice_prefixes() {
        assert_eq!(invoice_network("lntbs10u1p..."), Some("signet"));
        assert_eq!(invoice_network(" LNTBS1P... "), Some("signet"));
        assert_eq!(invoice_network("lntb10u1p..."), Some("testnet"));
        assert_eq!(invoice_network("lntb1p..."), Some("testnet"));
        assert_eq!(invoice_network("lnbcrt500n1p..."), Some("regtest"));
        assert_eq!(invoice_network("lnsb1p..."), Some("simnet"));
        assert_eq!(invoice_network("lnbc1p..."), Some("mainnet"));
        assert_eq!(invoice_network("lnxyz1p..."), None);
        assert_eq!(invoice_network("alice@example.org"), None);
        assert_eq!(foreign_invoice_network("lntbs10u1p...", "signet"), None);
        assert_eq!(foreign_invoice_network("lntb10u1p...", "testnet4"), None);
        assert_eq!(foreign_invoice_network("lntb10u1p...", "signet"), Some("testnet"));
        assert_eq!(foreign_invoice_network("lnbcrt10u1p...", "signet"), Some("regtest"));
        assert_eq!(foreign_invoice_network("lnxyz1p...", "signet"), None);
    }

    #[test]
    fn parses_rest_json_shapes() {
        let invoice: LndInvoice = serde_json::from_str(
            r#"{"r_hash":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=","state":"SETTLED","amt_paid_msat":"21000","settle_index":"7","memo":"x"}"#,
        )
        .unwrap();
        assert!(invoice.is_settled());
        assert_eq!(invoice.amt_paid_msat, 21_000);
        assert_eq!(invoice.settle_index, 7);
        assert_eq!(
            invoice.payment_hash().unwrap(),
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        );
        let item = parse_stream_line::<LndPayment>(br#"{"result":{"status":"SUCCEEDED","fee_msat":"1500"}}"#)
            .unwrap()
            .unwrap();
        let StreamItem::Result(payment) = item else {
            panic!("expected a result")
        };
        assert_eq!(payment.status(), PaymentStatus::Succeeded { fee_msat: 1500 });
        let item = parse_stream_line::<LndPayment>(br#"{"error":{"code":2,"message":"invoice is already paid"}}"#)
            .unwrap()
            .unwrap();
        assert!(matches!(item, StreamItem::Error(message) if message == "invoice is already paid"));
        let failed: LndPayment =
            serde_json::from_str(r#"{"status":"FAILED","failure_reason":"FAILURE_REASON_NO_ROUTE"}"#).unwrap();
        assert!(matches!(failed.status(), PaymentStatus::Failed { reason } if reason.contains("No route")));
        let error = rpc_error(
            StatusCode::NOT_FOUND,
            br#"{"code":5,"message":"unable to locate invoice"}"#,
        );
        assert_eq!(error.message, "unable to locate invoice");
        assert!(matches!(
            not_initiated("payment isn't initiated".into()),
            PaymentStatus::NotFound
        ));
        assert!(matches!(
            not_initiated("connection reset".into()),
            PaymentStatus::InFlight
        ));
    }
}
