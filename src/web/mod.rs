//! HTTP: server-rendered pages (maud + htmx), the LNURL endpoints, and the
//! security plumbing shared by all of them.

mod admin;
mod api;
pub(crate) mod assets;
mod handoff;
mod launch;
mod lnurlp;
mod pages;
mod protect;
mod user;

pub(crate) use protect::maintain_forever as maintain_protections;

#[cfg(test)]
pub(crate) use lnurlp::parse_callback_query as lnurlp_parse_callback_query;

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, HOST, LOCATION, ORIGIN,
    REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::request::Parts;
use axum::http::{Extensions, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use url::Url;

use crate::auth;
use crate::blocklist::Blocklist;
use crate::config::{Config, RateLimits, public_origin};
use crate::db::Account;
use crate::nostr::Challenges;
use crate::pow::Pow;
use crate::ratelimit::{RateLimiter, client_key};
use crate::util::constant_time_eq;
use crate::wallet::Wallet;

/// Scripts, workers, styles, manifests, and connections from this origin only; nothing inline, no eval.
const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'none'; script-src 'self'; worker-src 'self'; \
     style-src 'self'; img-src 'self' data:; manifest-src 'self'; connect-src 'self'; form-action 'self'; \
     base-uri 'none'; frame-ancestors 'none'";

const OPERATOR_SESSION_SECS: i64 = 12 * 3600;

pub(crate) struct App {
    pub(crate) wallet: Arc<Wallet>,
    pub(crate) limiter: RateLimiter,
    pub(crate) rate: RateLimits,
    pub(crate) challenges: Challenges,
    /// Proof of work for new accounts.
    pub(crate) pow: Pow,
    /// Operator blocks, loaded at startup and after each change.
    pub(crate) blocks: Blocklist,
    /// The operator's argon2 hash; no hash, no operator pages.
    pub(crate) admin_hash: Option<String>,
    /// A separate origin for the operator pages; when set, `/admin` answers only there.
    pub(crate) operator_origin: Option<Url>,
    pub(crate) client_ip_header: Option<HeaderName>,
    /// `server.handoff_origins`, serialized as browsers send `Origin`.
    pub(crate) handoff_origins: Vec<String>,
    pub(crate) reserved: HashSet<String>,
    pub(crate) session_ttl_secs: i64,
    pub(crate) secure_cookies: bool,
}

pub(crate) type Shared = Arc<App>;

impl App {
    pub(crate) fn new(wallet: Arc<Wallet>, config: &Config, admin_hash: Option<String>) -> Result<Self> {
        let client_ip_header = config
            .server
            .client_ip_header
            .as_deref()
            .map(|name| HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes()))
            .transpose()
            .context("invalid server.client_ip_header")?;
        let operator_origin = config
            .server
            .operator_url
            .as_deref()
            .map(public_origin)
            .transpose()
            .context("invalid server.operator_url")?;
        let handoff_origins: Vec<String> = config
            .server
            .handoff_origins
            .iter()
            .map(|origin| public_origin(origin).map(|url| url.origin().ascii_serialization()))
            .collect::<Result<_>>()
            .context("invalid server.handoff_origins")?;
        Ok(Self {
            secure_cookies: wallet.origin.scheme() == "https",
            wallet,
            limiter: RateLimiter::default(),
            rate: config.rate_limits.clone(),
            challenges: Challenges::default(),
            pow: Pow::new(&config.pow),
            blocks: Blocklist::default(),
            admin_hash,
            operator_origin,
            client_ip_header,
            handoff_origins,
            reserved: config
                .server
                .reserved_usernames
                .iter()
                .map(|name| name.trim().to_ascii_lowercase())
                .collect(),
            session_ttl_secs: i64::from(config.server.session_days) * 86_400,
        })
    }

    /// Whether a request may reach the operator pages: any host, unless
    /// `server.operator_url` names the only one.
    pub(crate) fn on_operator_host(&self, parts: &Parts) -> bool {
        let Some(origin) = &self.operator_origin else {
            return true;
        };
        let host = parts
            .headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .or_else(|| parts.uri.authority().map(|authority| authority.as_str()));
        host.is_some_and(|host| host.eq_ignore_ascii_case(&authority(origin)))
    }

    /// Counts an attempt; false (and a metric for the scope) when over the limit.
    pub(crate) fn allow(&self, scope: &str, key: &str, limit: u32, window: Duration) -> bool {
        let allowed = self.limiter.allow(&format!("{scope}:{key}"), limit, window);
        if !allowed {
            self.wallet.metrics.rate_limited(scope);
        }
        allowed
    }

    fn cookie_name(&self, base: &str) -> String {
        // `__Host-` cookies must be Secure, host-only, and for the whole site.
        if self.secure_cookies {
            format!("__Host-{base}")
        } else {
            base.to_owned()
        }
    }

    pub(crate) fn session_cookie(&self) -> String {
        self.cookie_name("session")
    }

    pub(crate) fn admin_cookie(&self) -> String {
        self.cookie_name("operator")
    }

    pub(crate) fn set_cookie(&self, name: &str, value: &str, max_age_secs: i64) -> HeaderValue {
        let secure = if self.secure_cookies { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure}"
        ))
        .expect("cookie values are base64url")
    }

    /// Stores a new session and returns its cookie. `None` is the operator.
    pub(crate) async fn start_session(&self, account_id: Option<i64>) -> Result<HeaderValue, Reject> {
        let session = auth::new_session();
        let (name, ttl) = match account_id {
            Some(_) => (self.session_cookie(), self.session_ttl_secs),
            None => (self.admin_cookie(), OPERATOR_SESSION_SECS),
        };
        self.wallet
            .db
            .create_session(&session.token_hash, account_id, &session.csrf, ttl)
            .await?;
        Ok(self.set_cookie(&name, &session.cookie, ttl))
    }

    pub(crate) fn nostr_auth_url(&self) -> String {
        format!("{}auth/nostr", self.wallet.origin)
    }
}

pub(crate) fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(user::home))
        .route("/signup", get(user::signup_page).post(user::signup))
        .route("/login", get(user::login_page).post(user::login))
        .route("/logout", post(user::logout))
        .route("/auth/pow", post(protect::pow_challenge))
        .route("/auth/nostr/challenge", post(user::nostr_challenge))
        .route("/auth/nostr", post(user::nostr_auth))
        .route(handoff::PATH, post(handoff::start))
        .route("/auth/nostr/handoff/continue", get(handoff::resume))
        .route("/auth/nostr/handoff/confirm", post(handoff::confirm))
        .route("/launch/lightning/{target}", get(launch::lightning))
        .route("/api/v1/address", get(api::address).options(api::preflight))
        .route("/wallet", get(user::wallet_page))
        .route("/wallet/receive", get(user::receive_page).post(user::receive))
        .route("/wallet/invoice/{hash}", get(user::invoice_status))
        .route("/wallet/send", get(user::send_page).post(user::send))
        .route("/wallet/send/review", post(user::review_send))
        .route("/wallet/send/edit", post(user::edit_send))
        .route("/wallet/send/amount", post(user::send_amount))
        .route("/wallet/payment/{id}", get(user::payment_status))
        .route("/wallet/faucet", post(user::faucet))
        .route("/settings", get(user::settings_page))
        .route("/settings/password", post(user::change_password))
        .route("/settings/nostr/unlink", post(user::unlink_nostr))
        .route("/admin", get(admin::dashboard))
        .route("/admin/login", get(admin::login_page).post(admin::login))
        .route("/admin/logout", post(admin::logout))
        .route("/admin/accounts/{id}/freeze", post(admin::freeze))
        .route("/admin/accounts/{id}/credit", post(admin::credit))
        .route("/admin/blocks", post(protect::add_block))
        .route("/admin/blocks/{id}/remove", post(protect::remove_block))
        .route("/.well-known/lnurlp/{username}", get(lnurlp::params))
        .route("/lnurlp/{username}/callback", get(lnurlp::callback))
        .route("/assets/{file}", get(assets::serve))
        .route("/healthz", get(healthz))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// `/metrics` and `/healthz`, for a private listener.
pub(crate) fn metrics_router(app: Shared) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .with_state(app)
}

/// 200 while the database answers; the LND invoice stream is reported but does not fail the check.
async fn healthz(State(app): State<Shared>) -> Response {
    let database = app.wallet.db.ping().await;
    let invoice_stream = app.wallet.metrics.invoice_stream_up.load(Ordering::Relaxed);
    let (status, state) = if database {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "error")
    };
    let body = serde_json::json!({
        "status": state,
        "database": database,
        "invoice_stream": invoice_stream,
        "invoice_stream_state": app.wallet.metrics.invoice_stream_state(),
        "reconciliation_last_success": app.wallet.metrics.reconciliation_last_success.load(Ordering::Relaxed),
    });
    (status, Json(body)).into_response()
}

async fn metrics(State(app): State<Shared>) -> Response {
    match app.wallet.db.totals().await {
        Ok(totals) => (
            [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
            app.wallet.metrics.render(&totals),
        )
            .into_response(),
        Err(error) => {
            tracing::error!(%error, "cannot read totals for metrics");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn not_found(State(app): State<Shared>) -> Response {
    let ctx = pages::Ctx::visitor(&app.wallet);
    (StatusCode::NOT_FOUND, pages::not_found(&ctx)).into_response()
}

/// Whether a state-changing request comes from a page of this site.
fn same_origin(headers: &HeaderMap, origin: &Url) -> bool {
    let expected = origin.origin().ascii_serialization();
    match headers.get(ORIGIN).and_then(|value| value.to_str().ok()) {
        Some(value) => value == expected,
        None => headers.get("sec-fetch-site").and_then(|value| value.to_str().ok()) == Some("same-origin"),
    }
}

/// `host[:port]` as browsers send it in `Host`, without the scheme's default port.
fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// Refuses blocked networks and cross-site writes, and adds security headers
/// to every response. Operator pages on their own origin may also post from that origin.
async fn guard(State(app): State<Shared>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let lnurl = path.starts_with("/.well-known/lnurlp/") || path.starts_with("/lnurlp/");
    let admin = path == "/admin" || path.starts_with("/admin/");
    if let Some(response) = protect::refuse_blocked(&app, &request, lnurl, admin) {
        return secure(response, lnurl);
    }
    let writes = !matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    // A handoff's credential is the signed event it carries, so other sites may post it.
    let signed_handoff = path == handoff::PATH;
    if writes && !signed_handoff {
        let headers = request.headers();
        let from_operator = admin
            && app
                .operator_origin
                .as_ref()
                .is_some_and(|origin| same_origin(headers, origin));
        if !from_operator && !same_origin(headers, &app.wallet.origin) {
            return (StatusCode::FORBIDDEN, "Cross-site request refused.").into_response();
        }
    }
    secure(next.run(request).await, lnurl)
}

fn secure(mut response: Response, lnurl: bool) -> Response {
    let headers = response.headers_mut();
    headers
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // The Send form's QR scanner may use the camera; nothing else may.
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(self), microphone=()"),
    );
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    if lnurl {
        // Browser wallets call LNURL endpoints from other origins.
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    }
    response
}

/// Why a request was turned away.
#[derive(Debug)]
pub(crate) enum Reject {
    /// Not signed in: go to the login page (htmx follows `HX-Redirect`).
    Login {
        htmx: bool,
    },
    Operator,
    Forbidden,
    NotFound,
    Server,
}

impl IntoResponse for Reject {
    fn into_response(self) -> Response {
        match self {
            Self::Login { htmx: true } => (
                StatusCode::UNAUTHORIZED,
                [(HeaderName::from_static("hx-redirect"), "/login")],
                "Please log in.",
            )
                .into_response(),
            Self::Login { htmx: false } => redirect("/login"),
            Self::Operator => redirect("/admin/login"),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "This form expired. Reload the page and try again.",
            )
                .into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "Not found.").into_response(),
            Self::Server => (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.").into_response(),
        }
    }
}

impl From<sqlx::Error> for Reject {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database error");
        Self::Server
    }
}

/// A 303 so a form POST is followed by a GET.
pub(crate) fn redirect(to: &str) -> Response {
    let location = HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/"));
    (StatusCode::SEE_OTHER, [(LOCATION, location)]).into_response()
}

/// A redirect that also sets a cookie.
pub(crate) fn redirect_with_cookie(to: &str, cookie: HeaderValue) -> Response {
    let mut response = redirect(to);
    response.headers_mut().insert(SET_COOKIE, cookie);
    response
}

pub(crate) fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request") && !headers.contains_key("hx-history-restore-request")
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
}

pub(crate) fn check_csrf(expected: &str, given: &str) -> Result<(), Reject> {
    if constant_time_eq(expected.as_bytes(), given.as_bytes()) {
        Ok(())
    } else {
        Err(Reject::Forbidden)
    }
}

/// The client's address: the trusted proxy's header when configured, else the peer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientIp {
    pub(crate) addr: IpAddr,
    /// IPv6 clients count as one address per prefix of this length.
    ipv6_prefix: u8,
}

impl ClientIp {
    /// What per-address limits count under: an IPv4 address or an IPv6 prefix.
    pub(crate) fn key(self) -> String {
        client_key(self.addr, self.ipv6_prefix)
    }

    pub(crate) fn from_request(app: &App, headers: &HeaderMap, extensions: &Extensions) -> Self {
        let forwarded = app
            .client_ip_header
            .as_ref()
            .and_then(|name| headers.get(name))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit(',').next())
            .and_then(|value| value.trim().parse::<IpAddr>().ok());
        let peer = extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip());
        Self {
            addr: forwarded.or(peer).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            ipv6_prefix: app.rate.ipv6_prefix_len,
        }
    }
}

impl FromRequestParts<Shared> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        Ok(Self::from_request(app, &parts.headers, &parts.extensions))
    }
}

/// A signed-in account holder.
#[derive(Debug, Clone)]
pub(crate) struct UserSession {
    pub(crate) account: Account,
    pub(crate) csrf: String,
    pub(crate) token_hash: String,
}

impl UserSession {
    pub(crate) async fn from_headers(app: &App, headers: &HeaderMap) -> Result<Option<Self>, Reject> {
        let Some(token) = cookie(headers, &app.session_cookie()) else {
            return Ok(None);
        };
        let token_hash = auth::token_hash(&token);
        let Some(session) = app.wallet.db.session(&token_hash).await? else {
            return Ok(None);
        };
        let Some(account_id) = session.account_id else {
            return Ok(None);
        };
        // A frozen account is signed out everywhere, even mid-session.
        let Some(account) = app
            .wallet
            .db
            .account(account_id)
            .await?
            .filter(|account| !account.frozen)
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            account,
            csrf: session.csrf_token,
            token_hash,
        }))
    }
}

impl FromRequestParts<Shared> for UserSession {
    type Rejection = Reject;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        let htmx = is_htmx(&parts.headers);
        Self::from_headers(app, &parts.headers)
            .await?
            .ok_or(Reject::Login { htmx })
    }
}

/// A request the operator pages may answer: they are configured, and the
/// request reached the operator host when one is set. Anything else is a 404.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OperatorHost;

impl FromRequestParts<Shared> for OperatorHost {
    type Rejection = Reject;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        if app.admin_hash.is_none() || !app.on_operator_host(parts) {
            return Err(Reject::NotFound);
        }
        Ok(Self)
    }
}

/// The operator, signed in with the separate admin credential.
#[derive(Debug, Clone)]
pub(crate) struct OperatorSession {
    pub(crate) csrf: String,
    pub(crate) token_hash: String,
}

impl FromRequestParts<Shared> for OperatorSession {
    type Rejection = Reject;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        OperatorHost::from_request_parts(parts, app).await?;
        let token = cookie(&parts.headers, &app.admin_cookie()).ok_or(Reject::Operator)?;
        let token_hash = auth::token_hash(&token);
        match app.wallet.db.session(&token_hash).await? {
            Some(session) if session.account_id.is_none() => Ok(Self {
                csrf: session.csrf_token,
                token_hash,
            }),
            _ => Err(Reject::Operator),
        }
    }
}
