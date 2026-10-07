//! A small JSON API for other apps. Requests are signed with the user's Nostr
//! key (NIP-98: `Authorization: Nostr <base64 event>`); browsers may call it
//! from the origins in `server.handoff_origins`.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE,
    AUTHORIZATION, VARY, WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;

use super::{App, ClientIp, Shared};
use crate::nostr::{self, Event};
use crate::util::now;

const MINUTE: Duration = Duration::from_secs(60);
/// NIP-98 asks for a tight window on HTTP auth events.
const MAX_SKEW_SECS: u64 = 60;
/// A signed event is a few hundred bytes; anything far larger is refused unread.
const MAX_AUTHORIZATION_LEN: usize = 16 * 1024;

/// CORS for an origin in `server.handoff_origins`: no credentials, only the
/// `Authorization` header. Other origins get no CORS headers at all.
fn allow_trusted_origin(app: &App, request: &HeaderMap, response: &mut Response, preflight: bool) {
    let headers = response.headers_mut();
    headers.insert(VARY, HeaderValue::from_static("Origin"));
    let Some(origin) = app
        .trusted_origin(request)
        .and_then(|origin| HeaderValue::from_str(origin).ok())
    else {
        return;
    };
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    if preflight {
        headers.insert(ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET"));
        headers.insert(ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Authorization"));
        headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    }
}

/// The CORS preflight for `GET /api/v1/address`.
pub(super) async fn preflight(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    allow_trusted_origin(&app, &headers, &mut response, true);
    response
}

/// `GET /api/v1/address`: the Lightning Address of the wallet that uses the
/// signing key. Never creates a wallet.
pub(super) async fn address(State(app): State<Shared>, ip: ClientIp, headers: HeaderMap, uri: Uri) -> Response {
    let mut response = lookup(&app, ip, &headers, &uri).await;
    allow_trusted_origin(&app, &headers, &mut response, false);
    response
}

async fn lookup(app: &App, ip: ClientIp, headers: &HeaderMap, uri: &Uri) -> Response {
    if !app.allow("api-ip", &ip.key(), app.rate.lnurl_per_ip_per_minute, MINUTE) {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many requests. Wait a minute.",
        );
    }
    // NIP-98 signs the full request URL, as this site's public origin serves it.
    let path = uri.path_and_query().map_or(uri.path(), |path| path.as_str());
    let url = format!("{}{}", app.wallet.origin.as_str().trim_end_matches('/'), path);
    let pubkey = match signer(headers, &url) {
        Ok(pubkey) => pubkey,
        Err(message) => {
            let mut response = error(StatusCode::UNAUTHORIZED, "unauthorized", message);
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Nostr"));
            return response;
        }
    };
    match app.wallet.db.account_by_nostr(&pubkey).await {
        Ok(Some(account)) if !account.frozen => Json(json!({
            "lightning_address": app.wallet.address(&account.username),
            "username": account.username,
        }))
        .into_response(),
        Ok(_) => error(StatusCode::NOT_FOUND, "no_account", "No wallet uses this Nostr key."),
        Err(database) => {
            tracing::error!(error = %database, "cannot look up a wallet by Nostr key");
            error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "Try again shortly.")
        }
    }
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

/// The public key that signed the request's `Authorization: Nostr ...` event.
fn signer(headers: &HeaderMap, url: &str) -> Result<String, &'static str> {
    let value = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() <= MAX_AUTHORIZATION_LEN)
        .ok_or("Send an Authorization: Nostr header.")?;
    let encoded = value
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("nostr"))
        .map(|(_, encoded)| encoded.trim())
        .ok_or("Send an Authorization: Nostr header.")?;
    let event = STANDARD
        .decode(encoded)
        .ok()
        .and_then(|json| serde_json::from_slice::<Event>(&json).ok())
        .ok_or("The Authorization header does not hold a base64 Nostr event.")?;
    nostr::verify_http_auth(&event, url, "GET", MAX_SKEW_SECS, now())
}
