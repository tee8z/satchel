//! Handoff sign-in: another app signs a NIP-98 event with the user's Nostr key
//! and posts it here in a top-level form, so one click opens the user's wallet.
//! Also where any sign-in goes next (`next`), which must stay on this site.

use sqlx::FromRow;

use crate::auth;
use crate::db::Db;
use crate::nostr;
use crate::util::{now, random_token};

/// Where sign-ins go when `next` is missing or unsafe.
pub(crate) const DEFAULT_NEXT: &str = "/wallet";
/// Longest `next` kept. BOLT11 invoices with route hints can pass a kilobyte.
const MAX_NEXT_LEN: usize = 2048;
/// How long a used handoff event id is remembered: far past the ±120 s window.
const EVENT_MEMORY_SECS: i64 = 600;
/// How long the redirect, confirmation, and create-wallet steps stay usable.
pub(crate) const PENDING_TTL_SECS: i64 = 600;

/// A local path to open after signing in, or [`DEFAULT_NEXT`]: one leading `/`,
/// no `//` anywhere (so never another host), printable ASCII without `\`
/// (browsers drop tabs and newlines, and read `\` as `/`), and length-capped.
pub(crate) fn safe_next(next: &str) -> String {
    let local = next.len() <= MAX_NEXT_LEN
        && next.starts_with('/')
        && !next.contains("//")
        && next.bytes().all(|byte| byte.is_ascii_graphic() && byte != b'\\');
    if local {
        next.to_owned()
    } else {
        DEFAULT_NEXT.to_owned()
    }
}

/// `npub1abcdefg…uvwxyz`, enough to recognise a key.
pub(crate) fn short_npub(pubkey_hex: &str) -> String {
    let npub = nostr::npub(pubkey_hex);
    if npub.len() <= 20 || !npub.is_ascii() {
        return npub;
    }
    format!("{}…{}", &npub[..12], &npub[npub.len() - 6..])
}

/// What a pending handoff leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// Sign in to the wallet that uses the key.
    SignIn,
    /// Create a wallet for a key no wallet uses yet.
    SignUp,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Self::SignIn => "signin",
            Self::SignUp => "signup",
        }
    }
}

/// A verified handoff waiting for its next step.
#[derive(Debug, Clone, FromRow)]
pub(crate) struct Pending {
    pub(crate) nostr_pubkey: String,
    pub(crate) next: String,
    /// Posted from an origin in `server.handoff_origins`.
    pub(crate) trusted: bool,
}

impl Db {
    /// Records a handoff event id; false when it was used before.
    pub(crate) async fn use_handoff_event(&self, event_id: &str) -> Result<bool, sqlx::Error> {
        let now = now();
        let mut tx = self.begin().await?;
        sqlx::query("DELETE FROM handoff_events WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let inserted = sqlx::query(
            "INSERT INTO handoff_events (event_id, expires_at) VALUES (?, ?) ON CONFLICT (event_id) DO NOTHING",
        )
        .bind(event_id)
        .bind(now + EVENT_MEMORY_SECS)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(inserted)
    }

    /// Stores a verified handoff behind a new random token and returns the token.
    pub(crate) async fn create_pending_handoff(
        &self,
        purpose: Purpose,
        nostr_pubkey: &str,
        next: &str,
        trusted: bool,
    ) -> Result<String, sqlx::Error> {
        let token = random_token();
        let now = now();
        let mut tx = self.begin().await?;
        sqlx::query("DELETE FROM handoff_pending WHERE expires_at <= ?")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO handoff_pending (token_hash, purpose, nostr_pubkey, next, trusted, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(auth::token_hash(&token))
        .bind(purpose.as_str())
        .bind(nostr_pubkey)
        .bind(next)
        .bind(trusted)
        .bind(now + PENDING_TTL_SECS)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(token)
    }

    /// The pending handoff behind a token, if it has not expired.
    pub(crate) async fn pending_handoff(&self, token: &str, purpose: Purpose) -> Result<Option<Pending>, sqlx::Error> {
        sqlx::query_as::<_, Pending>(
            "SELECT nostr_pubkey, next, trusted FROM handoff_pending \
             WHERE token_hash = ? AND purpose = ? AND expires_at > ?",
        )
        .bind(auth::token_hash(token))
        .bind(purpose.as_str())
        .bind(now())
        .fetch_optional(&self.read)
        .await
    }

    /// Uses up a pending handoff: it answers once.
    pub(crate) async fn take_pending_handoff(
        &self,
        token: &str,
        purpose: Purpose,
    ) -> Result<Option<Pending>, sqlx::Error> {
        sqlx::query_as::<_, Pending>(
            "DELETE FROM handoff_pending WHERE token_hash = ? AND purpose = ? AND expires_at > ? \
             RETURNING nostr_pubkey, next, trusted",
        )
        .bind(auth::token_hash(token))
        .bind(purpose.as_str())
        .bind(now())
        .fetch_optional(&self.write)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[test]
    fn next_stays_on_this_site() {
        let invoice = format!("/launch/lightning/lntbs10u1p{}", "q".repeat(1500));
        for good in [
            "/wallet",
            "/",
            "/settings?tab=nostr",
            "/launch/lightning/alice@wallet.example.org",
            "/launch/lightning/lightning:LNTBS10U1PTEST",
            "/launch/lightning/alice%40wallet.example.org",
            invoice.as_str(),
        ] {
            assert_eq!(safe_next(good), good, "{good} is local");
        }
        let overlong = format!("/{}", "a".repeat(MAX_NEXT_LEN));
        for bad in [
            "",
            "wallet",
            "//evil.example",
            "///evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "https://evil.example",
            "javascript:alert(1)",
            "/redirect//evil.example",
            "/\t/evil.example",
            "/\n/evil.example",
            "/ /evil.example",
            "/wallet\r\nSet-Cookie: x=1",
            "/caf\u{e9}",
            overlong.as_str(),
        ] {
            assert_eq!(safe_next(bad), DEFAULT_NEXT, "{bad:?} must fall back");
        }
    }

    #[test]
    fn short_npubs_keep_both_ends() {
        let pubkey = "ab".repeat(32);
        let full = nostr::npub(&pubkey);
        let short = short_npub(&pubkey);
        assert!(short.starts_with(&full[..12]) && short.ends_with(&full[full.len() - 6..]));
        assert!(short.chars().count() < full.len());
    }

    #[tokio::test]
    async fn events_are_used_once_and_pending_handoffs_answer_once() {
        let (db, _dir) = test_db().await;
        assert!(db.use_handoff_event("event-1").await.unwrap());
        assert!(!db.use_handoff_event("event-1").await.unwrap(), "replays are refused");
        assert!(db.use_handoff_event("event-2").await.unwrap());

        let pubkey = "cd".repeat(32);
        let token = db
            .create_pending_handoff(Purpose::SignIn, &pubkey, "/wallet", true)
            .await
            .unwrap();
        assert!(db.pending_handoff(&token, Purpose::SignUp).await.unwrap().is_none());
        let pending = db.pending_handoff(&token, Purpose::SignIn).await.unwrap().unwrap();
        assert_eq!(pending.nostr_pubkey, pubkey);
        assert!(pending.trusted);
        assert!(
            db.take_pending_handoff(&token, Purpose::SignIn)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            db.take_pending_handoff(&token, Purpose::SignIn)
                .await
                .unwrap()
                .is_none()
        );
        assert!(db.pending_handoff("unknown", Purpose::SignIn).await.unwrap().is_none());
    }
}
