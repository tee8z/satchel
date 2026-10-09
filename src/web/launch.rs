//! Deep links: `/launch/lightning/{invoice or address}` opens Send with the
//! target filled in. Nothing is paid until the person taps Pay.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use url::form_urlencoded;

use super::pages::{self, Ctx, SendValues};
use super::user::shown;
use super::{App, Reject, Shared, UserSession, redirect};
use crate::error::WalletError;
use crate::lnd::DecodedInvoice;
use crate::lnurl::{self, Destination};
use crate::util::{format_msat, format_time, random_token};

const MINUTE: Duration = Duration::from_secs(60);

/// What a link points at, as the Send field should hold it, and a description.
async fn prepare(app: &App, target: &str) -> Result<(String, Markup, Option<String>), WalletError> {
    match lnurl::parse_destination(target)? {
        Destination::Invoice(bolt11) => {
            let decoded = app.wallet.decode(&bolt11).await?;
            Ok((bolt11, invoice_details(&decoded), invoice_amount(decoded.num_msat)))
        }
        Destination::Address { user, domain } => {
            let address = format!("{user}@{domain}");
            let details = html! { p { "To " strong { (address) } ". Enter the amount below." } };
            Ok((address, details, None))
        }
        Destination::Lnurl(url) => {
            let details =
                html! { p { "To " strong { (url.host_str().unwrap_or_default()) } ". Enter the amount below." } };
            Ok((target.trim().to_owned(), details, None))
        }
    }
}

/// A fixed invoice amount for an input, retaining its exact millisatoshis.
pub(super) fn invoice_amount(msat: u64) -> Option<String> {
    (msat > 0).then(|| match msat % 1000 {
        0 => (msat / 1000).to_string(),
        rest => format!("{}.{rest:03}", msat / 1000),
    })
}

fn invoice_details(decoded: &DecodedInvoice) -> Markup {
    let expiry = if decoded.expiry == 0 { 3600 } else { decoded.expiry };
    let expires_at = i64::try_from(decoded.timestamp.saturating_add(expiry)).unwrap_or(i64::MAX);
    html! {
        dl {
            dt { "Amount" }
            dd {
                @if decoded.num_msat == 0 {
                    "Not set; enter it below"
                } @else {
                    (format_msat(i64::try_from(decoded.num_msat).unwrap_or(i64::MAX))) " sats"
                }
            }
            @if !decoded.description.is_empty() {
                dt { "Description" } dd { (decoded.description) }
            }
            dt { "Expires" } dd { (format_time(expires_at)) }
        }
    }
}

pub(super) async fn lightning(
    State(app): State<Shared>,
    headers: HeaderMap,
    uri: Uri,
    Path(target): Path<String>,
) -> Result<Response, Reject> {
    let Some(session) = UserSession::from_headers(&app, &headers).await? else {
        let next: String = form_urlencoded::byte_serialize(uri.path().as_bytes()).collect();
        return Ok(redirect(&format!("/login?next={next}")));
    };
    let account_key = session.account.id.to_string();
    let prepared = if app.allow("launch", &account_key, app.rate.send_per_account_per_minute, MINUTE) {
        prepare(&app, &target).await
    } else {
        Err(WalletError::invalid("Too many links opened. Wait a minute."))
    };
    // Errors show in the Send form as usual, with the link's text kept.
    let (destination, details, amount, error) = match prepared {
        Ok((destination, details, amount)) => (destination, Some(details), amount, None),
        Err(error) => (target.trim().to_owned(), None, None, Some(shown(&error))),
    };
    let values = SendValues {
        destination: &destination,
        amount: amount.as_deref().unwrap_or_default(),
        fixed_amount: amount.is_some(),
        details,
        ..SendValues::default()
    };
    let send = pages::send_section(&session.csrf, &random_token(), &values, error.as_deref().map(Err));
    let balance = app.wallet.db.balance(session.account.id).await?;
    let page = pages::layout(
        &Ctx::member(&app.wallet, &session),
        "Send",
        html! {
            (pages::action_heading("Send", balance, session.account.frozen))
            (send)
        },
    );
    Ok(page.into_response())
}
