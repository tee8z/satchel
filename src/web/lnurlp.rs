//! LNURL-pay endpoints for every account (LUD-06, LUD-12 comments, LUD-16 addresses).

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use serde_json::{Value, json};

use super::{App, ClientIp, Shared};
use crate::abuse;
use crate::db::Account;
use crate::error::{LnurlError, WalletError};
use crate::lnurl::{self, COMMENT_ALLOWED};

const MINUTE: Duration = Duration::from_secs(60);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PayParams {
    callback: String,
    max_sendable: u64,
    min_sendable: u64,
    metadata: String,
    tag: &'static str,
    comment_allowed: usize,
}

async fn find(app: &App, username: &str) -> Result<Account, LnurlError> {
    match app.wallet.db.account_by_username(&username.to_ascii_lowercase()).await {
        Ok(Some(account)) if account.frozen => Err(LnurlError::new("This address cannot receive payments right now")),
        Ok(Some(account)) => Ok(account),
        Ok(None) => Err(LnurlError::new("Unknown Lightning Address")),
        Err(error) => {
            tracing::error!(%error, "cannot look up an address");
            Err(LnurlError::new("Service unavailable; retry later"))
        }
    }
}

pub(super) async fn params(
    State(app): State<Shared>,
    ip: ClientIp,
    Path(username): Path<String>,
) -> Result<Json<PayParams>, LnurlError> {
    if !app.allow("lnurl-ip", &ip.key(), app.rate.lnurl_per_ip_per_minute, MINUTE) {
        return Err(LnurlError::new("Too many requests; retry later"));
    }
    let account = find(&app, &username).await?;
    let range = app
        .wallet
        .receivable(&account)
        .await
        .map_err(|_| LnurlError::new("Service unavailable; retry later"))?;
    let Some((min, max)) = range else {
        return Err(LnurlError::new("This address cannot receive payments right now"));
    };
    Ok(Json(PayParams {
        callback: app.wallet.lnurl_callback(&account.username),
        max_sendable: max,
        min_sendable: min,
        metadata: lnurl::metadata(&account.username, &app.wallet.domain),
        tag: "payRequest",
        comment_allowed: COMMENT_ALLOWED,
    }))
}

/// `amount` (msat) is required; `comment` is optional; other parameters, such as `nonce`, are ignored.
pub(crate) fn parse_callback_query(query: Option<&str>) -> Result<(u64, String), LnurlError> {
    let mut amount = None;
    let mut comment = String::new();
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "amount" => {
                if amount.is_some() {
                    return Err(LnurlError::new("Duplicate amount parameter"));
                }
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(LnurlError::new("Amount must be an integer in millisatoshis"));
                }
                amount = Some(value.parse::<u64>().map_err(|_| LnurlError::new("Invalid amount"))?);
            }
            "comment" => comment = value.into_owned(),
            _ => {}
        }
    }
    let amount = amount.ok_or_else(|| LnurlError::new("Missing amount parameter"))?;
    if comment.chars().count() > COMMENT_ALLOWED {
        return Err(LnurlError::new("Comment is too long"));
    }
    Ok((amount, comment))
}

pub(super) async fn callback(
    State(app): State<Shared>,
    ip: ClientIp,
    Path(username): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, LnurlError> {
    if !app.allow("lnurl-ip", &ip.key(), app.rate.lnurl_per_ip_per_minute, MINUTE) {
        return Err(LnurlError::new("Too many requests; retry later"));
    }
    let (amount_msat, comment) = parse_callback_query(query.as_deref())?;
    let account = find(&app, &username).await?;
    let account_key = account.id.to_string();
    if !app.allow(
        "lnurl-account",
        &account_key,
        app.rate.lnurl_per_account_per_minute,
        MINUTE,
    ) {
        return Err(LnurlError::new("Too many requests for this address; retry later"));
    }
    let invoice = app
        .wallet
        .create_invoice(&account, amount_msat, &comment, true)
        .await
        .map_err(|error| match error {
            WalletError::LimitExceeded(_) | WalletError::Frozen => {
                LnurlError::new("Amount is outside the accepted range")
            }
            WalletError::TooManyInvoices => LnurlError::new("This address has too many unpaid invoices; retry later"),
            WalletError::Unavailable => LnurlError::new("Could not create an invoice; retry later"),
            other => {
                tracing::error!(error = %other, "LNURL invoice failed");
                LnurlError::new("Could not create an invoice")
            }
        })?;
    let recorded = app
        .wallet
        .db
        .record_client_event(abuse::LNURL_INVOICE, &ip.key(), account.id, invoice.amount_msat)
        .await;
    if let Err(error) = recorded {
        tracing::error!(%error, "cannot record an LNURL invoice request");
    }
    Ok(Json(json!({ "pr": invoice.bolt11, "routes": [] })))
}
