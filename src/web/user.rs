//! Pages for account holders: sign up, log in, wallet, settings.

use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use maud::{Markup, html};
use serde::Deserialize;
use serde_json::json;

use super::pages::{self, Ctx, HandoffSignup, ReceiveState, SendValues, WalletView};
use super::{App, ClientIp, Reject, Shared, UserSession, busy, check_csrf, is_htmx, redirect, redirect_with_cookie};
use crate::auth;
use crate::error::WalletError;
use crate::handoff::{Purpose, safe_next, short_npub};
use crate::lnurl;
use crate::metrics::inc;
use crate::nostr;
use crate::util::{now, parse_sats, random_token};
use crate::wallet::PayRequest;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const HISTORY_LEN: i64 = 25;

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.is_unique_violation())
}

/// The message to show for a wallet error; unexpected ones are logged.
pub(super) fn shown(error: &WalletError) -> String {
    if let WalletError::Internal(source) = error {
        tracing::error!(error = %source, "wallet operation failed");
    }
    error.to_string()
}

fn msat_from_sats(input: &str) -> Option<u64> {
    parse_sats(input).and_then(|sats| sats.checked_mul(1000))
}

pub(super) async fn home(State(app): State<Shared>, headers: HeaderMap) -> Result<Response, Reject> {
    if UserSession::from_headers(&app, &headers).await?.is_some() {
        return Ok(redirect("/wallet"));
    }
    Ok(pages::landing(&Ctx::visitor(&app.wallet), &app.wallet.domain).into_response())
}

pub(super) async fn signup_page(
    State(app): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<NextQuery>,
) -> Result<Response, Reject> {
    let next = safe_next(&query.next);
    if UserSession::from_headers(&app, &headers).await?.is_some() {
        return Ok(redirect(&next));
    }
    Ok(pages::signup(&Ctx::visitor(&app.wallet), &app.wallet.domain, None, "", &next).into_response())
}

#[derive(Deserialize)]
pub(super) struct SignupForm {
    username: String,
    #[serde(default)]
    next: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
    /// A pending handoff token instead of a password: the new wallet uses the
    /// Nostr key the server holds behind it.
    #[serde(default)]
    handoff: String,
    #[serde(default)]
    pow_challenge: String,
    #[serde(default)]
    pow_nonce: String,
}

/// Every new wallet comes through here, from the password form or a handoff.
pub(super) async fn signup(
    State(app): State<Shared>,
    ip: ClientIp,
    Form(form): Form<SignupForm>,
) -> Result<Response, Reject> {
    let ctx = Ctx::visitor(&app.wallet);
    let pending = match form.handoff.as_str() {
        "" => None,
        token => match app.wallet.db.pending_handoff(token, Purpose::SignUp).await? {
            Some(pending) => Some(pending),
            None => return Ok(super::handoff::expired(&ctx)),
        },
    };
    let next = pending
        .as_ref()
        .map(|pending| pending.next.clone())
        .unwrap_or_else(|| safe_next(&form.next));
    let npub = pending.as_ref().map(|pending| short_npub(&pending.nostr_pubkey));
    let handoff_page = npub.as_deref().map(|npub| HandoffSignup {
        token: &form.handoff,
        npub,
    });
    let retry = |message: &str| {
        pages::signup_with(
            &ctx,
            &app.wallet.domain,
            Some(message),
            &form.username,
            handoff_page.as_ref(),
            &next,
        )
        .into_response()
    };
    let username = match auth::normalize_username(&form.username, &app.reserved) {
        Ok(username) => username,
        Err(message) => return Ok(retry(message)),
    };
    // A password sign-up checks its rules and takes a password worker before
    // it spends the network's sign-up allowance or the proof of work.
    let permit = match &pending {
        Some(_) => None,
        None => {
            if let Err(message) = auth::check_new_password(&form.password, &form.confirm) {
                return Ok(retry(message));
            }
            let Ok(permit) = app.passwords.admit() else {
                return Ok(busy(&retry));
            };
            Some(permit)
        }
    };
    if !app.allow("signup", &ip.key(), app.rate.signup_per_ip_per_hour, HOUR) {
        return Ok(retry("Too many new wallets from your network. Try again later."));
    }
    if let Some(pending) = &pending
        && app.wallet.db.account_by_nostr(&pending.nostr_pubkey).await?.is_some()
    {
        return Ok(retry(
            "This Nostr key already has a wallet. Go back to the app and open it again.",
        ));
    }
    if app.wallet.db.account_by_username(&username).await?.is_some() {
        return Ok(retry("That username is taken."));
    }
    // Both ways in pay the proof of work, before any password hashing.
    if let Some(message) = app.refuse_new_account(&form.pow_challenge, &form.pow_nonce).await? {
        return Ok(retry(message));
    }
    let (hash, nostr_pubkey) = match permit {
        Some(permit) => {
            let password = form.password.clone();
            let hash = permit
                .run(move || auth::hash_password(&password))
                .await?
                .map_err(|_| Reject::Server)?;
            (Some(hash), None)
        }
        None => (None, pending.as_ref().map(|pending| pending.nostr_pubkey.as_str())),
    };
    let account = match app
        .wallet
        .db
        .create_account(&username, hash.as_deref(), nostr_pubkey)
        .await
    {
        Ok(account) => account,
        Err(error) if is_unique_violation(&error) && nostr_pubkey.is_some() && !taken(&error) => {
            return Ok(retry(
                "This Nostr key already has a wallet. Go back to the app and open it again.",
            ));
        }
        Err(error) if is_unique_violation(&error) => return Ok(retry("That username is taken.")),
        Err(error) => return Err(error.into()),
    };
    inc(&app.wallet.metrics.signups);
    app.note_new_account(account.id, ip).await;
    tracing::info!(account = account.id, handoff = pending.is_some(), "account created");
    let next = match &pending {
        Some(pending) => {
            app.wallet
                .db
                .take_pending_handoff(&form.handoff, Purpose::SignUp)
                .await?;
            pending.next.clone()
        }
        None => next,
    };
    let cookie = app.start_session(Some(account.id)).await?;
    Ok(redirect_with_cookie(&next, cookie))
}

/// Whether a unique violation was on the username (and not the Nostr key).
fn taken(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.message().contains("accounts.username"))
}

#[derive(Deserialize)]
pub(super) struct NextQuery {
    #[serde(default)]
    next: String,
}

pub(super) async fn login_page(
    State(app): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<NextQuery>,
) -> Result<Response, Reject> {
    let next = safe_next(&query.next);
    if UserSession::from_headers(&app, &headers).await?.is_some() {
        return Ok(redirect(&next));
    }
    Ok(pages::login(&Ctx::visitor(&app.wallet), None, "", &next).into_response())
}

#[derive(Deserialize)]
pub(super) struct LoginForm {
    username: String,
    password: String,
    /// Where to go afterwards, such as a deep link that sent the visitor here.
    #[serde(default)]
    next: String,
}

pub(super) async fn login(
    State(app): State<Shared>,
    ip: ClientIp,
    Form(form): Form<LoginForm>,
) -> Result<Response, Reject> {
    let ctx = Ctx::visitor(&app.wallet);
    let next = safe_next(&form.next);
    let retry = |message: &str| pages::login(&ctx, Some(message), &form.username, &next).into_response();
    let username = form.username.trim().to_ascii_lowercase();
    let Ok(permit) = app.passwords.admit() else {
        return Ok(busy(&retry));
    };
    if !app.allow("login-ip", &ip.key(), app.rate.login_per_ip_per_minute, MINUTE)
        || !app.allow("login-account", &username, app.rate.login_per_account_per_hour, HOUR)
    {
        return Ok(retry("Too many login attempts. Wait a minute and try again."));
    }
    let account = app.wallet.db.account_by_username(&username).await?;
    // Unknown names check a dummy hash, so both failures take as long.
    let hash = account.as_ref().and_then(|account| account.password_hash.clone());
    let password = form.password.clone();
    let verified = permit
        .run(move || {
            // Lazy dummy-hash initialization also stays inside the worker budget.
            let hash = hash.unwrap_or_else(|| auth::DUMMY_HASH.clone());
            auth::verify_password(&hash, &password)
        })
        .await?;
    let Some(account) = account.filter(|account| verified && account.password_hash.is_some()) else {
        inc(&app.wallet.metrics.login_failures);
        return Ok(retry("Wrong username or password."));
    };
    if account.frozen {
        return Ok(retry("This account is frozen. Contact the operator."));
    }
    let cookie = app.start_session(Some(account.id)).await?;
    Ok(redirect_with_cookie(&next, cookie))
}

#[derive(Deserialize)]
pub(super) struct CsrfForm {
    csrf: String,
}

pub(super) async fn logout(
    State(app): State<Shared>,
    session: UserSession,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    app.wallet.db.delete_session(&session.token_hash).await?;
    Ok(redirect_with_cookie("/", app.set_cookie(&app.session_cookie(), "", 0)))
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn json_redirect(to: &str, cookie: Option<HeaderValue>) -> Response {
    let mut response = Json(json!({ "redirect": to })).into_response();
    if let Some(cookie) = cookie {
        response.headers_mut().insert(SET_COOKIE, cookie);
    }
    response
}

pub(super) async fn nostr_challenge(State(app): State<Shared>, ip: ClientIp) -> Response {
    if !app.allow("nostr", &ip.key(), app.rate.login_per_ip_per_minute, MINUTE) {
        return json_error(StatusCode::TOO_MANY_REQUESTS, "Too many attempts. Wait a minute.");
    }
    match app.challenges.issue() {
        Some(challenge) => Json(json!({ "challenge": challenge, "url": app.nostr_auth_url() })).into_response(),
        None => json_error(StatusCode::SERVICE_UNAVAILABLE, "Busy. Try again shortly."),
    }
}

#[derive(Deserialize)]
pub(super) struct NostrAuth {
    mode: String,
    event: nostr::Event,
    #[serde(default)]
    username: String,
    #[serde(default)]
    csrf: String,
    #[serde(default)]
    next: String,
    /// Proof of work, for `signup`.
    #[serde(default)]
    pow_challenge: String,
    #[serde(default)]
    pow_nonce: String,
}

/// Logs in, signs up, or links a key with a signed NIP-98-style event.
pub(super) async fn nostr_auth(
    State(app): State<Shared>,
    ip: ClientIp,
    headers: HeaderMap,
    Json(body): Json<NostrAuth>,
) -> Result<Response, Reject> {
    let pubkey = match nostr::verify_login(&body.event, &app.nostr_auth_url(), &app.challenges, now()) {
        Ok(pubkey) => pubkey,
        Err(message) => {
            inc(&app.wallet.metrics.login_failures);
            return Ok(json_error(StatusCode::UNAUTHORIZED, message));
        }
    };
    let db = &app.wallet.db;
    match body.mode.as_str() {
        "login" => {
            let Some(account) = db.account_by_nostr(&pubkey).await? else {
                return Ok(json_error(
                    StatusCode::NOT_FOUND,
                    "No wallet uses this Nostr key yet. Sign up first.",
                ));
            };
            if account.frozen {
                return Ok(json_error(
                    StatusCode::FORBIDDEN,
                    "This account is frozen. Contact the operator.",
                ));
            }
            let cookie = app.start_session(Some(account.id)).await?;
            Ok(json_redirect(&safe_next(&body.next), Some(cookie)))
        }
        "signup" => {
            if !app.allow("signup", &ip.key(), app.rate.signup_per_ip_per_hour, HOUR) {
                return Ok(json_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many new wallets from your network.",
                ));
            }
            let username = match auth::normalize_username(&body.username, &app.reserved) {
                Ok(username) => username,
                Err(message) => return Ok(json_error(StatusCode::BAD_REQUEST, message)),
            };
            if db.account_by_nostr(&pubkey).await?.is_some() {
                return Ok(json_error(
                    StatusCode::CONFLICT,
                    "This Nostr key already has a wallet. Log in instead.",
                ));
            }
            if db.account_by_username(&username).await?.is_some() {
                return Ok(json_error(StatusCode::CONFLICT, "That username is taken."));
            }
            if let Some(message) = app.refuse_new_account(&body.pow_challenge, &body.pow_nonce).await? {
                return Ok(json_error(StatusCode::FORBIDDEN, message));
            }
            let account = match db.create_account(&username, None, Some(&pubkey)).await {
                Ok(account) => account,
                Err(error) if is_unique_violation(&error) => {
                    return Ok(json_error(StatusCode::CONFLICT, "That username is taken."));
                }
                Err(error) => return Err(error.into()),
            };
            inc(&app.wallet.metrics.signups);
            app.note_new_account(account.id, ip).await;
            tracing::info!(account = account.id, "account created with Nostr");
            let cookie = app.start_session(Some(account.id)).await?;
            Ok(json_redirect(&safe_next(&body.next), Some(cookie)))
        }
        "link" => {
            let Some(session) = UserSession::from_headers(&app, &headers).await? else {
                return Ok(json_error(StatusCode::UNAUTHORIZED, "Log in first."));
            };
            check_csrf(&session.csrf, &body.csrf)?;
            if db
                .account_by_nostr(&pubkey)
                .await?
                .is_some_and(|other| other.id != session.account.id)
            {
                return Ok(json_error(
                    StatusCode::CONFLICT,
                    "This Nostr key belongs to another wallet.",
                ));
            }
            db.set_nostr(session.account.id, Some(&pubkey)).await?;
            Ok(json_redirect("/settings", None))
        }
        _ => Ok(json_error(StatusCode::BAD_REQUEST, "Unknown mode.")),
    }
}

/// Sections of the wallet page that a POST without htmx replaces.
#[derive(Default)]
struct Sections {
    view: WalletView,
    receive: Option<Markup>,
    send: Option<Markup>,
    faucet: Option<Markup>,
}

fn faucet_amount(app: &App) -> i64 {
    i64::try_from(app.wallet.faucet.amount_msat).unwrap_or(i64::MAX)
}

async fn render_wallet(app: &App, session: &UserSession, sections: Sections) -> Result<Markup, Reject> {
    let wallet = &app.wallet;
    let account = &session.account;
    let balance = wallet.db.balance(account.id).await?;
    let history = wallet.db.history(account.id, HISTORY_LEN).await?;
    let address = wallet.address(&account.username);
    let lnurl = lnurl::encode_lnurl(&wallet.lnurlp_url(&account.username));
    let csrf = &session.csrf;
    let receive = sections.receive.unwrap_or_else(|| {
        pages::receive_section(
            csrf,
            &ReceiveState::Form {
                error: None,
                amount: "",
                memo: "",
            },
        )
    });
    let send = sections
        .send
        .unwrap_or_else(|| pages::send_section(csrf, &random_token(), &SendValues::default(), None));
    let faucet_open = sections.faucet.is_some();
    let faucet = wallet.faucet.enabled.then(|| {
        sections
            .faucet
            .unwrap_or_else(|| pages::faucet_section(csrf, &random_token(), faucet_amount(app), None))
    });
    let page = pages::WalletPage {
        view: sections.view,
        address: &address,
        lnurl: &lnurl,
        balance_msat: balance,
        frozen: account.frozen,
        history: &history,
        receive,
        send,
        faucet,
        faucet_open,
    };
    Ok(pages::wallet(&Ctx::member(wallet, session), &page))
}

/// The fragment for htmx (with fresh balance and history when money moved),
/// or the whole page for a plain form post.
async fn respond(
    app: &App,
    session: &UserSession,
    headers: &HeaderMap,
    section: Markup,
    view: WalletView,
    money_moved: bool,
) -> Result<Response, Reject> {
    if !is_htmx(headers) {
        let mut sections = Sections {
            view,
            ..Sections::default()
        };
        match view {
            WalletView::Home => sections.faucet = Some(section),
            WalletView::Send => sections.send = Some(section),
            WalletView::Receive => sections.receive = Some(section),
        }
        return Ok(render_wallet(app, session, sections).await?.into_response());
    }
    if money_moved {
        return Ok(with_updates(app, session, section, view == WalletView::Home)
            .await?
            .into_response());
    }
    Ok(section.into_response())
}

/// Adds out-of-band swaps for the balance and history.
async fn with_updates(
    app: &App,
    session: &UserSession,
    markup: Markup,
    refresh_history: bool,
) -> Result<Markup, Reject> {
    let balance = app.wallet.db.balance(session.account.id).await?;
    let history = app.wallet.db.history(session.account.id, HISTORY_LEN).await?;
    Ok(html! {
        (markup)
        (pages::balance(balance, true))
        @if refresh_history { (pages::history(&history, true)) }
    })
}

pub(super) async fn wallet_page(State(app): State<Shared>, session: UserSession) -> Result<Response, Reject> {
    Ok(render_wallet(&app, &session, Sections::default())
        .await?
        .into_response())
}

pub(super) async fn send_page(State(app): State<Shared>, session: UserSession) -> Result<Response, Reject> {
    Ok(render_wallet(
        &app,
        &session,
        Sections {
            view: WalletView::Send,
            ..Sections::default()
        },
    )
    .await?
    .into_response())
}

pub(super) async fn receive_page(State(app): State<Shared>, session: UserSession) -> Result<Response, Reject> {
    Ok(render_wallet(
        &app,
        &session,
        Sections {
            view: WalletView::Receive,
            ..Sections::default()
        },
    )
    .await?
    .into_response())
}

#[derive(Deserialize)]
pub(super) struct ReceiveForm {
    csrf: String,
    amount_sat: String,
    #[serde(default)]
    memo: String,
}

pub(super) async fn receive(
    State(app): State<Shared>,
    session: UserSession,
    headers: HeaderMap,
    Form(form): Form<ReceiveForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    let account_key = session.account.id.to_string();
    let result = if !app.allow("receive", &account_key, app.rate.receive_per_account_per_minute, MINUTE) {
        Err(WalletError::invalid("Too many invoices. Wait a minute."))
    } else if let Some(amount_msat) = msat_from_sats(&form.amount_sat) {
        app.wallet
            .create_invoice(&session.account, amount_msat, &form.memo, false)
            .await
    } else {
        Err(WalletError::invalid("Enter a whole number of sats."))
    };
    let section = match &result {
        Ok(invoice) => pages::receive_section(&session.csrf, &ReceiveState::Invoice(invoice)),
        Err(error) => pages::receive_section(
            &session.csrf,
            &ReceiveState::Form {
                error: Some(&shown(error)),
                amount: &form.amount_sat,
                memo: &form.memo,
            },
        ),
    };
    respond(&app, &session, &headers, section, WalletView::Receive, false).await
}

pub(super) async fn invoice_status(
    State(app): State<Shared>,
    session: UserSession,
    Path(hash): Path<String>,
) -> Result<Response, Reject> {
    let invoice = app
        .wallet
        .db
        .invoice(&hash)
        .await?
        .filter(|invoice| invoice.account_id == session.account.id)
        .ok_or(Reject::NotFound)?;
    let status = pages::invoice_status(&invoice);
    if invoice.state == "settled" {
        return Ok(with_updates(&app, &session, status, false).await?.into_response());
    }
    Ok(status.into_response())
}

#[derive(Deserialize)]
pub(super) struct SendForm {
    csrf: String,
    key: String,
    destination: String,
    #[serde(default)]
    amount_sat: String,
    #[serde(default)]
    comment: String,
    #[serde(default)]
    max_fee_msat: Option<u64>,
    #[serde(skip)]
    fixed_amount: bool,
}

impl SendForm {
    async fn load_invoice(&mut self, app: &App) -> Result<(), WalletError> {
        if let lnurl::Destination::Invoice(bolt11) = lnurl::parse_destination(&self.destination)? {
            let decoded = app.wallet.decode(&bolt11).await?;
            self.destination = bolt11;
            if let Some(amount) = super::launch::invoice_amount(decoded.num_msat) {
                self.amount_sat = amount;
                self.fixed_amount = true;
            }
        }
        Ok(())
    }

    fn values(&self) -> SendValues<'_> {
        SendValues {
            destination: &self.destination,
            amount: &self.amount_sat,
            fixed_amount: self.fixed_amount,
            comment: &self.comment,
            ..SendValues::default()
        }
    }
}

#[derive(Deserialize)]
pub(super) struct AmountForm {
    csrf: String,
    destination: String,
}

/// Read-only invoice decoding for pasting and scanning in the Send form.
pub(super) async fn send_amount(
    State(app): State<Shared>,
    session: UserSession,
    Form(form): Form<AmountForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    if !app.allow(
        "decode",
        &session.account.id.to_string(),
        app.rate.send_per_account_per_minute,
        MINUTE,
    ) {
        return Ok(json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many payment checks. Wait a minute.",
        ));
    }
    let result = async {
        match lnurl::parse_destination(&form.destination)? {
            lnurl::Destination::Invoice(bolt11) => {
                let decoded = app.wallet.decode(&bolt11).await?;
                Ok::<_, WalletError>(super::launch::invoice_amount(decoded.num_msat))
            }
            _ => Ok(None),
        }
    }
    .await;
    Ok(match result {
        Ok(amount) => Json(json!({ "amount_sat": amount })).into_response(),
        Err(error) => json_error(StatusCode::BAD_REQUEST, &shown(&error)),
    })
}

pub(super) async fn review_send(
    State(app): State<Shared>,
    session: UserSession,
    headers: HeaderMap,
    Form(mut form): Form<SendForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    let result = if !app.allow(
        "review",
        &session.account.id.to_string(),
        app.rate.send_per_account_per_minute,
        MINUTE,
    ) {
        Err(WalletError::invalid("Too many payment checks. Wait a minute."))
    } else if let Err(error) = form.load_invoice(&app).await {
        Err(error)
    } else if !form.fixed_amount && !form.amount_sat.trim().is_empty() && msat_from_sats(&form.amount_sat).is_none() {
        Err(WalletError::invalid("Enter a whole number of sats."))
    } else {
        app.wallet
            .preview(
                &session.account,
                &PayRequest {
                    request_key: form.key.clone(),
                    destination: form.destination.clone(),
                    amount_msat: msat_from_sats(&form.amount_sat),
                    comment: form.comment.clone(),
                    max_fee_msat: None,
                },
            )
            .await
    };
    let values = form.values();
    let section = match result {
        Ok(preview) => pages::send_review(&session.csrf, &form.key, &values, &preview, &app.wallet.network_name),
        Err(error) => pages::send_section(&session.csrf, &form.key, &values, Some(Err(&shown(&error)))),
    };
    respond(&app, &session, &headers, section, WalletView::Send, false).await
}

pub(super) async fn edit_send(
    State(app): State<Shared>,
    session: UserSession,
    headers: HeaderMap,
    Form(mut form): Form<SendForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    let error = if app.allow(
        "decode",
        &session.account.id.to_string(),
        app.rate.send_per_account_per_minute,
        MINUTE,
    ) {
        form.load_invoice(&app).await.err().map(|error| shown(&error))
    } else {
        Some("Too many payment checks. Wait a minute.".to_owned())
    };
    let section = pages::send_section(&session.csrf, &form.key, &form.values(), error.as_deref().map(Err));
    respond(&app, &session, &headers, section, WalletView::Send, false).await
}

pub(super) async fn send(
    State(app): State<Shared>,
    session: UserSession,
    headers: HeaderMap,
    Form(mut form): Form<SendForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    let amount = form.amount_sat.trim();
    let account_key = session.account.id.to_string();
    let result = if !app.allow("send", &account_key, app.rate.send_per_account_per_minute, MINUTE) {
        Err(WalletError::invalid("Too many payments. Wait a minute."))
    } else if !amount.is_empty() && msat_from_sats(amount).is_none() {
        Err(WalletError::invalid("Enter a whole number of sats."))
    } else {
        let request = PayRequest {
            request_key: form.key.clone(),
            destination: form.destination.clone(),
            amount_msat: msat_from_sats(amount),
            comment: form.comment.clone(),
            max_fee_msat: form.max_fee_msat,
        };
        app.wallet.pay(&session.account, request).await
    };
    // Every answer gets a fresh request key; errors keep what was typed.
    let section = match &result {
        Ok(payment) => pages::send_section(
            &session.csrf,
            &random_token(),
            &SendValues::default(),
            Some(Ok(payment)),
        ),
        Err(error) => {
            if app.allow("decode", &account_key, app.rate.send_per_account_per_minute, MINUTE) {
                let _ = form.load_invoice(&app).await;
            }
            let values = form.values();
            pages::send_section(&session.csrf, &random_token(), &values, Some(Err(&shown(error))))
        }
    };
    let moved = result.as_ref().is_ok_and(|payment| payment.status != "pending");
    respond(&app, &session, &headers, section, WalletView::Send, moved).await
}

pub(super) async fn payment_status(
    State(app): State<Shared>,
    session: UserSession,
    Path(id): Path<i64>,
) -> Result<Response, Reject> {
    let payment = app
        .wallet
        .db
        .payment(session.account.id, id)
        .await?
        .ok_or(Reject::NotFound)?;
    let status = pages::payment_status(&payment);
    if payment.status == "pending" {
        return Ok(status.into_response());
    }
    Ok(with_updates(&app, &session, status, false).await?.into_response())
}

#[derive(Deserialize)]
pub(super) struct FaucetForm {
    csrf: String,
    key: String,
}

pub(super) async fn faucet(
    State(app): State<Shared>,
    session: UserSession,
    ip: ClientIp,
    headers: HeaderMap,
    Form(form): Form<FaucetForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    let result = app.wallet.faucet(&session.account, &form.key, &ip.key()).await;
    let amount = faucet_amount(&app);
    let section = match &result {
        Ok(payment) => pages::faucet_section(&session.csrf, &random_token(), amount, Some(Ok(payment))),
        Err(error) => pages::faucet_section(&session.csrf, &random_token(), amount, Some(Err(&shown(error)))),
    };
    respond(&app, &session, &headers, section, WalletView::Home, result.is_ok()).await
}

fn render_settings(app: &App, session: &UserSession, message: Option<Result<&str, &str>>) -> Markup {
    let account = &session.account;
    let address = app.wallet.address(&account.username);
    let npub = account.nostr_pubkey.as_deref().map(nostr::npub);
    let page = pages::SettingsPage {
        address: &address,
        created_at: account.created_at,
        npub: npub.as_deref(),
        has_password: account.password_hash.is_some(),
        csrf: &session.csrf,
        message,
    };
    pages::settings(&Ctx::member(&app.wallet, session), &page)
}

pub(super) async fn settings_page(State(app): State<Shared>, session: UserSession) -> Response {
    render_settings(&app, &session, None).into_response()
}

#[derive(Deserialize)]
pub(super) struct PasswordForm {
    csrf: String,
    #[serde(default)]
    current: String,
    new: String,
    confirm: String,
}

pub(super) async fn change_password(
    State(app): State<Shared>,
    mut session: UserSession,
    Form(form): Form<PasswordForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    let refuse =
        |session: &UserSession, message: &str| render_settings(&app, session, Some(Err(message))).into_response();
    if let Err(message) = auth::check_new_password(&form.new, &form.confirm) {
        return Ok(refuse(&session, message));
    }
    let Ok(permit) = app.passwords.admit() else {
        return Ok(busy(|message| refuse(&session, message)));
    };
    if !app.allow(
        "login-account",
        &session.account.username,
        app.rate.login_per_account_per_hour,
        HOUR,
    ) {
        return Ok(refuse(&session, "Too many attempts. Try again later."));
    }
    let current_hash = session.account.password_hash.clone();
    let current = form.current.clone();
    let new = form.new.clone();
    // One job checks the current password and hashes the new one, so the
    // change cannot be refused halfway for want of a second worker.
    let hash = permit
        .run(move || {
            let verified = current_hash.is_none_or(|hash| auth::verify_password(&hash, &current));
            verified.then(|| auth::hash_password(&new))
        })
        .await?
        .transpose()
        .map_err(|_| Reject::Server)?;
    let Some(hash) = hash else {
        return Ok(refuse(&session, "The current password is wrong."));
    };
    app.wallet.db.set_password(session.account.id, &hash).await?;
    app.wallet
        .db
        .delete_other_sessions(session.account.id, &session.token_hash)
        .await?;
    session.account.password_hash = Some(hash);
    Ok(render_settings(
        &app,
        &session,
        Some(Ok("Password saved. Other sessions were signed out.")),
    )
    .into_response())
}

pub(super) async fn unlink_nostr(
    State(app): State<Shared>,
    session: UserSession,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Reject> {
    check_csrf(&session.csrf, &form.csrf)?;
    if session.account.password_hash.is_none() {
        return Ok(
            render_settings(&app, &session, Some(Err("Set a password before unlinking Nostr."))).into_response(),
        );
    }
    app.wallet.db.set_nostr(session.account.id, None).await?;
    Ok(redirect("/settings"))
}
