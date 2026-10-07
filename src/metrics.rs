//! Prometheus text metrics. Totals only: no per-account labels.

use std::fmt::Write;
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
    pub(crate) rate_limited: AtomicU64,
    pub(crate) invoice_stream_up: AtomicBool,
    /// Updated by the reconciler each minute.
    pub(crate) node_channel_local_msat: AtomicU64,
    pub(crate) node_balance_known: AtomicBool,
}

pub(crate) fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Metrics {
    pub(crate) fn render(&self, totals: &Totals) -> String {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let mut out = String::new();
        let mut metric = |name: &str, kind: &str, help: &str, samples: &[(&str, f64)]| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for (labels, value) in samples {
                let _ = writeln!(out, "{name}{labels} {value}");
            }
        };
        metric(
            "wallet_accounts",
            "gauge",
            "Accounts on this server.",
            &[("", totals.accounts as f64)],
        );
        metric(
            "wallet_frozen_accounts",
            "gauge",
            "Frozen accounts.",
            &[("", totals.frozen_accounts as f64)],
        );
        metric(
            "wallet_liabilities_msat",
            "gauge",
            "Sum of all account balances.",
            &[("", totals.liabilities_msat as f64)],
        );
        if self.node_balance_known.load(Ordering::Relaxed) {
            let channel = load(&self.node_channel_local_msat);
            metric(
                "wallet_node_channel_local_msat",
                "gauge",
                "Local channel balance of the LND node.",
                &[("", channel as f64)],
            );
        }
        metric(
            "wallet_pending_payments",
            "gauge",
            "Outgoing payments waiting for a final status.",
            &[("", totals.pending_payments as f64)],
        );
        metric(
            "wallet_open_invoices",
            "gauge",
            "Unpaid invoices.",
            &[("", totals.open_invoices as f64)],
        );
        metric(
            "wallet_faucet_last_day_msat",
            "gauge",
            "Faucet credits in the last 24 hours.",
            &[("", totals.faucet_last_day_msat as f64)],
        );
        metric(
            "wallet_payments_total",
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
            "wallet_invoices_settled_total",
            "counter",
            "Invoices paid over Lightning since start.",
            &[
                ("{source=\"lnurl\"}", load(&self.invoices_settled_lnurl) as f64),
                ("{source=\"wallet\"}", load(&self.invoices_settled_wallet) as f64),
            ],
        );
        metric(
            "wallet_faucet_grants_total",
            "counter",
            "Faucet grants since start.",
            &[("", load(&self.faucet_grants) as f64)],
        );
        metric(
            "wallet_signups_total",
            "counter",
            "Accounts created since start.",
            &[("", load(&self.signups) as f64)],
        );
        metric(
            "wallet_login_failures_total",
            "counter",
            "Failed logins since start.",
            &[("", load(&self.login_failures) as f64)],
        );
        metric(
            "wallet_rate_limited_total",
            "counter",
            "Requests refused by rate limits since start.",
            &[("", load(&self.rate_limited) as f64)],
        );
        metric(
            "wallet_invoice_stream_up",
            "gauge",
            "Whether the LND invoice subscription is connected.",
            &[("", f64::from(u8::from(self.invoice_stream_up.load(Ordering::Relaxed))))],
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
        let text = metrics.render(&totals);
        assert!(text.contains("wallet_accounts 2\n"));
        assert!(text.contains("wallet_liabilities_msat 21000\n"));
        assert!(text.contains("wallet_node_channel_local_msat 50000\n"));
        assert!(text.contains("wallet_payments_total{kind=\"lightning\",outcome=\"succeeded\"} 1\n"));
        assert!(!text.contains("username"));
    }
}
