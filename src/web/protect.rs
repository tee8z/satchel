//! Abuse protections on the web side: the proof-of-work endpoint and the
//! checks every account creation passes, operator blocks in the middleware,
//! and the operator's blocks and busiest-addresses views.

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Form, Json};
use maud::{Markup, html};
use serde::Deserialize;

use super::pages::{self, Ctx};
use super::{App, ClientIp, OperatorSession, Reject, Shared, assets, check_csrf, redirect};
use crate::abuse::{self, BusyClient};
use crate::blocklist::{Block, Cidr};
use crate::error::LnurlError;
use crate::metrics::inc;
use crate::util::{format_msat, format_time, now};

const HOUR_SECS: i64 = 3600;
const DAY_SECS: i64 = 86_400;
const MAINTAIN_EVERY: Duration = Duration::from_secs(60);
const BUSIEST_LEN: i64 = 10;
/// One-click blocks from the busiest view expire after a day: crowds move on.
const QUICK_BLOCK_HOURS: i64 = 24;
/// The longest expiry the block form accepts (ten years).
const MAX_BLOCK_HOURS: i64 = 87_600;
const MAX_REASON_CHARS: usize = 140;

impl App {
    /// Proof-of-work bits a new account needs now, with the number of accounts
    /// created in the last hour. Also updates the difficulty gauge.
    pub(crate) async fn pow_difficulty(&self) -> Result<(u8, i64), sqlx::Error> {
        let recent = self.wallet.db.accounts_created_since(now() - HOUR_SECS).await?;
        let difficulty = self.pow.difficulty(u64::try_from(recent).unwrap_or(0));
        self.wallet
            .metrics
            .pow_difficulty
            .store(u64::from(difficulty), Ordering::Relaxed);
        Ok((difficulty, recent))
    }

    /// Checks every account creation passes before any expensive work: the
    /// global hourly cap, then the proof of work, which it uses up. `Some` is
    /// the reason to show.
    pub(crate) async fn refuse_new_account(
        &self,
        pow_challenge: &str,
        pow_nonce: &str,
    ) -> Result<Option<&'static str>, Reject> {
        let (required, recent) = self.pow_difficulty().await?;
        if recent >= i64::from(self.rate.signups_global_per_hour) {
            self.wallet.metrics.rate_limited("signup-global");
            return Ok(Some("Many wallets were created in the last hour. Try again later."));
        }
        if !self.pow.enabled {
            return Ok(None);
        }
        match self.pow.verify(pow_challenge, pow_nonce, required, now()) {
            Ok(()) => {
                inc(&self.wallet.metrics.pow_verified);
                Ok(None)
            }
            Err(error) => {
                inc(&self.wallet.metrics.pow_rejected);
                tracing::debug!(?error, "proof of work refused");
                Ok(Some(error.message()))
            }
        }
    }

    /// Records where a new account came from. A failure is logged and never
    /// undoes the sign-up.
    pub(crate) async fn note_new_account(&self, account_id: i64, ip: ClientIp) {
        if let Err(error) = self.wallet.db.record_signup(account_id, &ip.key()).await {
            tracing::error!(%error, account = account_id, "cannot record the sign-up address");
        }
    }
}

/// `POST /auth/pow`: a challenge for the next account creation. With proof of
/// work off it is trivial (zero bits) and never checked.
pub(super) async fn pow_challenge(State(app): State<Shared>) -> Result<Response, Reject> {
    let (difficulty, _) = app.pow_difficulty().await?;
    Ok(Json(app.pow.issue(difficulty, now())).into_response())
}

/// Hidden fields for a form that creates an account. `app.js` fetches a
/// challenge as the page loads, solves it in a worker, and fills them in.
pub(crate) fn pow_fields() -> Markup {
    html! {
        input type="hidden" name="pow_challenge" value="";
        input type="hidden" name="pow_nonce" value="";
        span hidden data-pow-worker=(assets::url("pow-worker.js")) data-pow-sha256=(assets::url("sha256.js")) {}
        p.muted.pow-status role="status" {}
        noscript {
            p.muted { "Creating a wallet runs a short check in your browser against automated sign-ups, so it needs JavaScript." }
        }
    }
}

/// The answer to a request from a blocked network, or `None` to go on. The
/// operator pages, static files and the health check stay reachable, so the
/// operator can always lift a block.
pub(super) fn refuse_blocked(app: &App, request: &Request, lnurl: bool, admin: bool) -> Option<Response> {
    let path = request.uri().path();
    if admin || path.starts_with("/assets/") || path == "/healthz" {
        return None;
    }
    let ip = ClientIp::from_request(app, request.headers(), request.extensions());
    if !app.blocks.blocks(ip.addr, now()) {
        return None;
    }
    inc(&app.wallet.metrics.blocked_requests);
    if lnurl {
        return Some(LnurlError::new("Requests from your network are blocked").into_response());
    }
    // Scripts and other apps call these; they read JSON, not a page.
    if path.starts_with("/api/") || matches!(path, "/auth/pow" | "/auth/nostr" | "/auth/nostr/challenge") {
        let body = serde_json::json!({ "error": "Requests from your network are blocked" });
        return Some((StatusCode::FORBIDDEN, Json(body)).into_response());
    }
    let ctx = Ctx::visitor(&app.wallet.network);
    let page = pages::layout(
        &ctx,
        "Blocked",
        html! {
            h1 { "Blocked" }
            p { "The operator of this wallet has blocked requests from your network." }
            p.muted { "If you think this is a mistake, contact the operator." }
        },
    );
    Some((StatusCode::FORBIDDEN, page).into_response())
}

/// Every minute: forget expired blocks, old client events and spent
/// challenges, and refresh the difficulty gauge.
pub(crate) async fn maintain_forever(app: Shared) {
    let mut every = tokio::time::interval(MAINTAIN_EVERY);
    loop {
        every.tick().await;
        let now = now();
        app.pow.prune(now);
        match app.wallet.db.delete_expired_blocks(now).await {
            Ok(0) => {}
            Ok(_) => {
                if let Err(error) = app.blocks.reload(&app.wallet.db).await {
                    tracing::error!(%error, "cannot reload blocks");
                }
            }
            Err(error) => tracing::error!(%error, "cannot delete expired blocks"),
        }
        if let Err(error) = app.wallet.db.prune_client_events(now).await {
            tracing::error!(%error, "cannot prune client events");
        }
        if let Err(error) = app.pow_difficulty().await {
            tracing::error!(%error, "cannot read recent sign-ups");
        }
    }
}

// ---- operator views ----

/// The blocks and busiest-addresses sections of the operator page.
pub(super) async fn operator_view(app: &App, csrf: &str) -> Result<Markup, Reject> {
    let db = &app.wallet.db;
    let since = now() - DAY_SECS;
    let blocks = db.blocks().await?;
    let signups = db.busiest_clients(abuse::SIGNUP, since, BUSIEST_LEN).await?;
    let faucet = db.busiest_clients(abuse::FAUCET, since, BUSIEST_LEN).await?;
    let lnurl = db.busiest_clients(abuse::LNURL_INVOICE, since, BUSIEST_LEN).await?;
    let busy = |title: &str, label: &str, rows: &[BusyClient], sats: bool| {
        busy_table(&BusyTable {
            app,
            csrf,
            title,
            label,
            rows,
            sats,
        })
    };
    Ok(html! {
        (blocks_section(csrf, &blocks))
        section.card #busiest {
            h2 { "Busiest addresses (24 h)" }
            p.muted {
                "Counted per client key: an IPv4 address, or an IPv6 prefix. A conference or office network "
                "can be busy and legitimate. One-click blocks cover the /24 or /56 and expire after "
                (QUICK_BLOCK_HOURS) " hours."
            }
            (busy("Sign-ups", "sign-ups", &signups, false))
            (busy("Faucet claims", "faucet claims", &faucet, true))
            (busy("LNURL invoices", "LNURL invoices", &lnurl, false))
        }
    })
}

fn blocks_section(csrf: &str, blocks: &[Block]) -> Markup {
    html! {
        section.card #blocks {
            h2 { "Blocked networks" }
            @if blocks.is_empty() {
                p.muted { "No blocks." }
            } @else {
                div.table {
                    table {
                        thead { tr { th { "Network" } th { "Reason" } th { "Expires" } th { "Added" } th { "Actions" } } }
                        tbody {
                            @for block in blocks {
                                tr {
                                    td { code { (block.cidr) } }
                                    td { (block.reason) }
                                    td {
                                        @match block.expires_at {
                                            Some(at) => { (format_time(at)) }
                                            None => { "never" }
                                        }
                                    }
                                    td { (format_time(block.created_at)) }
                                    td.actions {
                                        form.inline method="post" action=(format!("/admin/blocks/{}/remove", block.id)) {
                                            input type="hidden" name="csrf" value=(csrf);
                                            button.secondary type="submit" { "Remove" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            form method="post" action="/admin/blocks" {
                input type="hidden" name="csrf" value=(csrf);
                label for="block-cidr" { "Network (CIDR or a single address)" }
                input #block-cidr name="cidr" required placeholder="203.0.113.0/24 or 2001:db8:1::/56"
                    autocapitalize="none" spellcheck="false";
                label for="block-reason" { "Reason (only operators see it)" }
                input #block-reason name="reason" required maxlength=(MAX_REASON_CHARS);
                label for="block-hours" { "Expires after (hours; leave empty for never)" }
                input #block-hours name="hours" inputmode="numeric";
                button type="submit" { "Block" }
            }
        }
    }
}

struct BusyTable<'a> {
    app: &'a App,
    csrf: &'a str,
    title: &'a str,
    label: &'a str,
    rows: &'a [BusyClient],
    sats: bool,
}

fn busy_table(table: &BusyTable<'_>) -> Markup {
    let now = now();
    html! {
        h3 { (table.title) }
        @if table.rows.is_empty() {
            p.muted { "None in the last 24 hours." }
        } @else {
            div.table {
                table {
                    thead {
                        tr {
                            th { "Address" } th { (table.title) } th { "Accounts" }
                            @if table.sats { th { "Sats" } }
                            th { "Last" } th { "Actions" }
                        }
                    }
                    tbody {
                        @for row in table.rows {
                            @let quick = Cidr::quick_block(&row.client);
                            tr {
                                td { code { (row.client) } }
                                td.num { (row.events) }
                                td.num { (row.accounts) }
                                @if table.sats { td.num { (format_msat(row.amount_msat)) } }
                                td { (format_time(row.last_at)) }
                                td.actions {
                                    @match quick {
                                        Some(cidr) if table.app.blocks.blocks(cidr.network(), now) => { "blocked" }
                                        Some(cidr) => {
                                            form.inline method="post" action="/admin/blocks" {
                                                input type="hidden" name="csrf" value=(table.csrf);
                                                input type="hidden" name="cidr" value=(cidr);
                                                input type="hidden" name="reason"
                                                    value=(format!("Busy: {} {} in 24 h", row.events, table.label));
                                                input type="hidden" name="hours" value=(QUICK_BLOCK_HOURS);
                                                button.secondary type="submit" { "Block " (cidr) }
                                            }
                                        }
                                        None => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Deserialize)]
pub(super) struct BlockForm {
    csrf: String,
    cidr: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    hours: String,
}

pub(super) async fn add_block(
    State(app): State<Shared>,
    operator: OperatorSession,
    Form(form): Form<BlockForm>,
) -> Result<Response, Reject> {
    check_csrf(&operator.csrf, &form.csrf)?;
    let Some(cidr) = Cidr::parse(&form.cidr).filter(Cidr::is_narrow_enough) else {
        return Ok(redirect("/admin?error=cidr"));
    };
    let hours = form.hours.trim();
    let expires_at = if hours.is_empty() {
        None
    } else {
        match hours.parse::<i64>() {
            Ok(hours) if (1..=MAX_BLOCK_HOURS).contains(&hours) => Some(now() + hours * HOUR_SECS),
            _ => return Ok(redirect("/admin?error=hours")),
        }
    };
    let reason: String = form.reason.trim().chars().take(MAX_REASON_CHARS).collect();
    let reason = if reason.is_empty() {
        "Blocked by the operator".to_owned()
    } else {
        reason
    };
    app.wallet.db.add_block(&cidr, &reason, expires_at).await?;
    app.blocks.reload(&app.wallet.db).await?;
    tracing::info!(%cidr, ?expires_at, "operator blocked a network");
    Ok(redirect("/admin?done=block"))
}

#[derive(Deserialize)]
pub(super) struct CsrfForm {
    csrf: String,
}

pub(super) async fn remove_block(
    State(app): State<Shared>,
    operator: OperatorSession,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, Reject> {
    check_csrf(&operator.csrf, &form.csrf)?;
    if !app.wallet.db.remove_block(id).await? {
        return Err(Reject::NotFound);
    }
    app.blocks.reload(&app.wallet.db).await?;
    tracing::info!(block = id, "operator removed a block");
    Ok(redirect("/admin?done=unblock"))
}
