//! Server-rendered pages and the fragments htmx swaps into them. Every form
//! also works without JavaScript: it posts and gets the whole page back.

use maud::{DOCTYPE, Markup, html};

use super::UserSession;
use super::assets;
use crate::APP_NAME;
use crate::db::{AccountSummary, Totals};
use crate::ledger::{Invoice, Payment};
use crate::lnd::NodeBalances;
use crate::qr;
use crate::util::{format_msat, format_time, now};
use crate::wallet::Faucet;

/// htmx sends requests to this origin only and leaves styling to our stylesheet.
const HTMX_CONFIG: &str = r#"{"mode":"same-origin","defaultTimeout":35000,"includeIndicatorCSS":false}"#;

pub(crate) enum Nav<'a> {
    Visitor,
    Member { username: &'a str, csrf: &'a str },
    Operator { csrf: &'a str },
}

pub(crate) struct Ctx<'a> {
    pub(crate) network: &'a str,
    pub(crate) nav: Nav<'a>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn visitor(network: &'a str) -> Self {
        Self {
            network,
            nav: Nav::Visitor,
        }
    }

    pub(crate) fn member(network: &'a str, session: &'a UserSession) -> Self {
        Self {
            network,
            nav: Nav::Member {
                username: &session.account.username,
                csrf: &session.csrf,
            },
        }
    }

    pub(crate) fn operator(network: &'a str, csrf: &'a str) -> Self {
        Self {
            network,
            nav: Nav::Operator { csrf },
        }
    }
}

pub(crate) fn layout(ctx: &Ctx<'_>, title: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="robots" content="noindex";
                meta name="htmx-config" content=(HTMX_CONFIG);
                title { (title) " - " (APP_NAME) }
                link rel="stylesheet" href=(assets::url("app.css"));
                script src=(assets::url("htmx.min.js")) defer {}
                script src=(assets::url("app.js")) defer {}
            }
            body {
                div.banner role="note" {
                    strong { "Test network only (" (ctx.network) ")." }
                    " Not for real bitcoin: custodial, unaudited test software."
                }
                header.top {
                    a.brand href="/" { (APP_NAME) }
                    nav {
                        @match ctx.nav {
                            Nav::Visitor => {
                                a href="/login" { "Log in" }
                                a href="/signup" { "Sign up" }
                            }
                            Nav::Member { username, csrf } => {
                                a href="/wallet" { "Wallet" }
                                a href="/settings" title=(username) { "Settings" }
                                form.inline method="post" action="/logout" {
                                    input type="hidden" name="csrf" value=(csrf);
                                    button.link type="submit" { "Log out" }
                                }
                            }
                            Nav::Operator { csrf } => {
                                a href="/admin" { "Accounts" }
                                form.inline method="post" action="/admin/logout" {
                                    input type="hidden" name="csrf" value=(csrf);
                                    button.link type="submit" { "Log out" }
                                }
                            }
                        }
                    }
                }
                main { (content) }
                footer {
                    "Balances are test sats with no value. Never send real bitcoin to this wallet."
                }
            }
        }
    }
}

pub(crate) fn not_found(ctx: &Ctx<'_>) -> Markup {
    layout(
        ctx,
        "Not found",
        html! {
            h1 { "Not found" }
            p { a href="/" { "Go to the start page" } }
        },
    )
}

pub(crate) fn landing(ctx: &Ctx<'_>, domain: &str) -> Markup {
    layout(
        ctx,
        "Test wallet",
        html! {
            h1 { "A Lightning wallet for test networks" }
            p {
                "Get a wallet and a Lightning Address on " strong { (ctx.network) } " in seconds: pay invoices "
                "from apps you are testing, and receive payouts or refunds at " code { "you@" (domain) } "."
            }
            ul.warnings {
                li { "Test sats only. This server refuses to start on Bitcoin mainnet." }
                li { "Custodial: the operator's Lightning node holds every balance." }
                li { "Not audited. Never send real bitcoin here." }
            }
            p.actions {
                a.button href="/signup" { "Create a wallet" }
                a.button.secondary href="/login" { "Log in" }
            }
        },
    )
}

fn nostr_button(mode: &str, label: &str, csrf: Option<&str>) -> Markup {
    html! {
        div.nostr {
            button.secondary type="button" data-nostr=(mode) data-csrf=[csrf] { (label) }
            p.nostr-status role="status" {}
        }
    }
}

pub(crate) fn signup(ctx: &Ctx<'_>, domain: &str, error: Option<&str>, username: &str) -> Markup {
    layout(
        ctx,
        "Create a wallet",
        html! {
            h1 { "Create a wallet" }
            @if let Some(error) = error { p.error role="alert" { (error) } }
            form.card method="post" action="/signup" {
                label for="username" { "Username" }
                div.with-suffix {
                    input #username name="username" required minlength="3" maxlength="32" value=(username)
                        autocomplete="username" autocapitalize="none" spellcheck="false";
                    span { "@" (domain) }
                }
                label for="password" { "Password (at least 10 characters)" }
                input #password type="password" name="password" required minlength="10" autocomplete="new-password";
                label for="confirm" { "Repeat the password" }
                input #confirm type="password" name="confirm" required minlength="10" autocomplete="new-password";
                button type="submit" { "Create wallet" }
            }
            section.card {
                p { "Or sign up with a Nostr signer extension (NIP-07), using the username above:" }
                (nostr_button("signup", "Sign up with Nostr", None))
            }
            p { "Already have a wallet? " a href="/login" { "Log in" } }
        },
    )
}

pub(crate) fn login(ctx: &Ctx<'_>, error: Option<&str>, username: &str) -> Markup {
    layout(
        ctx,
        "Log in",
        html! {
            h1 { "Log in" }
            @if let Some(error) = error { p.error role="alert" { (error) } }
            form.card method="post" action="/login" {
                label for="username" { "Username" }
                input #username name="username" required value=(username) autocomplete="username"
                    autocapitalize="none" spellcheck="false";
                label for="password" { "Password" }
                input #password type="password" name="password" required autocomplete="current-password";
                button type="submit" { "Log in" }
            }
            section.card {
                p { "Linked a Nostr key? Log in with your signer extension:" }
                (nostr_button("login", "Log in with Nostr", None))
            }
            p { "New here? " a href="/signup" { "Create a wallet" } }
        },
    )
}

pub(crate) fn balance(balance_msat: i64, out_of_band: bool) -> Markup {
    html! {
        span #balance hx-swap-oob=[out_of_band.then_some("true")] { (format_msat(balance_msat)) " sats" }
    }
}

fn describe(payment: &Payment) -> String {
    let incoming = payment.direction == "in";
    let what = match (payment.kind.as_str(), incoming) {
        ("faucet", _) => "Faucet".to_owned(),
        ("operator", _) => "Credit from the operator".to_owned(),
        ("internal", true) => format!("From {}", payment.counterparty),
        ("internal", false) => format!("To {}", payment.counterparty),
        (_, true) => "Received over Lightning".to_owned(),
        (_, false) if payment.counterparty.is_empty() => "Sent over Lightning".to_owned(),
        (_, false) => format!("Sent to {}", payment.counterparty),
    };
    if payment.memo.is_empty() {
        what
    } else {
        format!("{what}: {}", payment.memo)
    }
}

pub(crate) fn history(payments: &[Payment], out_of_band: bool) -> Markup {
    html! {
        section.card #history hx-swap-oob=[out_of_band.then_some("true")] {
            h2 { "History" }
            @if payments.is_empty() {
                p.muted { "No payments yet." }
            } @else {
                ul.history {
                    @for payment in payments {
                        @let incoming = payment.direction == "in";
                        li title=[payment.payment_hash.as_deref()] {
                            span.what { (describe(payment)) }
                            span.amount.in[incoming].out[!incoming] {
                                (if incoming { "+" } else { "-" }) (format_msat(payment.amount_msat))
                            }
                            span.when { (format_time(payment.created_at)) }
                            span.state {
                                @match payment.status.as_str() {
                                    "pending" => { "pending" }
                                    "failed" => { "failed" @if let Some(failure) = &payment.failure { ": " (failure) } }
                                    _ => {
                                        @if payment.fee_msat > 0 { "fee " (format_msat(payment.fee_msat)) }
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

pub(crate) enum ReceiveState<'a> {
    Form {
        error: Option<&'a str>,
        amount: &'a str,
        memo: &'a str,
    },
    Invoice(&'a Invoice),
}

pub(crate) fn receive_section(csrf: &str, state: &ReceiveState<'_>) -> Markup {
    html! {
        section.card #receive {
            h2 { "Receive" }
            @match state {
                ReceiveState::Form { error, amount, memo } => {
                    form method="post" action="/wallet/receive" hx-post="/wallet/receive" hx-target="#receive"
                        hx-swap="outerHTML" hx-disable="find button" {
                        input type="hidden" name="csrf" value=(csrf);
                        label for="receive-amount" { "Amount (sats)" }
                        input #receive-amount name="amount_sat" inputmode="numeric" required value=(amount)
                            placeholder="1000";
                        label for="receive-memo" { "Note (optional)" }
                        input #receive-memo name="memo" maxlength="140" value=(memo);
                        button type="submit" { "Create invoice" }
                    }
                    @if let Some(error) = error { p.error role="alert" { (error) } }
                }
                ReceiveState::Invoice(invoice) => {
                    p { "Invoice for " strong { (format_msat(invoice.amount_msat)) " sats" } }
                    (qr::svg(&format!("lightning:{}", invoice.bolt11).to_uppercase(), "Invoice QR code"))
                    textarea.code readonly rows="4" aria-label="Invoice" { (invoice.bolt11) }
                    p.actions {
                        button.secondary type="button" data-copy=(invoice.bolt11) { "Copy invoice" }
                        a href="/wallet" { "New invoice" }
                    }
                    (invoice_status(invoice))
                }
            }
        }
    }
}

/// Polls every two seconds while the invoice is open; the answer replaces it.
pub(crate) fn invoice_status(invoice: &Invoice) -> Markup {
    html! {
        @match invoice.state.as_str() {
            "settled" => {
                p.ok #invoice-status role="status" { "Paid: " (format_msat(invoice.amount_msat)) " sats received." }
            }
            "canceled" => { p.error #invoice-status role="status" { "This invoice was canceled." } }
            _ if invoice.expires_at <= now() => {
                p.error #invoice-status role="status" { "This invoice expired." }
            }
            _ => {
                p.muted #invoice-status role="status" hx-get=(format!("/wallet/invoice/{}", invoice.payment_hash))
                    hx-trigger="every 2s" hx-swap="outerHTML" {
                    "Waiting for payment. Expires " (format_time(invoice.expires_at)) "."
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct SendValues<'a> {
    pub(crate) destination: &'a str,
    pub(crate) amount: &'a str,
    pub(crate) comment: &'a str,
}

pub(crate) fn send_section(
    csrf: &str,
    key: &str,
    values: &SendValues<'_>,
    result: Option<Result<&Payment, &str>>,
) -> Markup {
    html! {
        section.card #send {
            h2 { "Send" }
            form method="post" action="/wallet/send" hx-post="/wallet/send" hx-target="#send" hx-swap="outerHTML"
                hx-disable="find button" {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="key" value=(key);
                label for="send-to" { "Invoice, Lightning Address, or LNURL" }
                textarea #send-to name="destination" rows="3" required autocapitalize="none" spellcheck="false" {
                    (values.destination)
                }
                label for="send-amount" { "Amount (sats; for addresses and invoices without an amount)" }
                input #send-amount name="amount_sat" inputmode="numeric" value=(values.amount);
                label for="send-comment" { "Comment for an address (optional)" }
                input #send-comment name="comment" maxlength="140" value=(values.comment);
                button type="submit" { "Send" }
                p.muted { "A routing-fee budget is held while a payment is in flight; the unused part comes back." }
            }
            @match result {
                Some(Ok(payment)) => { (payment_status(payment)) }
                Some(Err(message)) => { p.error role="alert" { (message) } }
                None => {}
            }
        }
    }
}

/// Polls every two seconds while the payment is pending.
pub(crate) fn payment_status(payment: &Payment) -> Markup {
    html! {
        @match payment.status.as_str() {
            "succeeded" => {
                p.ok #payment-status role="status" {
                    "Sent " (format_msat(payment.amount_msat)) " sats"
                    @if !payment.counterparty.is_empty() { " to " (payment.counterparty) }
                    @if payment.fee_msat > 0 { " (fee " (format_msat(payment.fee_msat)) " sats)" }
                    "."
                }
            }
            "failed" => {
                p.error #payment-status role="alert" {
                    "Payment failed: " (payment.failure.as_deref().unwrap_or("unknown reason"))
                    " Your sats are back in your balance."
                }
            }
            _ => {
                p.muted #payment-status role="status" hx-get=(format!("/wallet/payment/{}", payment.id))
                    hx-trigger="every 2s" hx-swap="outerHTML" {
                    "Payment in flight: " (format_msat(payment.amount_msat + payment.fee_limit_msat))
                    " sats held, including the fee budget."
                }
            }
        }
    }
}

pub(crate) fn faucet_section(
    csrf: &str,
    key: &str,
    amount_msat: i64,
    result: Option<Result<&Payment, &str>>,
) -> Markup {
    html! {
        section.card #faucet {
            h2 { "Faucet" }
            p { "Get " (format_msat(amount_msat)) " test sats from the operator, a few times a day." }
            form method="post" action="/wallet/faucet" hx-post="/wallet/faucet" hx-target="#faucet" hx-swap="outerHTML"
                hx-disable="find button" {
                input type="hidden" name="csrf" value=(csrf);
                input type="hidden" name="key" value=(key);
                button type="submit" { "Get test sats" }
            }
            @match result {
                Some(Ok(payment)) => {
                    p.ok role="status" { "Added " (format_msat(payment.amount_msat)) " test sats." }
                }
                Some(Err(message)) => { p.error role="alert" { (message) } }
                None => {}
            }
        }
    }
}

pub(crate) struct WalletPage<'a> {
    pub(crate) address: &'a str,
    pub(crate) lnurl: &'a str,
    pub(crate) balance_msat: i64,
    pub(crate) frozen: bool,
    pub(crate) history: &'a [Payment],
    pub(crate) receive: Markup,
    pub(crate) send: Markup,
    pub(crate) faucet: Option<Markup>,
}

pub(crate) fn wallet(ctx: &Ctx<'_>, page: &WalletPage<'_>) -> Markup {
    layout(
        ctx,
        "Wallet",
        html! {
            section.card.summary {
                p.label { "Balance" }
                p.balance { (balance(page.balance_msat, false)) }
                @if page.frozen {
                    p.error role="alert" { "This account is frozen: it cannot send or receive. Contact the operator." }
                }
                p.label { "Your Lightning Address" }
                p.address {
                    code { (page.address) }
                    button.secondary type="button" data-copy=(page.address) { "Copy" }
                }
                details {
                    summary { "Show a QR code for your address (LNURL)" }
                    (qr::svg(page.lnurl, "Lightning Address QR code"))
                    textarea.code readonly rows="3" aria-label="LNURL" { (page.lnurl) }
                }
            }
            (page.receive)
            (page.send)
            @if let Some(faucet) = &page.faucet { (faucet) }
            (history(page.history, false))
        },
    )
}

pub(crate) struct SettingsPage<'a> {
    pub(crate) address: &'a str,
    pub(crate) created_at: i64,
    pub(crate) npub: Option<&'a str>,
    pub(crate) has_password: bool,
    pub(crate) csrf: &'a str,
    pub(crate) message: Option<Result<&'a str, &'a str>>,
}

pub(crate) fn settings(ctx: &Ctx<'_>, page: &SettingsPage<'_>) -> Markup {
    layout(
        ctx,
        "Settings",
        html! {
            h1 { "Settings" }
            @match page.message {
                Some(Ok(message)) => { p.ok role="status" { (message) } }
                Some(Err(message)) => { p.error role="alert" { (message) } }
                None => {}
            }
            section.card {
                h2 { "Account" }
                dl {
                    dt { "Lightning Address" } dd { code { (page.address) } }
                    dt { "Created" } dd { (format_time(page.created_at)) }
                }
            }
            section.card {
                h2 { "Nostr login" }
                @if let Some(npub) = page.npub {
                    p { "Linked key:" }
                    p { code.wrap { (npub) } }
                    @if page.has_password {
                        form method="post" action="/settings/nostr/unlink" {
                            input type="hidden" name="csrf" value=(page.csrf);
                            button.secondary type="submit" { "Unlink this key" }
                        }
                    } @else {
                        p.muted { "Set a password below before unlinking Nostr." }
                    }
                } @else {
                    p { "Link a Nostr key to log in with a NIP-07 signer extension." }
                    (nostr_button("link", "Link Nostr signer", Some(page.csrf)))
                }
            }
            section.card {
                h2 { @if page.has_password { "Change password" } @else { "Set a password" } }
                form method="post" action="/settings/password" {
                    input type="hidden" name="csrf" value=(page.csrf);
                    @if page.has_password {
                        label for="current" { "Current password" }
                        input #current type="password" name="current" required autocomplete="current-password";
                    }
                    label for="new" { "New password (at least 10 characters)" }
                    input #new type="password" name="new" required minlength="10" autocomplete="new-password";
                    label for="confirm" { "Repeat the new password" }
                    input #confirm type="password" name="confirm" required minlength="10" autocomplete="new-password";
                    button type="submit" { "Save password" }
                }
            }
        },
    )
}

pub(crate) fn admin_login(ctx: &Ctx<'_>, error: Option<&str>) -> Markup {
    layout(
        ctx,
        "Operator login",
        html! {
            h1 { "Operator login" }
            @if let Some(error) = error { p.error role="alert" { (error) } }
            form.card method="post" action="/admin/login" {
                label for="password" { "Operator password" }
                input #password type="password" name="password" required autocomplete="current-password";
                button type="submit" { "Log in" }
            }
        },
    )
}

pub(crate) struct AdminPage<'a> {
    pub(crate) csrf: &'a str,
    pub(crate) totals: &'a Totals,
    /// The node's balances and when they were read.
    pub(crate) node: Option<(NodeBalances, i64)>,
    pub(crate) accounts: &'a [AccountSummary],
    pub(crate) search: &'a str,
    pub(crate) faucet: Faucet,
    pub(crate) notice: Option<Result<&'a str, &'a str>>,
}

pub(crate) fn admin_dashboard(ctx: &Ctx<'_>, page: &AdminPage<'_>) -> Markup {
    let totals = page.totals;
    let sats = |msat: u64| format_msat(i64::try_from(msat).unwrap_or(i64::MAX));
    layout(
        ctx,
        "Operator",
        html! {
            h1 { "Operator" }
            @match page.notice {
                Some(Ok(message)) => { p.ok role="status" { (message) } }
                Some(Err(message)) => { p.error role="alert" { (message) } }
                None => {}
            }
            section.card {
                h2 { "Totals" }
                dl {
                    dt { "Accounts" } dd { (totals.accounts) " (" (totals.frozen_accounts) " frozen)" }
                    dt { "Liabilities (sum of balances)" } dd { (format_msat(totals.liabilities_msat)) " sats" }
                    @match page.node {
                        Some((node, read_at)) => {
                            @let cover = i64::try_from(node.channel_local_msat).unwrap_or(i64::MAX) - totals.liabilities_msat;
                            dt { "Node channel balance (local)" } dd { (sats(node.channel_local_msat)) " sats" }
                            dt { "Channel balance minus liabilities" }
                            dd.error[cover < 0] { (format_msat(cover)) " sats" }
                            dt { "Node on-chain (confirmed)" } dd { (format_msat(i64::try_from(node.onchain_confirmed_sat.saturating_mul(1000)).unwrap_or(i64::MAX))) " sats" }
                            dt { "Node balances read" }
                            dd.error[read_at < now() - 300] { (format_time(read_at)) }
                        }
                        None => { dt { "Node balance" } dd.error { "not read yet (checked every minute)" } }
                    }
                    dt { "Pending payments" } dd { (totals.pending_payments) }
                    dt { "Open invoices" } dd { (totals.open_invoices) }
                    dt { "Faucet" }
                    dd {
                        @if page.faucet.enabled {
                            "On: " (sats(page.faucet.amount_msat)) " sats per grant, "
                            (sats(page.faucet.per_account_daily_msat)) " per account and "
                            (sats(page.faucet.global_daily_msat)) " overall per 24 h; "
                            (format_msat(totals.faucet_last_day_msat)) " given in the last 24 h."
                        } @else { "Off" }
                    }
                }
            }
            form.search method="get" action="/admin" {
                label for="q" { "Find accounts" }
                input #q name="q" value=(page.search) placeholder="username";
                button.secondary type="submit" { "Search" }
            }
            div.table {
                table {
                    thead { tr { th { "Account" } th { "Balance" } th { "Login" } th { "Created" } th { "Actions" } } }
                    tbody {
                        @for account in page.accounts {
                            tr.frozen[account.frozen] {
                                td { (account.username) @if account.frozen { " (frozen)" } }
                                td.num { (format_msat(account.balance_msat)) }
                                td {
                                    @if account.has_password { "password " }
                                    @if account.has_nostr { "nostr" }
                                }
                                td { (format_time(account.created_at)) }
                                td.actions {
                                    form.inline method="post" action=(format!("/admin/accounts/{}/freeze", account.id)) {
                                        input type="hidden" name="csrf" value=(page.csrf);
                                        input type="hidden" name="frozen" value=(if account.frozen { "0" } else { "1" });
                                        button.secondary type="submit" { @if account.frozen { "Unfreeze" } @else { "Freeze" } }
                                    }
                                    form.inline method="post" action=(format!("/admin/accounts/{}/credit", account.id)) {
                                        input type="hidden" name="csrf" value=(page.csrf);
                                        input name="amount_sat" inputmode="numeric" required placeholder="sats" size="7"
                                            aria-label=(format!("Credit {} with sats", account.username));
                                        input name="note" maxlength="140" placeholder="note" size="10" aria-label="Note";
                                        button.secondary type="submit" { "Credit" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}
