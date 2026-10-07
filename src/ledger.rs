//! Money movement. Each balance change is an append-only ledger entry with a
//! unique idempotency key, written in the same transaction as the payment row
//! it belongs to. SQLite triggers reject updates, deletes, and any entry that
//! would make a balance negative; the single write connection serialises the
//! checks below with the inserts that depend on them.

use sqlx::{FromRow, Sqlite, Transaction};

use crate::db::{Account, Db};
use crate::error::WalletError;
use crate::util::now;

/// Columns for [`Payment`], followed by the rest of the query.
macro_rules! select_payment {
    ($rest:literal) => {
        concat!(
            "SELECT id, direction, kind, status, amount_msat, fee_msat, fee_limit_msat, payment_hash, \
             counterparty, memo, failure, created_at FROM payments ",
            $rest
        )
    };
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct Payment {
    pub(crate) id: i64,
    pub(crate) direction: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) amount_msat: i64,
    pub(crate) fee_msat: i64,
    pub(crate) fee_limit_msat: i64,
    pub(crate) payment_hash: Option<String>,
    pub(crate) counterparty: String,
    pub(crate) memo: String,
    pub(crate) failure: Option<String>,
    pub(crate) created_at: i64,
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct Invoice {
    pub(crate) payment_hash: String,
    pub(crate) account_id: i64,
    pub(crate) bolt11: String,
    pub(crate) amount_msat: i64,
    pub(crate) memo: String,
    pub(crate) source: String,
    pub(crate) state: String,
    pub(crate) expires_at: i64,
}

pub(crate) struct NewInvoice<'a> {
    pub(crate) payment_hash: &'a str,
    pub(crate) account_id: i64,
    pub(crate) bolt11: &'a str,
    pub(crate) amount_msat: i64,
    pub(crate) memo: &'a str,
    pub(crate) source: &'static str,
    pub(crate) expires_at: i64,
}

/// An outgoing Lightning payment before LND sees it.
pub(crate) struct NewSend<'a> {
    pub(crate) account_id: i64,
    pub(crate) request_key: &'a str,
    pub(crate) amount_msat: i64,
    pub(crate) fee_limit_msat: i64,
    pub(crate) payment_hash: &'a str,
    pub(crate) counterparty: &'a str,
    pub(crate) memo: &'a str,
}

#[derive(Debug)]
pub(crate) enum Started {
    New(i64),
    /// The same request key was used before: nothing new happened.
    Existing(Box<Payment>),
}

#[derive(Debug, Clone)]
pub(crate) enum SendOutcome {
    Succeeded { fee_msat: i64 },
    Failed { reason: String },
}

#[derive(Debug, Clone, FromRow)]
pub(crate) struct PendingSend {
    pub(crate) id: i64,
    pub(crate) payment_hash: String,
}

/// A credit that does not come from Lightning: faucet or operator grants.
pub(crate) struct Grant<'a> {
    pub(crate) account_id: i64,
    pub(crate) kind: &'static str,
    pub(crate) amount_msat: i64,
    pub(crate) request_key: &'a str,
    pub(crate) memo: &'a str,
    pub(crate) max_balance_msat: i64,
    /// Rolling 24-hour faucet limits: (per account, global).
    pub(crate) daily_limits: Option<(i64, i64)>,
}

pub(crate) struct Transfer<'a> {
    pub(crate) sender: &'a Account,
    pub(crate) recipient: &'a Account,
    pub(crate) amount_msat: i64,
    pub(crate) request_key: &'a str,
    pub(crate) memo: &'a str,
    pub(crate) max_balance_msat: i64,
}

struct NewPayment<'a> {
    account_id: i64,
    direction: &'static str,
    kind: &'static str,
    status: &'static str,
    amount_msat: i64,
    fee_limit_msat: i64,
    payment_hash: Option<&'a str>,
    request_key: Option<&'a str>,
    counterparty: &'a str,
    memo: &'a str,
}

type Tx = Transaction<'static, Sqlite>;

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.is_unique_violation())
}

async fn insert_payment(tx: &mut Tx, payment: &NewPayment<'_>) -> Result<i64, sqlx::Error> {
    let now = now();
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO payments (account_id, direction, kind, status, amount_msat, fee_limit_msat, payment_hash, \
         request_key, counterparty, memo, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(payment.account_id)
    .bind(payment.direction)
    .bind(payment.kind)
    .bind(payment.status)
    .bind(payment.amount_msat)
    .bind(payment.fee_limit_msat)
    .bind(payment.payment_hash)
    .bind(payment.request_key)
    .bind(payment.counterparty)
    .bind(payment.memo)
    .bind(now)
    .bind(now)
    .fetch_one(&mut **tx)
    .await
}

/// Appends one ledger entry. The key makes a repeated entry fail instead of counting twice.
async fn insert_entry(
    tx: &mut Tx,
    account_id: i64,
    amount_msat: i64,
    kind: &'static str,
    payment_id: i64,
    key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ledger (account_id, amount_msat, kind, payment_id, idempotency_key, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(account_id)
    .bind(amount_msat)
    .bind(kind)
    .bind(payment_id)
    .bind(key)
    .bind(now())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn balance_in(tx: &mut Tx, account_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT COALESCE(SUM(amount_msat), 0) FROM ledger WHERE account_id = ?")
        .bind(account_id)
        .fetch_one(&mut **tx)
        .await
}

async fn payment_by_key(tx: &mut Tx, request_key: &str) -> Result<Option<Payment>, sqlx::Error> {
    sqlx::query_as::<_, Payment>(select_payment!("WHERE request_key = ?"))
        .bind(request_key)
        .fetch_optional(&mut **tx)
        .await
}

async fn payment_by_id(tx: &mut Tx, id: i64) -> Result<Payment, sqlx::Error> {
    sqlx::query_as::<_, Payment>(select_payment!("WHERE id = ?"))
        .bind(id)
        .fetch_one(&mut **tx)
        .await
}

/// Frozen or missing accounts cannot move money.
async fn ensure_active(tx: &mut Tx, account_id: i64) -> Result<(), WalletError> {
    let frozen = sqlx::query_scalar::<_, bool>("SELECT frozen FROM accounts WHERE id = ?")
        .bind(account_id)
        .fetch_optional(&mut **tx)
        .await?;
    match frozen {
        None => Err(WalletError::NotFound),
        Some(true) => Err(WalletError::Frozen),
        Some(false) => Ok(()),
    }
}

impl Db {
    pub(crate) async fn balance(&self, account_id: i64) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>("SELECT COALESCE(SUM(amount_msat), 0) FROM ledger WHERE account_id = ?")
            .bind(account_id)
            .fetch_one(&self.read)
            .await
    }

    pub(crate) async fn liabilities(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>("SELECT COALESCE(SUM(amount_msat), 0) FROM ledger")
            .fetch_one(&self.read)
            .await
    }

    pub(crate) async fn history(&self, account_id: i64, limit: i64) -> Result<Vec<Payment>, sqlx::Error> {
        sqlx::query_as::<_, Payment>(select_payment!("WHERE account_id = ? ORDER BY id DESC LIMIT ?"))
            .bind(account_id)
            .bind(limit)
            .fetch_all(&self.read)
            .await
    }

    pub(crate) async fn payment(&self, account_id: i64, id: i64) -> Result<Option<Payment>, sqlx::Error> {
        sqlx::query_as::<_, Payment>(select_payment!("WHERE account_id = ? AND id = ?"))
            .bind(account_id)
            .bind(id)
            .fetch_optional(&self.read)
            .await
    }

    pub(crate) async fn payment_by_request_key(&self, request_key: &str) -> Result<Option<Payment>, sqlx::Error> {
        sqlx::query_as::<_, Payment>(select_payment!("WHERE request_key = ?"))
            .bind(request_key)
            .fetch_optional(&self.read)
            .await
    }

    // ---- invoices ----

    pub(crate) async fn create_invoice(&self, invoice: &NewInvoice<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO invoices (payment_hash, account_id, bolt11, amount_msat, memo, source, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(invoice.payment_hash)
        .bind(invoice.account_id)
        .bind(invoice.bolt11)
        .bind(invoice.amount_msat)
        .bind(invoice.memo)
        .bind(invoice.source)
        .bind(now())
        .bind(invoice.expires_at)
        .execute(&self.write)
        .await?;
        Ok(())
    }

    pub(crate) async fn invoice(&self, payment_hash: &str) -> Result<Option<Invoice>, sqlx::Error> {
        sqlx::query_as::<_, Invoice>(
            "SELECT payment_hash, account_id, bolt11, amount_msat, memo, source, state, expires_at \
             FROM invoices WHERE payment_hash = ?",
        )
        .bind(payment_hash)
        .fetch_optional(&self.read)
        .await
    }

    /// Open invoices, oldest first, for reconciliation with LND.
    pub(crate) async fn open_invoices(&self, limit: i64) -> Result<Vec<Invoice>, sqlx::Error> {
        sqlx::query_as::<_, Invoice>(
            "SELECT payment_hash, account_id, bolt11, amount_msat, memo, source, state, expires_at \
             FROM invoices WHERE state = 'open' ORDER BY created_at LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.read)
        .await
    }

    pub(crate) async fn mark_invoice_canceled(&self, payment_hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE invoices SET state = 'canceled' WHERE payment_hash = ? AND state = 'open'")
            .bind(payment_hash)
            .execute(&self.write)
            .await?;
        Ok(())
    }

    /// Credits an invoice LND reports as settled, exactly once. Returns the
    /// invoice when this call credited it; `None` if it is not ours or was
    /// already credited (over Lightning or internally).
    pub(crate) async fn settle_invoice(
        &self,
        payment_hash: &str,
        amount_paid_msat: i64,
    ) -> Result<Option<Invoice>, WalletError> {
        if amount_paid_msat <= 0 {
            return Ok(None);
        }
        let mut tx = self.begin().await?;
        let invoice = sqlx::query_as::<_, Invoice>(
            "SELECT payment_hash, account_id, bolt11, amount_msat, memo, source, state, expires_at \
             FROM invoices WHERE payment_hash = ?",
        )
        .bind(payment_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(invoice) = invoice else {
            return Ok(None);
        };
        if invoice.state == "settled" {
            return Ok(None);
        }
        let now = now();
        sqlx::query(
            "UPDATE invoices SET state = 'settled', amount_paid_msat = ?, settled_at = ? \
             WHERE payment_hash = ? AND state <> 'settled'",
        )
        .bind(amount_paid_msat)
        .bind(now)
        .bind(payment_hash)
        .execute(&mut *tx)
        .await?;
        let payment = NewPayment {
            account_id: invoice.account_id,
            direction: "in",
            kind: "lightning",
            status: "succeeded",
            amount_msat: amount_paid_msat,
            fee_limit_msat: 0,
            payment_hash: Some(payment_hash),
            request_key: None,
            counterparty: "",
            memo: &invoice.memo,
        };
        let id = match insert_payment(&mut tx, &payment).await {
            Ok(id) => id,
            Err(error) if is_unique_violation(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        insert_entry(
            &mut tx,
            invoice.account_id,
            amount_paid_msat,
            "credit",
            id,
            &format!("payment:{id}:credit"),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(invoice))
    }

    // ---- outgoing Lightning payments ----

    /// Records a pending send and reserves its amount plus the fee budget.
    pub(crate) async fn begin_send(&self, send: &NewSend<'_>) -> Result<Started, WalletError> {
        let mut tx = self.begin().await?;
        if let Some(existing) = payment_by_key(&mut tx, send.request_key).await? {
            return Ok(Started::Existing(Box::new(existing)));
        }
        ensure_active(&mut tx, send.account_id).await?;
        let payment = NewPayment {
            account_id: send.account_id,
            direction: "out",
            kind: "lightning",
            status: "pending",
            amount_msat: send.amount_msat,
            fee_limit_msat: send.fee_limit_msat,
            payment_hash: Some(send.payment_hash),
            request_key: Some(send.request_key),
            counterparty: send.counterparty,
            memo: send.memo,
        };
        let id = match insert_payment(&mut tx, &payment).await {
            Ok(id) => id,
            Err(error) if is_unique_violation(&error) => return Err(WalletError::AlreadyPaid),
            Err(error) => return Err(error.into()),
        };
        let reserved = send.amount_msat + send.fee_limit_msat;
        insert_entry(
            &mut tx,
            send.account_id,
            -reserved,
            "debit",
            id,
            &format!("payment:{id}:debit"),
        )
        .await?;
        tx.commit().await?;
        Ok(Started::New(id))
    }

    /// Settles a pending send once: unused fee budget comes back on success,
    /// everything comes back on failure. Returns whether this call changed it.
    pub(crate) async fn finish_send(&self, id: i64, outcome: &SendOutcome) -> Result<bool, WalletError> {
        let mut tx = self.begin().await?;
        let row = sqlx::query_as::<_, (i64, i64, i64, String)>(
            "SELECT account_id, amount_msat, fee_limit_msat, status FROM payments WHERE id = ? AND direction = 'out'",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((account_id, amount_msat, fee_limit_msat, status)) = row else {
            return Ok(false);
        };
        if status != "pending" {
            return Ok(false);
        }
        match outcome {
            SendOutcome::Succeeded { fee_msat } => {
                // LND enforces the fee limit; never charge more than was reserved.
                let fee = (*fee_msat).clamp(0, fee_limit_msat);
                sqlx::query("UPDATE payments SET status = 'succeeded', fee_msat = ?, updated_at = ? WHERE id = ?")
                    .bind(fee)
                    .bind(now())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                if fee_limit_msat > fee {
                    let key = format!("payment:{id}:fee-refund");
                    insert_entry(&mut tx, account_id, fee_limit_msat - fee, "refund", id, &key).await?;
                }
            }
            SendOutcome::Failed { reason } => {
                sqlx::query("UPDATE payments SET status = 'failed', failure = ?, updated_at = ? WHERE id = ?")
                    .bind(reason)
                    .bind(now())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                let key = format!("payment:{id}:refund");
                insert_entry(&mut tx, account_id, amount_msat + fee_limit_msat, "refund", id, &key).await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn pending_sends(&self, created_before: i64) -> Result<Vec<PendingSend>, sqlx::Error> {
        sqlx::query_as::<_, PendingSend>(
            "SELECT id, payment_hash FROM payments \
             WHERE status = 'pending' AND direction = 'out' AND kind = 'lightning' \
             AND payment_hash IS NOT NULL AND created_at < ? ORDER BY id",
        )
        .bind(created_before)
        .fetch_all(&self.read)
        .await
    }

    // ---- payments inside this server ----

    /// Pays an open invoice of another account without touching Lightning.
    /// The caller cancels the invoice in LND first, so it cannot also be paid there.
    pub(crate) async fn pay_invoice_internally(
        &self,
        sender: &Account,
        request_key: &str,
        invoice: &Invoice,
        recipient: &str,
    ) -> Result<Payment, WalletError> {
        let mut tx = self.begin().await?;
        if let Some(existing) = payment_by_key(&mut tx, request_key).await? {
            return Ok(existing);
        }
        ensure_active(&mut tx, sender.id).await?;
        let now = now();
        let updated = sqlx::query(
            "UPDATE invoices SET state = 'settled', amount_paid_msat = amount_msat, settled_at = ? \
             WHERE payment_hash = ? AND state = 'open' AND expires_at > ?",
        )
        .bind(now)
        .bind(&invoice.payment_hash)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(WalletError::invalid("This invoice is no longer open."));
        }
        let out = NewPayment {
            account_id: sender.id,
            direction: "out",
            kind: "internal",
            status: "succeeded",
            amount_msat: invoice.amount_msat,
            fee_limit_msat: 0,
            payment_hash: Some(&invoice.payment_hash),
            request_key: Some(request_key),
            counterparty: recipient,
            memo: &invoice.memo,
        };
        let out_id = insert_payment(&mut tx, &out).await?;
        let key = format!("payment:{out_id}:debit");
        insert_entry(&mut tx, sender.id, -invoice.amount_msat, "debit", out_id, &key).await?;
        let incoming = NewPayment {
            account_id: invoice.account_id,
            direction: "in",
            kind: "internal",
            counterparty: &sender.username,
            request_key: None,
            ..out
        };
        let in_id = insert_payment(&mut tx, &incoming).await?;
        let key = format!("payment:{in_id}:credit");
        insert_entry(&mut tx, invoice.account_id, invoice.amount_msat, "credit", in_id, &key).await?;
        let payment = payment_by_id(&mut tx, out_id).await?;
        tx.commit().await?;
        Ok(payment)
    }

    /// Moves sats between two accounts of this server (Lightning Address on our own domain).
    pub(crate) async fn transfer(&self, transfer: &Transfer<'_>) -> Result<Payment, WalletError> {
        let (sender, recipient) = (transfer.sender, transfer.recipient);
        if sender.id == recipient.id {
            return Err(WalletError::invalid("You cannot pay yourself."));
        }
        let mut tx = self.begin().await?;
        if let Some(existing) = payment_by_key(&mut tx, transfer.request_key).await? {
            return Ok(existing);
        }
        ensure_active(&mut tx, sender.id).await?;
        if ensure_active(&mut tx, recipient.id).await.is_err() {
            return Err(WalletError::invalid("That account cannot receive payments right now."));
        }
        if balance_in(&mut tx, recipient.id).await? + transfer.amount_msat > transfer.max_balance_msat {
            return Err(WalletError::limit("That account cannot hold that many more sats."));
        }
        let out = NewPayment {
            account_id: sender.id,
            direction: "out",
            kind: "internal",
            status: "succeeded",
            amount_msat: transfer.amount_msat,
            fee_limit_msat: 0,
            payment_hash: None,
            request_key: Some(transfer.request_key),
            counterparty: &recipient.username,
            memo: transfer.memo,
        };
        let out_id = insert_payment(&mut tx, &out).await?;
        let key = format!("payment:{out_id}:debit");
        insert_entry(&mut tx, sender.id, -transfer.amount_msat, "debit", out_id, &key).await?;
        let incoming = NewPayment {
            account_id: recipient.id,
            direction: "in",
            counterparty: &sender.username,
            request_key: None,
            ..out
        };
        let in_id = insert_payment(&mut tx, &incoming).await?;
        let key = format!("payment:{in_id}:credit");
        insert_entry(&mut tx, recipient.id, transfer.amount_msat, "credit", in_id, &key).await?;
        let payment = payment_by_id(&mut tx, out_id).await?;
        tx.commit().await?;
        Ok(payment)
    }

    /// Faucet or operator credit, within the balance cap and any daily limits.
    pub(crate) async fn grant(&self, grant: &Grant<'_>) -> Result<Payment, WalletError> {
        let mut tx = self.begin().await?;
        if let Some(existing) = payment_by_key(&mut tx, grant.request_key).await? {
            return Ok(existing);
        }
        ensure_active(&mut tx, grant.account_id).await?;
        if let Some((per_account, global)) = grant.daily_limits {
            let since = now() - 86_400;
            let (mine, everyone) = sqlx::query_as::<_, (i64, i64)>(
                "SELECT COALESCE(SUM(CASE WHEN account_id = ? THEN amount_msat ELSE 0 END), 0), \
                 COALESCE(SUM(amount_msat), 0) FROM payments WHERE kind = 'faucet' AND created_at > ?",
            )
            .bind(grant.account_id)
            .bind(since)
            .fetch_one(&mut *tx)
            .await?;
            if mine + grant.amount_msat > per_account {
                return Err(WalletError::limit(
                    "You have used today's faucet allowance. Try again tomorrow.",
                ));
            }
            if everyone + grant.amount_msat > global {
                return Err(WalletError::limit(
                    "The faucet has given out today's sats. Try again tomorrow.",
                ));
            }
        }
        if balance_in(&mut tx, grant.account_id).await? + grant.amount_msat > grant.max_balance_msat {
            return Err(WalletError::limit("This wallet is at its balance limit."));
        }
        let payment = NewPayment {
            account_id: grant.account_id,
            direction: "in",
            kind: grant.kind,
            status: "succeeded",
            amount_msat: grant.amount_msat,
            fee_limit_msat: 0,
            payment_hash: None,
            request_key: Some(grant.request_key),
            counterparty: "",
            memo: grant.memo,
        };
        let id = insert_payment(&mut tx, &payment).await?;
        insert_entry(
            &mut tx,
            grant.account_id,
            grant.amount_msat,
            "credit",
            id,
            &format!("payment:{id}:credit"),
        )
        .await?;
        let payment = payment_by_id(&mut tx, id).await?;
        tx.commit().await?;
        Ok(payment)
    }
}
