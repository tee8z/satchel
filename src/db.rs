//! SQLite storage. All writes go through one connection, so balance checks and
//! the inserts that depend on them never interleave; readers use a separate pool.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};

use crate::util::now;

static MIGRATOR: Migrator = sqlx::migrate!();

#[derive(Clone, Debug)]
pub(crate) struct Db {
    pub(crate) read: SqlitePool,
    pub(crate) write: SqlitePool,
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct Account {
    pub(crate) id: i64,
    pub(crate) username: String,
    pub(crate) password_hash: Option<String>,
    pub(crate) nostr_pubkey: Option<String>,
    pub(crate) frozen: bool,
    pub(crate) created_at: i64,
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct Session {
    pub(crate) account_id: Option<i64>,
    pub(crate) csrf_token: String,
}

/// One row of the operator's account list.
#[derive(Clone, Debug, FromRow)]
pub(crate) struct AccountSummary {
    pub(crate) id: i64,
    pub(crate) username: String,
    pub(crate) has_password: bool,
    pub(crate) has_nostr: bool,
    pub(crate) frozen: bool,
    pub(crate) created_at: i64,
    pub(crate) balance_msat: i64,
}

/// Totals for the operator page and the metrics endpoint.
#[derive(Clone, Debug, Default, FromRow)]
pub(crate) struct Totals {
    pub(crate) accounts: i64,
    pub(crate) frozen_accounts: i64,
    pub(crate) liabilities_msat: i64,
    pub(crate) pending_payments: i64,
    pub(crate) open_invoices: i64,
    pub(crate) faucet_last_day_msat: i64,
}

impl Db {
    pub(crate) async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).context("cannot create the database directory")?;
        }
        let write = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(10)),
            )
            .await
            .context("cannot open the database")?;
        MIGRATOR.run(&write).await.context("cannot migrate the database")?;
        let read = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .read_only(true)
                    .pragma("query_only", "ON")
                    .busy_timeout(Duration::from_secs(10)),
            )
            .await
            .context("cannot open database readers")?;
        Ok(Self { read, write })
    }

    /// Whether the database answers a trivial query, for `/healthz`.
    pub(crate) async fn ping(&self) -> bool {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&self.read)
            .await
            .is_ok()
    }

    /// A write transaction that holds the write lock from its first statement.
    pub(crate) async fn begin(&self) -> Result<Transaction<'static, Sqlite>, sqlx::Error> {
        self.write.begin_with("BEGIN IMMEDIATE").await
    }

    // ---- accounts ----

    pub(crate) async fn create_account(
        &self,
        username: &str,
        password_hash: Option<&str>,
        nostr_pubkey: Option<&str>,
    ) -> Result<Account, sqlx::Error> {
        sqlx::query_as::<_, Account>(
            "INSERT INTO accounts (username, password_hash, nostr_pubkey, created_at) VALUES (?, ?, ?, ?) \
             RETURNING id, username, password_hash, nostr_pubkey, frozen, created_at",
        )
        .bind(username)
        .bind(password_hash)
        .bind(nostr_pubkey)
        .bind(now())
        .fetch_one(&self.write)
        .await
    }

    pub(crate) async fn account(&self, id: i64) -> Result<Option<Account>, sqlx::Error> {
        sqlx::query_as::<_, Account>(
            "SELECT id, username, password_hash, nostr_pubkey, frozen, created_at FROM accounts WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.read)
        .await
    }

    pub(crate) async fn account_by_username(&self, username: &str) -> Result<Option<Account>, sqlx::Error> {
        sqlx::query_as::<_, Account>(
            "SELECT id, username, password_hash, nostr_pubkey, frozen, created_at FROM accounts WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.read)
        .await
    }

    pub(crate) async fn account_by_nostr(&self, pubkey: &str) -> Result<Option<Account>, sqlx::Error> {
        sqlx::query_as::<_, Account>(
            "SELECT id, username, password_hash, nostr_pubkey, frozen, created_at FROM accounts WHERE nostr_pubkey = ?",
        )
        .bind(pubkey)
        .fetch_optional(&self.read)
        .await
    }

    pub(crate) async fn set_password(&self, account_id: i64, password_hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE accounts SET password_hash = ? WHERE id = ?")
            .bind(password_hash)
            .bind(account_id)
            .execute(&self.write)
            .await?;
        Ok(())
    }

    /// Links or (with `None`) unlinks a Nostr key. Unlinking needs a password,
    /// which the table's CHECK constraint enforces.
    pub(crate) async fn set_nostr(&self, account_id: i64, pubkey: Option<&str>) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE accounts SET nostr_pubkey = ? WHERE id = ?")
            .bind(pubkey)
            .bind(account_id)
            .execute(&self.write)
            .await?;
        Ok(())
    }

    pub(crate) async fn set_frozen(&self, account_id: i64, frozen: bool) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("UPDATE accounts SET frozen = ? WHERE id = ?")
            .bind(frozen)
            .bind(account_id)
            .execute(&self.write)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    // ---- sessions ----

    pub(crate) async fn create_session(
        &self,
        token_hash: &str,
        account_id: Option<i64>,
        csrf_token: &str,
        ttl_secs: i64,
    ) -> Result<(), sqlx::Error> {
        let now = now();
        let mut tx = self.begin().await?;
        sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO sessions (token_hash, account_id, csrf_token, created_at, expires_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(token_hash)
        .bind(account_id)
        .bind(csrf_token)
        .bind(now)
        .bind(now + ttl_secs)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    pub(crate) async fn session(&self, token_hash: &str) -> Result<Option<Session>, sqlx::Error> {
        sqlx::query_as::<_, Session>(
            "SELECT account_id, csrf_token FROM sessions WHERE token_hash = ? AND expires_at > ?",
        )
        .bind(token_hash)
        .bind(now())
        .fetch_optional(&self.read)
        .await
    }

    pub(crate) async fn delete_session(&self, token_hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.write)
            .await?;
        Ok(())
    }

    /// Signs the account out everywhere except the given session (after a password change).
    pub(crate) async fn delete_other_sessions(&self, account_id: i64, keep: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE account_id = ? AND token_hash <> ?")
            .bind(account_id)
            .bind(keep)
            .execute(&self.write)
            .await?;
        Ok(())
    }

    // ---- cursors ----

    pub(crate) async fn cursor(&self, name: &str) -> Result<i64, sqlx::Error> {
        let value = sqlx::query_scalar::<_, i64>("SELECT value FROM cursors WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.read)
            .await?;
        Ok(value.unwrap_or(0))
    }

    /// Moves a cursor forward; it never goes back.
    pub(crate) async fn advance_cursor(&self, name: &str, value: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO cursors (name, value) VALUES (?, ?) \
             ON CONFLICT (name) DO UPDATE SET value = MAX(value, excluded.value)",
        )
        .bind(name)
        .bind(value)
        .execute(&self.write)
        .await?;
        Ok(())
    }

    // ---- operator views ----

    pub(crate) async fn totals(&self) -> Result<Totals, sqlx::Error> {
        sqlx::query_as::<_, Totals>(
            "SELECT \
               (SELECT COUNT(*) FROM accounts) AS accounts, \
               (SELECT COUNT(*) FROM accounts WHERE frozen = 1) AS frozen_accounts, \
               (SELECT COALESCE(SUM(amount_msat), 0) FROM ledger) AS liabilities_msat, \
               (SELECT COUNT(*) FROM payments WHERE status = 'pending') AS pending_payments, \
               (SELECT COUNT(*) FROM invoices WHERE state = 'open') AS open_invoices, \
               (SELECT COALESCE(SUM(amount_msat), 0) FROM payments WHERE kind = 'faucet' AND created_at > ?) \
                 AS faucet_last_day_msat",
        )
        .bind(now() - 86_400)
        .fetch_one(&self.read)
        .await
    }

    /// Accounts by balance, optionally filtered by a username substring.
    pub(crate) async fn account_summaries(&self, search: &str, limit: i64) -> Result<Vec<AccountSummary>, sqlx::Error> {
        sqlx::query_as::<_, AccountSummary>(
            "SELECT a.id, a.username, a.password_hash IS NOT NULL AS has_password, \
               a.nostr_pubkey IS NOT NULL AS has_nostr, a.frozen, a.created_at, \
               COALESCE((SELECT SUM(l.amount_msat) FROM ledger l WHERE l.account_id = a.id), 0) AS balance_msat \
             FROM accounts a WHERE ? = '' OR instr(a.username, ?) > 0 \
             ORDER BY balance_msat DESC, a.id LIMIT ?",
        )
        .bind(search)
        .bind(search)
        .bind(limit)
        .fetch_all(&self.read)
        .await
    }
}

#[cfg(test)]
pub(crate) async fn test_db() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("wallet.db")).await.unwrap();
    (db, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accounts_need_a_login_method_and_unique_names() {
        let (db, _dir) = test_db().await;
        assert!(db.create_account("alice", None, None).await.is_err());
        let alice = db.create_account("alice", Some("hash"), None).await.unwrap();
        assert_eq!(alice.username, "alice");
        assert!(!alice.frozen);
        let duplicate = db.create_account("alice", Some("hash"), None).await.unwrap_err();
        assert!(duplicate.as_database_error().unwrap().is_unique_violation());
        let bob = db
            .create_account("bob", None, Some("ab".repeat(32).as_str()))
            .await
            .unwrap();
        // Bob has no password, so his Nostr key cannot be unlinked.
        assert!(db.set_nostr(bob.id, None).await.is_err());
        assert!(db.set_frozen(bob.id, true).await.unwrap());
        assert!(db.account(bob.id).await.unwrap().unwrap().frozen);
    }

    #[tokio::test]
    async fn sessions_expire_and_cursors_only_advance() {
        let (db, _dir) = test_db().await;
        let alice = db.create_account("alice", Some("hash"), None).await.unwrap();
        db.create_session("live", Some(alice.id), "csrf", 60).await.unwrap();
        db.create_session("dead", Some(alice.id), "csrf", -1).await.unwrap();
        assert_eq!(db.session("live").await.unwrap().unwrap().account_id, Some(alice.id));
        assert!(db.session("dead").await.unwrap().is_none());
        db.advance_cursor("settle_index", 7).await.unwrap();
        db.advance_cursor("settle_index", 3).await.unwrap();
        assert_eq!(db.cursor("settle_index").await.unwrap(), 7);
    }
}
