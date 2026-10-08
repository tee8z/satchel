//! Prometheus text metrics. Totals only: no per-account labels.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::db::Totals;

#[derive(Debug, Default)]
pub(crate) struct Metrics {
    pub(crate) sends_succeeded: AtomicU64,
    pub(crate) sends_failed: AtomicU64,
    pub(crate) internal_payments: AtomicU64,
    pub(crate) invoices_settled_lnurl: AtomicU64,
    pub(crate) invoices_settled_wallet: AtomicU64,
    pub(crate) faucet_grants: AtomicU64,
    pub(crate) signups: AtomicU64,
    pub(crate) login_failures: AtomicU64,
    /// Requests refused by rate limits and global caps, by scope (fixed names from the code).
    rate_limited: Mutex<BTreeMap<String, u64>>,
    pub(crate) blocked_requests: AtomicU64,
    pub(crate) pow_verified: AtomicU64,
    pub(crate) pow_rejected: AtomicU64,
    /// Proof-of-work bits required for a new account right now.
    pub(crate) pow_difficulty: AtomicU64,
    pub(crate) faucet_paid_msat: AtomicU64,
    pub(crate) invoice_stream_up: AtomicBool,
    pub(crate) invoice_stream_connecting: AtomicBool,
    pub(crate) invoice_stream_reconnects: AtomicU64,
    pub(crate) reconciliation_last_attempt: AtomicU64,
    pub(crate) reconciliation_last_success: AtomicU64,
    pub(crate) reconciliation_failures: AtomicU64,
    /// Updated by the reconciler each minute.
    pub(crate) node_channel_local_msat: AtomicU64,
    pub(crate) node_balance_known: AtomicBool,
}

pub(crate) fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Metrics {
    pub(crate) fn invoice_stream_state(&self) -> &'static str {
        if self.invoice_stream_up.load(Ordering::Relaxed) {
            "connected"
        } else if self.invoice_stream_connecting.load(Ordering::Relaxed) {
            // LND's REST proxy can wait for an invoice before sending headers.
            "awaiting_response"
        } else {
            "disconnected"
        }
    }

    pub(crate) fn rate_limited(&self, scope: &str) {
        let mut counts = self.rate_limited.lock().expect("metrics lock is not poisoned");
        *counts.entry(scope.to_owned()).or_default() += 1;
    }

    pub(crate) fn render(&self, totals: &Totals) -> String {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let limited: Vec<(String, f64)> = self
            .rate_limited
            .lock()
            .expect("metrics lock is not poisoned")
            .iter()
            .map(|(scope, count)| (format!("{{scope=\"{scope}\"}}"), *count as f64))
            .collect();
        let limited: Vec<(&str, f64)> = limited
            .iter()
            .map(|(labels, count)| (labels.as_str(), *count))
            .collect();
        let mut out = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, samples: &[(&str, f64)]| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for (labels, value) in samples {
                let _ = writeln!(out, "{name}{labels} {value}");
            }
        };
        metric(
            "satchel_accounts",
            "gauge",
            "Accounts on this server.",
            &[("", totals.accounts as f64)],
        );
        metric(
            "satchel_frozen_accounts",
            "gauge",
            "Frozen accounts.",
            &[("", totals.frozen_accounts as f64)],
        );
        metric(
            "satchel_liabilities_msat",
            "gauge",
            "Sum of all account balances.",
            &[("", totals.liabilities_msat as f64)],
        );
        if self.node_balance_known.load(Ordering::Relaxed) {
            let channel = load(&self.node_channel_local_msat);
            metric(
                "satchel_node_channel_local_msat",
                "gauge",
                "Local channel balance of the LND node.",
                &[("", channel as f64)],
            );
        }
        metric(
            "satchel_pending_payments",
            "gauge",
            "Outgoing payments waiting for a final status.",
            &[("", totals.pending_payments as f64)],
        );
        metric(
            "satchel_open_invoices",
            "gauge",
            "Unpaid invoices.",
            &[("", totals.open_invoices as f64)],
        );
        metric(
            "satchel_faucet_last_day_msat",
            "gauge",
            "Faucet credits in the last 24 hours.",
            &[("", totals.faucet_last_day_msat as f64)],
        );
        metric(
            "satchel_payments_total",
            "counter",
            "Payments by kind and outcome since start.",
            &[
                (
                    "{kind=\"lightning\",outcome=\"succeeded\"}",
                    load(&self.sends_succeeded) as f64,
                ),
                (
                    "{kind=\"lightning\",outcome=\"failed\"}",
                    load(&self.sends_failed) as f64,
                ),
                (
                    "{kind=\"internal\",outcome=\"succeeded\"}",
                    load(&self.internal_payments) as f64,
                ),
            ],
        );
        metric(
            "satchel_invoices_settled_total",
            "counter",
            "Invoices paid over Lightning since start.",
            &[
                ("{source=\"lnurl\"}", load(&self.invoices_settled_lnurl) as f64),
                ("{source=\"wallet\"}", load(&self.invoices_settled_wallet) as f64),
            ],
        );
        metric(
            "satchel_faucet_grants_total",
            "counter",
            "Faucet grants since start.",
            &[("", load(&self.faucet_grants) as f64)],
        );
        metric(
            "satchel_signups_total",
            "counter",
            "Accounts created since start.",
            &[("", load(&self.signups) as f64)],
        );
        metric(
            "satchel_login_failures_total",
            "counter",
            "Failed logins since start.",
            &[("", load(&self.login_failures) as f64)],
        );
        metric(
            "satchel_rate_limited_total",
            "counter",
            "Requests refused by rate limits and global caps since start, by scope.",
            &limited,
        );
        metric(
            "satchel_blocked_requests_total",
            "counter",
            "Requests refused by operator blocks since start.",
            &[("", load(&self.blocked_requests) as f64)],
        );
        metric(
            "satchel_pow_checks_total",
            "counter",
            "Proof-of-work solutions checked for new accounts since start.",
            &[
                ("{outcome=\"verified\"}", load(&self.pow_verified) as f64),
                ("{outcome=\"rejected\"}", load(&self.pow_rejected) as f64),
            ],
        );
        metric(
            "satchel_pow_difficulty_bits",
            "gauge",
            "Proof-of-work bits a new account needs now.",
            &[("", load(&self.pow_difficulty) as f64)],
        );
        metric(
            "satchel_faucet_paid_msat_total",
            "counter",
            "Faucet sats paid since start, in msat.",
            &[("", load(&self.faucet_paid_msat) as f64)],
        );
        metric(
            "satchel_invoice_stream_up",
            "gauge",
            "Whether the LND invoice subscription is connected.",
            &[("", f64::from(u8::from(self.invoice_stream_up.load(Ordering::Relaxed))))],
        );
        metric(
            "satchel_invoice_stream_connecting",
            "gauge",
            "Subscription awaiting HTTP response headers; an idle LND may not send them yet.",
            &[(
                "",
                f64::from(u8::from(self.invoice_stream_connecting.load(Ordering::Relaxed))),
            )],
        );
        for (name, help, value) in [
            (
                "satchel_reconciliation_last_attempt_timestamp_seconds",
                "Last reconciliation start, or zero before the first attempt.",
                load(&self.reconciliation_last_attempt),
            ),
            (
                "satchel_reconciliation_last_success_timestamp_seconds",
                "Last reconciliation that checked invoices, pending sends and node balance successfully.",
                load(&self.reconciliation_last_success),
            ),
        ] {
            metric(name, "gauge", help, &[("", value as f64)]);
        }
        metric(
            "satchel_reconciliation_failures_total",
            "counter",
            "Reconciliation passes with at least one failed check or ledger update.",
            &[("", load(&self.reconciliation_failures) as f64)],
        );
        metric(
            "satchel_invoice_stream_reconnects_total",
            "counter",
            "Invoice subscriptions that ended or failed and will be retried.",
            &[("", load(&self.invoice_stream_reconnects) as f64)],
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_totals_without_account_labels() {
        let metrics = Metrics::default();
        inc(&metrics.sends_succeeded);
        metrics.node_channel_local_msat.store(50_000, Ordering::Relaxed);
        metrics.node_balance_known.store(true, Ordering::Relaxed);
        let totals = Totals {
            accounts: 2,
            liabilities_msat: 21_000,
            ..Totals::default()
        };
        metrics.rate_limited("signup");
        metrics.rate_limited("signup");
        metrics.rate_limited("lnurl-ip");
        inc(&metrics.blocked_requests);
        metrics.pow_difficulty.store(19, Ordering::Relaxed);
        let text = metrics.render(&totals);
        assert!(text.contains("satchel_rate_limited_total{scope=\"signup\"} 2\n"));
        assert!(text.contains("satchel_rate_limited_total{scope=\"lnurl-ip\"} 1\n"));
        assert!(text.contains("satchel_blocked_requests_total 1\n"));
        assert!(text.contains("satchel_pow_difficulty_bits 19\n"));
        assert!(text.contains("satchel_pow_checks_total{outcome=\"rejected\"} 0\n"));
        assert!(text.contains("satchel_accounts 2\n"));
        assert!(text.contains("satchel_liabilities_msat 21000\n"));
        assert!(text.contains("satchel_node_channel_local_msat 50000\n"));
        assert!(text.contains("satchel_payments_total{kind=\"lightning\",outcome=\"succeeded\"} 1\n"));
        assert!(!text.contains("username"));
    }
}
