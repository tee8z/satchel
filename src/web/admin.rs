//! The operator's pages, behind a separate password and session.

use std::time::Duration;

use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::pages::{self, Ctx};
use super::{ClientIp, OperatorSession, Reject, Shared, check_csrf, redirect, redirect_with_cookie};
use crate::auth;
use crate::error::WalletError;
use crate::metrics::inc;
use crate::util::{parse_sats, random_token};

const MINUTE: Duration = Duration::from_secs(60);

pub(super) async fn login_page(State(app): State<Shared>) -> Result<Response, Reject> {
    if app.admin_hash.is_none() {
        return Err(Reject::NotFound);
    }
    Ok(pages::admin_login(&Ctx::visitor(&app.wallet.network), None).into_response())
}

#[derive(Deserialize)]
pub(super) struct LoginForm {
    password: String,
}

pub(super) async fn login(
    State(app): State<Shared>,
    ip: ClientIp,
    Form(form): Form<LoginForm>,
) -> Result<Response, Reject> {
    let Some(hash) = app.admin_hash.clone() else {
        return Err(Reject::NotFound);
    };
    let ctx = Ctx::visitor(&app.wallet.network);
    if !app.allow("operator-login", &ip.key(), app.rate.login_per_ip_per_minute, MINUTE) {
        return Ok(pages::admin_login(&ctx, Some("Too many attempts. Wait a minute.")).into_response());
    }
    let password = form.password.clone();
    let verified = tokio::task::spawn_blocking(move || auth::verify_password(&hash, &password))
        .await
        .unwrap_or(false);
    if !verified {
        inc(&app.wallet.metrics.login_failures);
        tracing::warn!(client = %ip.key(), "operator login failed");
        return Ok(pages::admin_login(&ctx, Some("Wrong password.")).into_response());
    }
    tracing::info!(client = %ip.key(), "operator logged in");
    let cookie = app.start_session(None).await?;
    Ok(redirect_with_cookie("/admin", cookie))
}

#[derive(Deserialize)]
pub(super) struct CsrfForm {
    csrf: String,
}

pub(super) async fn logout(
    State(app): State<Shared>,
    operator: OperatorSession,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Reject> {
    check_csrf(&operator.csrf, &form.csrf)?;
    app.wallet.db.delete_session(&operator.token_hash).await?;
    Ok(redirect_with_cookie("/", app.set_cookie(&app.admin_cookie(), "", 0)))
}

#[derive(Deserialize)]
pub(super) struct DashboardQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    done: String,
    #[serde(default)]
    error: String,
}

/// Fixed notices, so query parameters never put text on the page.
fn notice(query: &DashboardQuery) -> Option<Result<&'static str, &'static str>> {
    match (query.done.as_str(), query.error.as_str()) {
        ("credit", _) => Some(Ok("Credit added.")),
        ("freeze", _) => Some(Ok("Account frozen.")),
        ("unfreeze", _) => Some(Ok("Account unfrozen.")),
        (_, "amount") => Some(Err("Enter a whole number of sats above zero.")),
        (_, "limit") => Some(Err("Refused: the account would pass its balance limit.")),
        (_, "frozen") => Some(Err("Refused: the account is frozen.")),
        (_, "missing") => Some(Err("No such account.")),
        (_, "failed") => Some(Err("The credit failed. Check the logs.")),
        _ => None,
    }
}

pub(super) async fn dashboard(
    State(app): State<Shared>,
    operator: OperatorSession,
    Query(query): Query<DashboardQuery>,
) -> Result<Response, Reject> {
    let db = &app.wallet.db;
    let totals = db.totals().await?;
    let node = tokio::time::timeout(Duration::from_secs(5), app.wallet.lnd.balances())
        .await
        .ok()
        .and_then(Result::ok);
    let search = query.q.trim();
    let accounts = db.account_summaries(search, 200).await?;
    let page = pages::AdminPage {
        csrf: &operator.csrf,
        totals: &totals,
        node,
        accounts: &accounts,
        search,
        faucet: app.wallet.faucet,
        notice: notice(&query),
    };
    Ok(pages::admin_dashboard(&Ctx::operator(&app.wallet.network, &operator.csrf), &page).into_response())
}

#[derive(Deserialize)]
pub(super) struct FreezeForm {
    csrf: String,
    frozen: String,
}

pub(super) async fn freeze(
    State(app): State<Shared>,
    operator: OperatorSession,
    Path(id): Path<i64>,
    Form(form): Form<FreezeForm>,
) -> Result<Response, Reject> {
    check_csrf(&operator.csrf, &form.csrf)?;
    let frozen = form.frozen == "1";
    if !app.wallet.db.set_frozen(id, frozen).await? {
        return Err(Reject::NotFound);
    }
    tracing::info!(account = id, frozen, "operator changed an account");
    Ok(redirect(if frozen {
        "/admin?done=freeze"
    } else {
        "/admin?done=unfreeze"
    }))
}

#[derive(Deserialize)]
pub(super) struct CreditForm {
    csrf: String,
    amount_sat: String,
    #[serde(default)]
    note: String,
}

pub(super) async fn credit(
    State(app): State<Shared>,
    operator: OperatorSession,
    Path(id): Path<i64>,
    Form(form): Form<CreditForm>,
) -> Result<Response, Reject> {
    check_csrf(&operator.csrf, &form.csrf)?;
    let Some(amount_msat) = parse_sats(&form.amount_sat)
        .filter(|sats| *sats > 0)
        .and_then(|sats| sats.checked_mul(1000))
    else {
        return Ok(redirect("/admin?error=amount"));
    };
    let key = format!("operator:{}", random_token());
    match app.wallet.operator_credit(id, amount_msat, &form.note, &key).await {
        Ok(_) => {
            tracing::info!(account = id, amount_msat, "operator credit");
            Ok(redirect("/admin?done=credit"))
        }
        Err(error) => {
            let code = match error {
                WalletError::LimitExceeded(_) => "limit",
                WalletError::Frozen => "frozen",
                WalletError::NotFound => "missing",
                other => {
                    tracing::error!(error = %other, account = id, "operator credit failed");
                    "failed"
                }
            };
            Ok(redirect(&format!("/admin?error={code}")))
        }
    }
}
