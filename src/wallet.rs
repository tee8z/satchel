//! Wallet operations on top of the ledger and the LND node: receiving,
//! sending, internal transfers, the faucet, and keeping both sides in step.

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};
use url::Url;

use crate::auth::valid_request_key;
use crate::config::Config;
use crate::db::{Account, Db};
use crate::error::WalletError;
use crate::ledger::{DailyLimits, Grant, Invoice, NewInvoice, NewSend, Payment, SendOutcome, Started, Transfer};
use crate::lnd::{
    self, DecodedInvoice, InvoiceRequest, Lightning, LndInvoice, NodeBalances, PaymentStatus, SendRequest,
};
use crate::lnurl::{self, COMMENT_ALLOWED, Destination, LnurlClient};
use crate::metrics::{Metrics, inc};
use crate::util::{format_msat, now, sha256};

const SETTLE_CURSOR: &str = "lnd_invoice_settle_index";
/// Payments younger than this are left to the task that started them.
const RECONCILE_GRACE_SECS: i64 = 120;
const RECONCILE_EVERY: Duration = Duration::from_secs(60);

/// Limits in msat.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) max_balance_msat: u64,
    pub(crate) max_payment_msat: u64,
    pub(crate) min_receive_msat: u64,
    pub(crate) max_receive_msat: u64,
    pub(crate) invoice_expiry_secs: u32,
    pub(crate) fee_limit_ppm: u64,
    pub(crate) min_fee_limit_msat: u64,
    pub(crate) max_open_invoices: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Faucet {
    pub(crate) enabled: bool,
    pub(crate) amount_msat: u64,
    pub(crate) per_account_daily_msat: u64,
    pub(crate) per_address_daily_msat: u64,
    pub(crate) global_daily_msat: u64,
}

/// What the send form asks for.
#[derive(Debug, Clone)]
pub(crate) struct PayRequest {
    pub(crate) request_key: String,
    pub(crate) destination: String,
    pub(crate) amount_msat: Option<u64>,
    pub(crate) comment: String,
}

/// A read-only check before the user authorizes a payment. Sending checks again.
pub(crate) struct PaymentPreview {
    pub(crate) recipient: String,
    pub(crate) amount_msat: u64,
    pub(crate) fee_limit_msat: u64,
    pub(crate) description: String,
}

/// An outgoing Lightning payment after decoding.
struct Outgoing {
    bolt11: String,
    decoded: DecodedInvoice,
    amount_msat: u64,
    /// Set only for invoices without an amount.
    amount_override: Option<u64>,
    counterparty: String,
    memo: String,
}

pub(crate) struct Wallet {
    pub(crate) db: Db,
    pub(crate) lnd: Arc<dyn Lightning>,
    pub(crate) lnurl: LnurlClient,
    pub(crate) limits: Limits,
    pub(crate) faucet: Faucet,
    pub(crate) origin: Url,
    /// Lightning Address domain: the public origin's host (and port, if any).
    pub(crate) domain: String,
    pub(crate) network: String,
    pub(crate) network_name: String,
    pub(crate) recovery_url: Option<String>,
    pub(crate) metrics: Metrics,
    /// How long a request waits for a payment's result before showing it as pending.
    pub(crate) send_wait: Duration,
    /// Payments a live task is still driving; reconciliation leaves them alone.
    in_flight: Mutex<HashSet<i64>>,
    /// The node's balances and when the reconciler last read them, so pages never wait on LND.
    node_balances: Mutex<Option<(NodeBalances, i64)>>,
}

fn truncate(text: &str, max: usize) -> String {
    text.trim().chars().take(max).collect()
}

fn to_i64(msat: u64) -> i64 {
    i64::try_from(msat).unwrap_or(i64::MAX)
}

impl Wallet {
    pub(crate) fn new(db: Db, lnd: Arc<dyn Lightning>, network: String, config: &Config, origin: Url) -> Self {
        let limits = &config.limits;
        let host = origin.host_str().unwrap_or_default();
        let domain = match origin.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        Self {
            db,
            lnd,
            lnurl: LnurlClient {
                timeout: Duration::from_secs(config.lnd.request_timeout_secs),
                allow_private: config.server.allow_private_lnurl_hosts,
            },
            limits: Limits {
                max_balance_msat: limits.max_balance_sat * 1000,
                max_payment_msat: limits.max_payment_sat * 1000,
                min_receive_msat: limits.min_receive_sat * 1000,
                max_receive_msat: limits.max_receive_sat * 1000,
                invoice_expiry_secs: limits.invoice_expiry_secs,
                fee_limit_ppm: limits.fee_limit_ppm,
                min_fee_limit_msat: limits.min_fee_limit_sat * 1000,
                max_open_invoices: limits.max_open_invoices,
            },
            faucet: Faucet {
                enabled: config.faucet.enabled,
                amount_msat: config.faucet.amount_sat * 1000,
                per_account_daily_msat: config.faucet.per_account_daily_sat * 1000,
                per_address_daily_msat: config.faucet.per_address_daily_sat * 1000,
                global_daily_msat: config.faucet.global_daily_sat * 1000,
            },
            domain,
            origin,
            network_name: config
                .server
                .network_name
                .as_deref()
                .unwrap_or(&network)
                .trim()
                .to_owned(),
            recovery_url: config.server.recovery_url.clone(),
            network,
            metrics: Metrics::default(),
            send_wait: Duration::from_secs(20),
            in_flight: Mutex::default(),
            node_balances: Mutex::default(),
        }
    }

    /// The node's balances as of the last reconciliation, with the time read.
    pub(crate) fn cached_balances(&self) -> Option<(NodeBalances, i64)> {
        *self.node_balances.lock().expect("balance lock is not poisoned")
    }

    pub(crate) fn address(&self, username: &str) -> String {
        format!("{username}@{}", self.domain)
    }

    pub(crate) fn lnurlp_url(&self, username: &str) -> String {
        format!("{}.well-known/lnurlp/{username}", self.origin)
    }

    pub(crate) fn lnurl_callback(&self, username: &str) -> String {
        format!("{}lnurlp/{username}/callback", self.origin)
    }

    pub(crate) fn fee_limit_msat(&self, amount_msat: u64) -> u64 {
        let proportional = u128::from(amount_msat) * u128::from(self.limits.fee_limit_ppm) / 1_000_000;
        u64::try_from(proportional)
            .unwrap_or(u64::MAX)
            .max(self.limits.min_fee_limit_msat)
    }

    /// The msat range this account can receive now, or `None` if it cannot receive.
    pub(crate) async fn receivable(&self, account: &Account) -> Result<Option<(u64, u64)>, WalletError> {
        if account.frozen {
            return Ok(None);
        }
        let balance = u64::try_from(self.db.balance(account.id).await?).unwrap_or(0);
        let room = self.limits.max_balance_msat.saturating_sub(balance);
        let max = room.min(self.limits.max_receive_msat);
        Ok((max >= self.limits.min_receive_msat).then_some((self.limits.min_receive_msat, max)))
    }

    /// Creates an LND invoice owned by the account. LNURL invoices commit to the
    /// address metadata (description hash); the memo then holds the payer's comment.
    pub(crate) async fn create_invoice(
        &self,
        account: &Account,
        amount_msat: u64,
        memo: &str,
        for_lnurl: bool,
    ) -> Result<Invoice, WalletError> {
        if account.frozen {
            return Err(WalletError::Frozen);
        }
        let source = if for_lnurl { "lnurl" } else { "wallet" };
        if self.db.open_invoice_count(account.id, source).await? >= i64::from(self.limits.max_open_invoices) {
            return Err(WalletError::TooManyInvoices);
        }
        let Some((min, max)) = self.receivable(account).await? else {
            return Err(WalletError::limit("This wallet cannot receive more right now."));
        };
        if !(min..=max).contains(&amount_msat) {
            return Err(WalletError::limit(format!(
                "You can receive between {} and {} sats right now.",
                format_msat(to_i64(min)),
                format_msat(to_i64(max))
            )));
        }
        let memo = truncate(memo, COMMENT_ALLOWED);
        let request = InvoiceRequest {
            amount_msat,
            memo: if for_lnurl { String::new() } else { memo.clone() },
            description_hash: for_lnurl.then(|| sha256(lnurl::metadata(&account.username, &self.domain).as_bytes())),
            expiry_secs: self.limits.invoice_expiry_secs,
        };
        let added = self.lnd.add_invoice(request).await.map_err(|error| {
            warn!(%error, "LND did not create an invoice");
            WalletError::Unavailable
        })?;
        let expires_at = now() + i64::from(self.limits.invoice_expiry_secs);
        let row = NewInvoice {
            payment_hash: &added.payment_hash,
            account_id: account.id,
            bolt11: &added.bolt11,
            amount_msat: to_i64(amount_msat),
            memo: &memo,
            source,
            expires_at,
        };
        if let Err(error) = self.db.create_invoice(&row).await {
            // Without a row nobody would be credited, so take the invoice back.
            if let Err(cancel) = self.lnd.cancel_invoice(added.payment_hash.clone()).await {
                error!(%cancel, payment_hash = %added.payment_hash, "unrecorded invoice is still open in LND");
            }
            return Err(error.into());
        }
        Ok(Invoice {
            payment_hash: added.payment_hash,
            account_id: account.id,
            bolt11: added.bolt11,
            amount_msat: to_i64(amount_msat),
            memo,
            source: source.to_owned(),
            state: "open".to_owned(),
            expires_at,
        })
    }

    fn check_send_amount(&self, amount_msat: u64) -> Result<(), WalletError> {
        if amount_msat == 0 {
            return Err(WalletError::invalid("Enter an amount above zero."));
        }
        if amount_msat > self.limits.max_payment_msat {
            return Err(WalletError::limit(format!(
                "The most you can send at once is {} sats.",
                format_msat(to_i64(self.limits.max_payment_msat))
            )));
        }
        Ok(())
    }

    pub(crate) async fn preview(&self, account: &Account, request: &PayRequest) -> Result<PaymentPreview, WalletError> {
        if account.frozen {
            return Err(WalletError::Frozen);
        }
        let destination = lnurl::parse_destination(&request.destination)?;
        let (recipient, amount, description, internal) = match &destination {
            Destination::Invoice(bolt11) => {
                let decoded = self.decode(bolt11).await?;
                let amount = if decoded.num_msat == 0 {
                    request
                        .amount_msat
                        .ok_or_else(|| WalletError::invalid("This invoice has no amount. Enter one in sats."))?
                } else {
                    decoded.num_msat
                };
                let own = self.db.invoice(&decoded.payment_hash).await?;
                let recipient = if let Some(invoice) = &own {
                    if invoice.account_id == account.id {
                        return Err(WalletError::invalid("That is your own invoice."));
                    }
                    if invoice.state != "open" {
                        return Err(WalletError::AlreadyPaid);
                    }
                    let recipient = self
                        .db
                        .account(invoice.account_id)
                        .await?
                        .ok_or(WalletError::NotFound)?;
                    if recipient.frozen {
                        return Err(WalletError::invalid("That account cannot receive payments right now."));
                    }
                    self.address(&recipient.username)
                } else {
                    format!("Lightning node {}", decoded.destination)
                };
                (
                    recipient,
                    amount,
                    truncate(&decoded.description, COMMENT_ALLOWED),
                    own.is_some(),
                )
            }
            _ => {
                let url = destination
                    .pay_url()
                    .ok_or_else(|| WalletError::invalid("That Lightning Address is not valid."))?;
                let amount = request
                    .amount_msat
                    .ok_or_else(|| WalletError::invalid("Enter an amount in sats."))?;
                self.check_send_amount(amount)?;
                let internal = self.own_username(&url);
                if let Some(username) = &internal {
                    let recipient = self
                        .db
                        .account_by_username(username)
                        .await?
                        .ok_or_else(|| WalletError::invalid("There is no account with that address here."))?;
                    if recipient.id == account.id {
                        return Err(WalletError::invalid("That is your own Lightning Address."));
                    }
                    if recipient.frozen {
                        return Err(WalletError::invalid("That account cannot receive payments right now."));
                    }
                } else {
                    let params = self.lnurl.pay_params(&url).await?;
                    if !(params.min_sendable..=params.max_sendable).contains(&amount) {
                        return Err(WalletError::invalid(format!(
                            "That address accepts {} to {} sats.",
                            format_msat(to_i64(params.min_sendable)),
                            format_msat(to_i64(params.max_sendable))
                        )));
                    }
                }
                let recipient = match &destination {
                    Destination::Address { user, domain } => format!("{user}@{domain}"),
                    _ => url.to_string(),
                };
                (
                    recipient,
                    amount,
                    truncate(&request.comment, COMMENT_ALLOWED),
                    internal.is_some(),
                )
            }
        };
        self.check_send_amount(amount)?;
        let fee_limit = if internal { 0 } else { self.fee_limit_msat(amount) };
        if self.db.balance(account.id).await? < to_i64(amount.saturating_add(fee_limit)) {
            return Err(WalletError::InsufficientBalance);
        }
        Ok(PaymentPreview {
            recipient,
            amount_msat: amount,
            fee_limit_msat: fee_limit,
            description,
        })
    }

    /// Pays an invoice, a Lightning Address, or an LNURL. A repeated request
    /// key returns the first attempt instead of paying again.
    pub(crate) async fn pay(self: &Arc<Self>, account: &Account, request: PayRequest) -> Result<Payment, WalletError> {
        if account.frozen {
            return Err(WalletError::Frozen);
        }
        if !valid_request_key(&request.request_key) {
            return Err(WalletError::invalid(
                "This form expired. Reload the page and try again.",
            ));
        }
        let key = format!("{}:{}", account.id, request.request_key);
        if let Some(existing) = self.db.payment_by_request_key(&key).await? {
            return Ok(existing);
        }
        let comment = truncate(&request.comment, COMMENT_ALLOWED);
        let destination = lnurl::parse_destination(&request.destination)?;
        if let Destination::Invoice(bolt11) = &destination {
            return self
                .pay_invoice(account, &key, bolt11.clone(), request.amount_msat)
                .await;
        }
        let url = destination
            .pay_url()
            .ok_or_else(|| WalletError::invalid("That Lightning Address is not valid."))?;
        let amount = request
            .amount_msat
            .ok_or_else(|| WalletError::invalid("Enter an amount in sats."))?;
        self.check_send_amount(amount)?;
        if let Some(username) = self.own_username(&url) {
            return self.pay_own_address(account, &key, &username, amount, &comment).await;
        }
        let counterparty = match &destination {
            Destination::Address { user, domain } => format!("{user}@{domain}"),
            _ => url.host_str().unwrap_or_default().to_owned(),
        };
        let params = self.lnurl.pay_params(&url).await?;
        if !(params.min_sendable..=params.max_sendable).contains(&amount) {
            return Err(WalletError::invalid(format!(
                "That address accepts {} to {} sats.",
                format_msat(to_i64(params.min_sendable)),
                format_msat(to_i64(params.max_sendable))
            )));
        }
        let bolt11 = self.lnurl.invoice(&params, amount, Some(&comment)).await?;
        let decoded = self.decode(&bolt11).await?;
        if decoded.num_msat != amount {
            return Err(WalletError::invalid(
                "The recipient's invoice is for a different amount.",
            ));
        }
        if decoded.description_hash != hex::encode(sha256(params.metadata.as_bytes())) {
            return Err(WalletError::invalid(
                "The recipient's invoice does not match its address metadata.",
            ));
        }
        let outgoing = Outgoing {
            bolt11,
            decoded,
            amount_msat: amount,
            amount_override: None,
            counterparty,
            memo: comment,
        };
        self.pay_outgoing(account, &key, outgoing).await
    }

    /// The username when a pay URL points at this server's own LNURL endpoint.
    fn own_username(&self, url: &Url) -> Option<String> {
        if url.host_str() != self.origin.host_str()
            || url.port_or_known_default() != self.origin.port_or_known_default()
        {
            return None;
        }
        url.path()
            .strip_prefix("/.well-known/lnurlp/")
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .map(str::to_ascii_lowercase)
    }

    /// Decodes an invoice this wallet could pay: on the node's network and not expired.
    pub(crate) async fn decode(&self, bolt11: &str) -> Result<DecodedInvoice, WalletError> {
        if lnd::is_mainnet_invoice(bolt11) {
            return Err(WalletError::invalid(
                "That is a mainnet invoice. This wallet only works on test networks.",
            ));
        }
        if let Some(network) = lnd::foreign_invoice_network(bolt11, &self.network) {
            return Err(WalletError::invalid(format!(
                "That invoice is for {network}, but this wallet runs on {}.",
                self.network_name
            )));
        }
        let decoded = self.lnd.decode_invoice(bolt11.to_owned()).await.map_err(|error| {
            warn!(%error, "cannot decode invoice");
            WalletError::invalid(format!(
                "Could not read that invoice. Is it a {} invoice?",
                self.network_name
            ))
        })?;
        if decoded.payment_hash.len() != 64 {
            return Err(WalletError::invalid("Could not read that invoice."));
        }
        let expiry = if decoded.expiry == 0 { 3600 } else { decoded.expiry };
        if i64::try_from(decoded.timestamp.saturating_add(expiry)).unwrap_or(i64::MAX) <= now() {
            return Err(WalletError::invalid(
                "That invoice has expired. Ask the recipient for a new one.",
            ));
        }
        Ok(decoded)
    }

    async fn pay_invoice(
        self: &Arc<Self>,
        account: &Account,
        key: &str,
        bolt11: String,
        amount_msat: Option<u64>,
    ) -> Result<Payment, WalletError> {
        let decoded = self.decode(&bolt11).await?;
        let (amount, amount_override) = if decoded.num_msat == 0 {
            let amount =
                amount_msat.ok_or_else(|| WalletError::invalid("This invoice has no amount. Enter one in sats."))?;
            (amount, Some(amount))
        } else {
            (decoded.num_msat, None)
        };
        let counterparty = format!("node {}", decoded.destination.chars().take(16).collect::<String>());
        let memo = truncate(&decoded.description, COMMENT_ALLOWED);
        let outgoing = Outgoing {
            bolt11,
            decoded,
            amount_msat: amount,
            amount_override,
            counterparty,
            memo,
        };
        self.pay_outgoing(account, key, outgoing).await
    }

    async fn pay_outgoing(
        self: &Arc<Self>,
        account: &Account,
        key: &str,
        outgoing: Outgoing,
    ) -> Result<Payment, WalletError> {
        self.check_send_amount(outgoing.amount_msat)?;
        if let Some(invoice) = self.db.invoice(&outgoing.decoded.payment_hash).await? {
            return self.pay_own_invoice(account, key, invoice).await;
        }
        let fee_limit = self.fee_limit_msat(outgoing.amount_msat);
        let send = NewSend {
            account_id: account.id,
            request_key: key,
            amount_msat: to_i64(outgoing.amount_msat),
            fee_limit_msat: to_i64(fee_limit),
            payment_hash: &outgoing.decoded.payment_hash,
            counterparty: &outgoing.counterparty,
            memo: &outgoing.memo,
        };
        let id = match self.db.begin_send(&send).await? {
            Started::Existing(payment) => return Ok(*payment),
            Started::New(id) => id,
        };
        self.set_in_flight(id, true);
        let request = SendRequest {
            bolt11: outgoing.bolt11,
            amount_msat: outgoing.amount_override,
            fee_limit_msat: fee_limit,
        };
        let wallet = Arc::clone(self);
        let task = tokio::spawn(async move { wallet.drive_send(id, request).await });
        // Wait a little for the result; the payment carries on if the page gives up.
        let _ = tokio::time::timeout(self.send_wait, task).await;
        self.db.payment(account.id, id).await?.ok_or(WalletError::NotFound)
    }

    fn set_in_flight(&self, id: i64, live: bool) {
        let mut in_flight = self.in_flight.lock().expect("in-flight lock is not poisoned");
        if live {
            in_flight.insert(id);
        } else {
            in_flight.remove(&id);
        }
    }

    fn is_in_flight(&self, id: i64) -> bool {
        self.in_flight
            .lock()
            .expect("in-flight lock is not poisoned")
            .contains(&id)
    }

    async fn drive_send(&self, id: i64, request: SendRequest) {
        let fee_limit = request.fee_limit_msat;
        match self.lnd.send_payment(request).await {
            Ok(PaymentStatus::Succeeded { fee_msat }) => {
                if fee_msat > fee_limit {
                    warn!(
                        id,
                        fee_msat, fee_limit, "LND paid more fee than the limit; the operator covers it"
                    );
                }
                self.finish_send(
                    id,
                    SendOutcome::Succeeded {
                        fee_msat: to_i64(fee_msat),
                    },
                )
                .await;
            }
            Ok(PaymentStatus::Failed { reason }) => self.finish_send(id, SendOutcome::Failed { reason }).await,
            Ok(status) => warn!(id, ?status, "payment outcome unknown; reconciliation will settle it"),
            Err(error) => warn!(id, %error, "payment outcome unknown; reconciliation will settle it"),
        }
        self.set_in_flight(id, false);
    }

    async fn finish_send(&self, id: i64, outcome: SendOutcome) {
        match self.db.finish_send(id, &outcome).await {
            Ok(true) => {
                let counter = match outcome {
                    SendOutcome::Succeeded { .. } => &self.metrics.sends_succeeded,
                    SendOutcome::Failed { .. } => &self.metrics.sends_failed,
                };
                inc(counter);
                info!(id, ?outcome, "payment finished");
            }
            Ok(false) => {}
            Err(error) => error!(id, %error, "cannot record payment outcome"),
        }
    }

    /// Pays another account's invoice inside the ledger. The invoice is
    /// canceled in LND first, so it cannot also be paid over Lightning.
    async fn pay_own_invoice(&self, account: &Account, key: &str, invoice: Invoice) -> Result<Payment, WalletError> {
        if invoice.account_id == account.id {
            return Err(WalletError::invalid("That is your own invoice."));
        }
        if invoice.state != "open" {
            return Err(WalletError::AlreadyPaid);
        }
        if invoice.expires_at <= now() {
            return Err(WalletError::invalid(
                "That invoice has expired. Ask the recipient for a new one.",
            ));
        }
        if self.db.balance(account.id).await? < invoice.amount_msat {
            return Err(WalletError::InsufficientBalance);
        }
        let recipient = self
            .db
            .account(invoice.account_id)
            .await?
            .ok_or(WalletError::NotFound)?;
        if recipient.frozen {
            return Err(WalletError::invalid("That account cannot receive payments right now."));
        }
        let recipient = self.address(&recipient.username);
        if let Err(error) = self.lnd.cancel_invoice(invoice.payment_hash.clone()).await {
            warn!(%error, "cannot cancel invoice for an internal payment");
            return Err(WalletError::invalid(
                "That invoice is being paid right now. Check your history in a moment.",
            ));
        }
        let sender = self.address(&account.username);
        match self
            .db
            .pay_invoice_internally(account, key, &invoice, (&sender, &recipient))
            .await
        {
            Ok(payment) => {
                inc(&self.metrics.internal_payments);
                Ok(payment)
            }
            Err(error) => {
                // LND has canceled it, so record that it can no longer be paid.
                if let Err(mark) = self.db.mark_invoice_canceled(&invoice.payment_hash).await {
                    error!(%mark, "cannot mark invoice canceled");
                }
                Err(error)
            }
        }
    }

    async fn pay_own_address(
        &self,
        account: &Account,
        key: &str,
        username: &str,
        amount_msat: u64,
        comment: &str,
    ) -> Result<Payment, WalletError> {
        let recipient = self
            .db
            .account_by_username(username)
            .await?
            .ok_or_else(|| WalletError::invalid("There is no account with that address here."))?;
        let sender_label = self.address(&account.username);
        let recipient_label = self.address(&recipient.username);
        let transfer = Transfer {
            sender: account,
            recipient: &recipient,
            sender_label: &sender_label,
            recipient_label: &recipient_label,
            amount_msat: to_i64(amount_msat),
            request_key: key,
            memo: comment,
            max_balance_msat: to_i64(self.limits.max_balance_msat),
        };
        let payment = self.db.transfer(&transfer).await?;
        inc(&self.metrics.internal_payments);
        Ok(payment)
    }

    /// Operator-funded test sats, within the faucet limits and the node's
    /// channel balance. `client` is the claimant's rate-limit key.
    pub(crate) async fn faucet(
        &self,
        account: &Account,
        request_key: &str,
        client: &str,
    ) -> Result<Payment, WalletError> {
        if !self.faucet.enabled {
            return Err(WalletError::invalid("The faucet is turned off."));
        }
        if account.frozen {
            return Err(WalletError::Frozen);
        }
        if !valid_request_key(request_key) {
            return Err(WalletError::invalid(
                "This form expired. Reload the page and try again.",
            ));
        }
        let key = format!("{}:{request_key}", account.id);
        if let Some(existing) = self.db.payment_by_request_key(&key).await? {
            return Ok(existing);
        }
        // Balances are claims on the node: never promise more than its channels hold.
        let node = self.lnd.balances().await.map_err(|error| {
            warn!(%error, "cannot read node balance for the faucet");
            WalletError::Unavailable
        })?;
        let liabilities = u64::try_from(self.db.liabilities().await?).unwrap_or(0);
        if liabilities.saturating_add(self.faucet.amount_msat) > node.channel_local_msat {
            return Err(WalletError::limit("The faucet is empty right now."));
        }
        let grant = Grant {
            account_id: account.id,
            kind: "faucet",
            amount_msat: to_i64(self.faucet.amount_msat),
            request_key: &key,
            memo: "Test sats from the faucet",
            max_balance_msat: to_i64(self.limits.max_balance_msat),
            daily_limits: Some(DailyLimits {
                per_account: to_i64(self.faucet.per_account_daily_msat),
                per_address: to_i64(self.faucet.per_address_daily_msat),
                global: to_i64(self.faucet.global_daily_msat),
                client,
            }),
        };
        let payment = self.db.grant(&grant).await?;
        inc(&self.metrics.faucet_grants);
        self.metrics
            .faucet_paid_msat
            .fetch_add(self.faucet.amount_msat, Ordering::Relaxed);
        Ok(payment)
    }

    /// A manual credit from the operator page.
    pub(crate) async fn operator_credit(
        &self,
        account_id: i64,
        amount_msat: u64,
        note: &str,
        request_key: &str,
    ) -> Result<Payment, WalletError> {
        let memo = truncate(note, COMMENT_ALLOWED);
        let grant = Grant {
            account_id,
            kind: "operator",
            amount_msat: to_i64(amount_msat),
            request_key,
            memo: if memo.is_empty() {
                "Credit from the operator"
            } else {
                &memo
            },
            max_balance_msat: to_i64(self.limits.max_balance_msat),
            daily_limits: None,
        };
        self.db.grant(&grant).await
    }

    /// Credits a settled LND invoice to its account, once.
    pub(crate) async fn credit_settled(&self, invoice: &LndInvoice) -> Result<(), WalletError> {
        if !invoice.is_settled() {
            return Ok(());
        }
        let Some(payment_hash) = invoice.payment_hash() else {
            return Ok(());
        };
        if let Some(credited) = self
            .db
            .settle_invoice(&payment_hash, to_i64(invoice.amt_paid_msat))
            .await?
        {
            let counter = if credited.source == "lnurl" {
                &self.metrics.invoices_settled_lnurl
            } else {
                &self.metrics.invoices_settled_wallet
            };
            inc(counter);
            info!(%payment_hash, amount_msat = invoice.amt_paid_msat, "invoice paid");
        }
        Ok(())
    }

    /// Follows LND's invoice stream forever, reconnecting with backoff. Each
    /// connection resumes after the last settle index processed.
    pub(crate) async fn follow_invoices(self: Arc<Self>) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            match self.follow_invoices_once().await {
                Ok(()) => warn!("LND invoice stream ended; reconnecting"),
                Err(error) => warn!(%error, "LND invoice stream failed; reconnecting"),
            }
            self.metrics.invoice_stream_up.store(false, Ordering::Relaxed);
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    async fn follow_invoices_once(&self) -> anyhow::Result<()> {
        let cursor = u64::try_from(self.db.cursor(SETTLE_CURSOR).await?).unwrap_or(0);
        let mut stream = self.lnd.subscribe_invoices(cursor).await?;
        self.metrics.invoice_stream_up.store(true, Ordering::Relaxed);
        info!(settle_index = cursor, "following LND invoices");
        while let Some(invoice) = stream.next().await? {
            self.credit_settled(&invoice).await?;
            if invoice.is_settled() && invoice.settle_index > 0 {
                self.db
                    .advance_cursor(SETTLE_CURSOR, to_i64(invoice.settle_index))
                    .await?;
            }
        }
        Ok(())
    }

    /// Runs [`Wallet::reconcile`] now and then every minute.
    pub(crate) async fn reconcile_forever(self: Arc<Self>) {
        loop {
            self.reconcile().await;
            tokio::time::sleep(RECONCILE_EVERY).await;
        }
    }

    /// Brings the ledger in line with LND: credits invoices paid while the
    /// stream was down, and settles payments whose outcome was not recorded.
    pub(crate) async fn reconcile(&self) {
        self.reconcile_invoices().await;
        self.reconcile_sends(now() - RECONCILE_GRACE_SECS).await;
        match self.lnd.balances().await {
            Ok(balances) => {
                self.metrics
                    .node_channel_local_msat
                    .store(balances.channel_local_msat, Ordering::Relaxed);
                self.metrics.node_balance_known.store(true, Ordering::Relaxed);
                *self.node_balances.lock().expect("balance lock is not poisoned") = Some((balances, now()));
            }
            Err(error) => {
                self.metrics.node_balance_known.store(false, Ordering::Relaxed);
                warn!(%error, "cannot read node balance");
            }
        }
    }

    async fn reconcile_invoices(&self) {
        let open = match self.db.open_invoices(500).await {
            Ok(open) => open,
            Err(error) => return error!(%error, "cannot list open invoices"),
        };
        for invoice in open {
            match self.lnd.lookup_invoice(invoice.payment_hash.clone()).await {
                Ok(Some(lnd)) if lnd.is_settled() => {
                    if let Err(error) = self.credit_settled(&lnd).await {
                        error!(%error, "cannot credit a settled invoice");
                    }
                }
                Ok(Some(lnd)) if lnd.is_canceled() => self.mark_canceled(&invoice.payment_hash).await,
                // LND no longer knows it (a reset node) and it has expired: it cannot be paid.
                Ok(None) if invoice.expires_at < now() - 3600 => self.mark_canceled(&invoice.payment_hash).await,
                Ok(_) => {}
                Err(error) => return warn!(%error, "cannot look up invoices; will retry"),
            }
        }
    }

    async fn mark_canceled(&self, payment_hash: &str) {
        if let Err(error) = self.db.mark_invoice_canceled(payment_hash).await {
            error!(%error, "cannot mark invoice canceled");
        }
    }

    /// Settles pending sends created before `created_before` that no live task is driving.
    pub(crate) async fn reconcile_sends(&self, created_before: i64) {
        let pending = match self.db.pending_sends(created_before).await {
            Ok(pending) => pending,
            Err(error) => return error!(%error, "cannot list pending payments"),
        };
        for send in pending {
            if self.is_in_flight(send.id) {
                continue;
            }
            let outcome = match self.lnd.track_payment(send.payment_hash.clone()).await {
                Ok(PaymentStatus::Succeeded { fee_msat }) => SendOutcome::Succeeded {
                    fee_msat: to_i64(fee_msat),
                },
                Ok(PaymentStatus::Failed { reason }) => SendOutcome::Failed { reason },
                Ok(PaymentStatus::NotFound) => SendOutcome::Failed {
                    reason: "The node never started this payment.".to_owned(),
                },
                Ok(PaymentStatus::InFlight) => continue,
                Err(error) => {
                    warn!(id = send.id, %error, "cannot track payment; will retry");
                    continue;
                }
            };
            self.finish_send(send.id, outcome).await;
        }
    }
}
