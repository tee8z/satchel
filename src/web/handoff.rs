//! Handoff sign-in from another app (`POST /auth/nostr/handoff`, see
//! docs/integrating.md). The app posts an event signed with the user's Nostr
//! key in a top-level form. For a key with a wallet, Satchel then redirects once
//! to `/auth/nostr/handoff/continue`: browsers withhold this site's `SameSite`
//! session cookie from another site's form post but send it on that redirect,
//! so the next step knows who is signed in here.

use std::time::Duration;

use axum::Form;
use axum::extract::State;
use axum::http::header::{ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use maud::{Markup, html};
use serde::Deserialize;

use super::pages::{self, Ctx, HandoffSignup};
use super::{App, ClientIp, Reject, Shared, UserSession, cookie, redirect, redirect_with_cookie};
use crate::auth;
use crate::db::Account;
use crate::handoff::{PENDING_TTL_SECS, Pending, Purpose, safe_next, short_npub};
use crate::metrics::inc;
use crate::nostr::{self, Event, MAX_CLOCK_SKEW_SECS};
use crate::util::now;

/// Where apps post handoffs: the one form endpoint other sites may post to.
pub(super) const PATH: &str = "/auth/nostr/handoff";
const CONTINUE: &str = "/auth/nostr/handoff/continue";
const MINUTE: Duration = Duration::from_secs(60);
/// A signed event is a few hundred bytes; anything far larger is refused unread.
const MAX_EVENT_LEN: usize = 16 * 1024;
const FROZEN: &str = "This wallet is frozen. Contact the operator.";

impl App {
    /// The URL handoff events must name in their `u` tag.
    pub(super) fn handoff_url(&self) -> String {
        format!("{}auth/nostr/handoff", self.wallet.origin)
    }

    /// The request's `Origin` when it is listed in `server.handoff_origins`.
    pub(super) fn trusted_origin<'h>(&self, headers: &'h HeaderMap) -> Option<&'h str> {
        headers
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok())
            .filter(|origin| self.handoff_origins.iter().any(|trusted| trusted.as_str() == *origin))
    }

    fn handoff_cookie(&self) -> String {
        self.cookie_name("handoff")
    }
}

/// Why a handoff stopped, with a way back.
fn refused(ctx: &Ctx<'_>, status: StatusCode, message: &str) -> Response {
    let page = pages::layout(
        ctx,
        "Could not open your wallet",
        html! {
            h1 { "Could not open your wallet" }
            p.error role="alert" { (message) }
            p { "Go back to the app and open your wallet again, or " a href="/login" { "log in here" } "." }
        },
    );
    (status, page).into_response()
}

/// The answer when a handoff token is unknown, used, or too old.
pub(super) fn expired(ctx: &Ctx<'_>) -> Response {
    refused(
        ctx,
        StatusCode::BAD_REQUEST,
        "This sign-in expired or was already used.",
    )
}

fn without_handoff_cookie(app: &App, mut response: Response) -> Response {
    response
        .headers_mut()
        .append(SET_COOKIE, app.set_cookie(&app.handoff_cookie(), "", 0));
    response
}

#[derive(Deserialize)]
pub(super) struct HandoffForm {
    #[serde(default)]
    event: String,
    #[serde(default)]
    next: String,
}

/// Checks a handoff, then continues to sign-in (a key with a wallet) or to the
/// create-wallet page (a key without one).
pub(super) async fn start(
    State(app): State<Shared>,
    ip: ClientIp,
    headers: HeaderMap,
    Form(form): Form<HandoffForm>,
) -> Result<Response, Reject> {
    let ctx = Ctx::visitor(&app.wallet);
    if !app.allow("handoff", &ip.key(), app.rate.login_per_ip_per_minute, MINUTE) {
        return Ok(refused(
            &ctx,
            StatusCode::TOO_MANY_REQUESTS,
            "Too many sign-in attempts from your network. Wait a minute.",
        ));
    }
    let event = Some(form.event.as_str())
        .filter(|event| event.len() <= MAX_EVENT_LEN)
        .and_then(|event| serde_json::from_str::<Event>(event).ok());
    let Some(event) = event else {
        return Ok(refused(
            &ctx,
            StatusCode::BAD_REQUEST,
            "The sign-in request is malformed.",
        ));
    };
    let pubkey = match nostr::verify_http_auth(&event, &app.handoff_url(), "POST", MAX_CLOCK_SKEW_SECS, now()) {
        Ok(pubkey) => pubkey,
        Err(message) => {
            inc(&app.wallet.metrics.login_failures);
            return Ok(refused(&ctx, StatusCode::UNAUTHORIZED, message));
        }
    };
    if !app.wallet.db.use_handoff_event(&event.id).await? {
        inc(&app.wallet.metrics.login_failures);
        return Ok(refused(
            &ctx,
            StatusCode::UNAUTHORIZED,
            "This sign-in request was already used.",
        ));
    }
    let next = safe_next(&form.next);
    let db = &app.wallet.db;
    match db.account_by_nostr(&pubkey).await? {
        Some(account) if account.frozen => Ok(refused(&ctx, StatusCode::FORBIDDEN, FROZEN)),
        Some(_) => {
            let trusted = app.trusted_origin(&headers).is_some();
            let token = db
                .create_pending_handoff(Purpose::SignIn, &pubkey, &next, trusted)
                .await?;
            let cookie = app.set_cookie(&app.handoff_cookie(), &token, PENDING_TTL_SECS);
            Ok(redirect_with_cookie(CONTINUE, cookie))
        }
        None => {
            // No wallet uses the key yet: offer one through the regular sign-up form.
            let token = db
                .create_pending_handoff(Purpose::SignUp, &pubkey, &next, false)
                .await?;
            let username = suggested_username(&app, &event).await?;
            let npub = short_npub(&pubkey);
            let page = HandoffSignup {
                token: &token,
                npub: &npub,
            };
            Ok(pages::signup_with(&ctx, &app.wallet.domain, None, &username, Some(&page)).into_response())
        }
    }
}

/// The event's `name` tag as a username, when it is valid and free.
async fn suggested_username(app: &App, event: &Event) -> Result<String, Reject> {
    let Some(name) = nostr::tag(event, "name").and_then(|name| auth::normalize_username(name, &app.reserved).ok())
    else {
        return Ok(String::new());
    };
    let free = app.wallet.db.account_by_username(&name).await?.is_none();
    Ok(if free { name } else { String::new() })
}

/// The wallet a pending sign-in opens, or the page saying why not.
async fn signin_account(app: &App, pending: &Pending, ctx: &Ctx<'_>) -> Result<Result<Account, Response>, Reject> {
    Ok(match app.wallet.db.account_by_nostr(&pending.nostr_pubkey).await? {
        Some(account) if account.frozen => Err(refused(ctx, StatusCode::FORBIDDEN, FROZEN)),
        Some(account) => Ok(account),
        None => Err(expired(ctx)),
    })
}

/// The redirect after a handoff for a key with a wallet, where this site's
/// cookies are visible. A trusted app's handoff signs in straight away when
/// the browser is signed out or already in that wallet; anything else asks.
pub(super) async fn resume(State(app): State<Shared>, headers: HeaderMap) -> Result<Response, Reject> {
    let visitor = Ctx::visitor(&app.wallet);
    let Some(token) = cookie(&headers, &app.handoff_cookie()) else {
        return Ok(expired(&visitor));
    };
    let Some(pending) = app.wallet.db.pending_handoff(&token, Purpose::SignIn).await? else {
        return Ok(without_handoff_cookie(&app, expired(&visitor)));
    };
    let account = match signin_account(&app, &pending, &visitor).await? {
        Ok(account) => account,
        Err(response) => return Ok(without_handoff_cookie(&app, response)),
    };
    let session = UserSession::from_headers(&app, &headers).await?;
    let other_wallet = session.as_ref().is_some_and(|session| session.account.id != account.id);
    if pending.trusted && !other_wallet {
        if app
            .wallet
            .db
            .take_pending_handoff(&token, Purpose::SignIn)
            .await?
            .is_none()
        {
            return Ok(without_handoff_cookie(&app, expired(&visitor)));
        }
        return finish(&app, &account, session.as_ref(), &pending.next).await;
    }
    let ctx = match &session {
        Some(session) => Ctx::member(&app.wallet, session),
        None => visitor,
    };
    Ok(confirmation(&ctx, &app, &account, &token, session.as_ref()).into_response())
}

/// "Continue as alice?": shown for apps not in `server.handoff_origins` and
/// when another wallet is signed in here, so no site can sign a browser in
/// to a wallet of its choosing without a click.
fn confirmation(ctx: &Ctx<'_>, app: &App, account: &Account, token: &str, session: Option<&UserSession>) -> Markup {
    let npub = account.nostr_pubkey.as_deref().map(short_npub).unwrap_or_default();
    pages::layout(
        ctx,
        "Continue to your wallet",
        html! {
            h1 { "Continue as " (account.username) "?" }
            section.card {
                p {
                    "An app asked to open the wallet " strong { (app.wallet.address(&account.username)) }
                    " with the Nostr key " code { (npub) } "."
                }
                @if let Some(current) = session.filter(|current| current.account.id != account.id) {
                    p {
                        "You are signed in here as " strong { (current.account.username) }
                        ". Continuing signs you out of that wallet."
                    }
                }
                form method="post" action="/auth/nostr/handoff/confirm" {
                    input type="hidden" name="token" value=(token);
                    button type="submit" { "Continue as " (account.username) }
                }
                p.muted { "Not expecting this? Close this page; nothing happens unless you continue." }
            }
        },
    )
}

#[derive(Deserialize)]
pub(super) struct ConfirmForm {
    #[serde(default)]
    token: String,
}

/// The confirmation button: a same-origin post, so no other site can press it.
pub(super) async fn confirm(
    State(app): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<ConfirmForm>,
) -> Result<Response, Reject> {
    let visitor = Ctx::visitor(&app.wallet);
    let Some(pending) = app.wallet.db.take_pending_handoff(&form.token, Purpose::SignIn).await? else {
        return Ok(without_handoff_cookie(&app, expired(&visitor)));
    };
    let account = match signin_account(&app, &pending, &visitor).await? {
        Ok(account) => account,
        Err(response) => return Ok(without_handoff_cookie(&app, response)),
    };
    let session = UserSession::from_headers(&app, &headers).await?;
    finish(&app, &account, session.as_ref(), &pending.next).await
}

/// Signs the browser in to `account`, replacing another wallet's session,
/// and opens `next`.
async fn finish(app: &App, account: &Account, session: Option<&UserSession>, next: &str) -> Result<Response, Reject> {
    let session_cookie = match session {
        Some(current) if current.account.id == account.id => None,
        current => {
            if let Some(current) = current {
                app.wallet.db.delete_session(&current.token_hash).await?;
            }
            Some(app.start_session(Some(account.id)).await?)
        }
    };
    let mut response = redirect(next);
    if let Some(cookie) = session_cookie {
        response.headers_mut().append(SET_COOKIE, cookie);
    }
    Ok(without_handoff_cookie(app, response))
}
