//! Activity per client address: where accounts were created, who claimed the
//! faucet, and who asked for LNURL invoices. It feeds the per-address faucet
//! cap and the operator's busiest-addresses view. Also what freezing an
//! account does beyond the flag.

use sqlx::FromRow;
use tracing::warn;

use crate::db::Db;
use crate::util::now;
use crate::wallet::Wallet;

pub(crate) const SIGNUP: &str = "signup";
pub(crate) const FAUCET: &str = "faucet";
pub(crate) const LNURL_INVOICE: &str = "lnurl_invoice";

/// Client events are kept this long; the operator view and the faucet cap look back a day.
const EVENT_RETENTION_SECS: i64 = 7 * 86_400;

/// One row of the busiest-addresses view.
#[derive(Clone, Debug, FromRow)]
pub(crate) struct BusyClient {
    pub(crate) client: String,
    pub(crate) events: i64,
    pub(crate) accounts: i64,
    pub(crate) amount_msat: i64,
    pub(crate) last_at: i64,
}

impl Db {
    /// Accounts created since `since`, across the whole service.
    pub(crate) async fn accounts_created_since(&self, since: i64) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM accounts WHERE created_at > ?")
            .bind(since)
            .fetch_one(&self.read)
            .await
    }

    /// Notes the client key an account was created from.
    pub(crate) async fn record_signup(&self, account_id: i64, client: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.begin().await?;
        sqlx::query("UPDATE accounts SET signup_client = ? WHERE id = ?")
            .bind(client)
            .bind(account_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO client_events (kind, client, account_id, created_at) VALUES (?, ?, ?, ?)")
            .bind(SIGNUP)
            .bind(client)
            .bind(account_id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    pub(crate) async fn record_client_event(
        &self,
        kind: &str,
        client: &str,
        account_id: i64,
        amount_msat: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO client_events (kind, client, account_id, amount_msat, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(kind)
        .bind(client)
        .bind(account_id)
        .bind(amount_msat)
        .bind(now())
        .execute(&self.write)
        .await?;
        Ok(())
    }

    /// The client keys with the most events of a kind since `since`.
    pub(crate) async fn busiest_clients(
        &self,
        kind: &str,
        since: i64,
        limit: i64,
    ) -> Result<Vec<BusyClient>, sqlx::Error> {
        sqlx::query_as::<_, BusyClient>(
            "SELECT client, COUNT(*) AS events, COUNT(DISTINCT account_id) AS accounts, \
               COALESCE(SUM(amount_msat), 0) AS amount_msat, MAX(created_at) AS last_at \
             FROM client_events WHERE kind = ? AND created_at > ? \
             GROUP BY client ORDER BY events DESC, last_at DESC LIMIT ?",
        )
        .bind(kind)
        .bind(since)
        .bind(limit)
        .fetch_all(&self.read)
        .await
    }

    pub(crate) async fn prune_client_events(&self, now: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM client_events WHERE created_at < ?")
            .bind(now - EVENT_RETENTION_SECS)
            .execute(&self.write)
            .await?;
        Ok(())
    }

    /// Unpaid, unexpired invoices an account holds from one source (`wallet` or `lnurl`).
    pub(crate) async fn open_invoice_count(&self, account_id: i64, source: &str) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM invoices WHERE account_id = ? AND source = ? AND state = 'open' AND expires_at > ?",
        )
        .bind(account_id)
        .bind(source)
        .bind(now())
        .fetch_one(&self.read)
        .await
    }

    /// Payment hashes of an account's open invoices.
    async fn open_invoice_hashes(&self, account_id: i64) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar::<_, String>("SELECT payment_hash FROM invoices WHERE account_id = ? AND state = 'open'")
            .bind(account_id)
            .fetch_all(&self.read)
            .await
    }

    /// Signs an account out everywhere.
    pub(crate) async fn delete_sessions(&self, account_id: i64) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE account_id = ?")
            .bind(account_id)
            .execute(&self.write)
            .await?;
        Ok(())
    }
}

/// After the operator freezes an account: sign it out everywhere and cancel
/// its unpaid invoices, so nothing more arrives. An invoice LND settles anyway
/// (a payment already in flight) is still credited: the sats reached the node.
pub(crate) async fn after_freeze(wallet: &Wallet, account_id: i64) -> Result<(), sqlx::Error> {
    wallet.db.delete_sessions(account_id).await?;
    for payment_hash in wallet.db.open_invoice_hashes(account_id).await? {
        match wallet.lnd.cancel_invoice(payment_hash.clone()).await {
            Ok(()) => wallet.db.mark_invoice_canceled(&payment_hash).await?,
            Err(error) => warn!(%error, %payment_hash, "cannot cancel a frozen account's invoice"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn busiest_clients_rank_by_events() {
        let (db, _dir) = test_db().await;
        let alice = db.create_account("alice", Some("hash"), None).await.unwrap();
        let bob = db.create_account("bob", Some("hash"), None).await.unwrap();
        db.record_signup(alice.id, "203.0.113.7").await.unwrap();
        db.record_signup(bob.id, "203.0.113.7").await.unwrap();
        db.record_client_event(FAUCET, "2001:db8:1::/56", alice.id, 10_000_000)
            .await
            .unwrap();
        let signups = db.busiest_clients(SIGNUP, now() - 86_400, 10).await.unwrap();
        assert_eq!(signups.len(), 1);
        assert_eq!(
            (signups[0].client.as_str(), signups[0].events, signups[0].accounts),
            ("203.0.113.7", 2, 2)
        );
        let faucet = db.busiest_clients(FAUCET, now() - 86_400, 10).await.unwrap();
        assert_eq!(faucet[0].amount_msat, 10_000_000);
        assert_eq!(db.accounts_created_since(now() - 3600).await.unwrap(), 2);
        db.prune_client_events(now() + EVENT_RETENTION_SECS + 1).await.unwrap();
        assert!(db.busiest_clients(SIGNUP, 0, 10).await.unwrap().is_empty());
    }
}
