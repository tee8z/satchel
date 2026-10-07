//! Operator blocks: IPv4 and IPv6 networks refused on every public route.
//! Stored in SQLite, cached in memory, and reloaded whenever they change.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::RwLock;

use sqlx::FromRow;

use crate::db::Db;
use crate::util::now;

/// Narrowest prefixes an operator may block, so one typo cannot block a continent.
const MIN_V4_PREFIX: u8 = 8;
const MIN_V6_PREFIX: u8 = 16;
/// What a one-click block from the busiest-addresses view covers.
pub(crate) const QUICK_V4_PREFIX: u8 = 24;
pub(crate) const QUICK_V6_PREFIX: u8 = 56;

/// An IPv4 or IPv6 network, host bits cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cidr {
    network: IpAddr,
    prefix: u8,
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix.min(32))).unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from(u32::from(v4) & mask))
        }
        IpAddr::V6(v6) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix.min(128))).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
        }
    }
}

impl Cidr {
    /// `203.0.113.0/24`, `2001:db8:1::/56`, or a bare address (one host).
    /// IPv4-mapped IPv6 addresses count as IPv4.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let address = address.parse::<IpAddr>().ok()?.to_canonical();
        let max = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) if !prefix.is_empty() && prefix.len() <= 3 && prefix.bytes().all(|b| b.is_ascii_digit()) => {
                prefix.parse::<u8>().ok().filter(|prefix| *prefix <= max)?
            }
            Some(_) => return None,
            None => max,
        };
        Some(Self {
            network: mask(address, prefix),
            prefix,
        })
    }

    /// The network's first address.
    pub(crate) fn network(&self) -> IpAddr {
        self.network
    }

    /// Whether the operator may block this network.
    pub(crate) fn is_narrow_enough(&self) -> bool {
        self.prefix
            >= match self.network {
                IpAddr::V4(_) => MIN_V4_PREFIX,
                IpAddr::V6(_) => MIN_V6_PREFIX,
            }
    }

    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.network.is_ipv4() && mask(ip, self.prefix) == self.network
    }

    /// The network a one-click block covers for a client key from the rate
    /// limiter: the /24 of an IPv4 address, or the /56 of an IPv6 prefix
    /// (or the whole prefix, when clients are grouped by a wider one).
    pub(crate) fn quick_block(client: &str) -> Option<Self> {
        let key = Self::parse(client)?;
        let prefix = match key.network {
            IpAddr::V4(_) => QUICK_V4_PREFIX,
            IpAddr::V6(_) => key.prefix.min(QUICK_V6_PREFIX),
        };
        Some(Self {
            network: mask(key.network, prefix),
            prefix,
        })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[derive(Clone, Debug, FromRow)]
pub(crate) struct Block {
    pub(crate) id: i64,
    pub(crate) cidr: String,
    pub(crate) reason: String,
    pub(crate) expires_at: Option<i64>,
    pub(crate) created_at: i64,
}

/// The blocks in force, in memory, so checking a request never waits on SQLite.
#[derive(Debug, Default)]
pub(crate) struct Blocklist {
    active: RwLock<Vec<(Cidr, Option<i64>)>>,
}

impl Blocklist {
    /// Reads the blocks again after a change (or at startup).
    pub(crate) async fn reload(&self, db: &Db) -> Result<(), sqlx::Error> {
        let blocks = db.blocks().await?;
        let active = blocks
            .iter()
            .filter_map(|block| Some((Cidr::parse(&block.cidr)?, block.expires_at)))
            .collect();
        *self.active.write().expect("blocklist lock is not poisoned") = active;
        Ok(())
    }

    /// Whether a block in force covers this address.
    pub(crate) fn blocks(&self, ip: IpAddr, now: i64) -> bool {
        self.active
            .read()
            .expect("blocklist lock is not poisoned")
            .iter()
            .any(|(cidr, expires_at)| expires_at.is_none_or(|expires_at| expires_at > now) && cidr.contains(ip))
    }
}

impl Db {
    /// Blocks in force, newest first.
    pub(crate) async fn blocks(&self) -> Result<Vec<Block>, sqlx::Error> {
        sqlx::query_as::<_, Block>(
            "SELECT id, cidr, reason, expires_at, created_at FROM blocks \
             WHERE expires_at IS NULL OR expires_at > ? ORDER BY id DESC",
        )
        .bind(now())
        .fetch_all(&self.read)
        .await
    }

    /// Adds a block; blocking a network again replaces its reason and expiry.
    pub(crate) async fn add_block(
        &self,
        cidr: &Cidr,
        reason: &str,
        expires_at: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO blocks (cidr, reason, expires_at, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (cidr) DO UPDATE SET reason = excluded.reason, expires_at = excluded.expires_at",
        )
        .bind(cidr.to_string())
        .bind(reason)
        .bind(expires_at)
        .bind(now())
        .execute(&self.write)
        .await?;
        Ok(())
    }

    pub(crate) async fn remove_block(&self, id: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM blocks WHERE id = ?")
            .bind(id)
            .execute(&self.write)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Forgets blocks that have expired; they no longer refuse anything.
    pub(crate) async fn delete_expired_blocks(&self, now: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM blocks WHERE expires_at IS NOT NULL AND expires_at <= ?")
            .bind(now)
            .execute(&self.write)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn parses_and_matches_networks() {
        let v4 = Cidr::parse("203.0.113.77/24").unwrap();
        assert_eq!(v4.to_string(), "203.0.113.0/24");
        assert!(v4.contains(ip("203.0.113.1")));
        assert!(v4.contains(ip("::ffff:203.0.113.200")));
        assert!(!v4.contains(ip("203.0.114.1")));
        assert!(!v4.contains(ip("2001:db8::1")));
        let v6 = Cidr::parse("2001:db8:1:2ff::/56").unwrap();
        assert_eq!(v6.to_string(), "2001:db8:1:200::/56");
        assert!(v6.contains(ip("2001:db8:1:2aa:1::1")));
        assert!(!v6.contains(ip("2001:db8:1:300::1")));
        assert_eq!(Cidr::parse("198.51.100.9").unwrap().to_string(), "198.51.100.9/32");
        assert_eq!(Cidr::parse(" 2001:db8::1 ").unwrap().to_string(), "2001:db8::1/128");
        for bad in [
            "",
            "nonsense",
            "1.2.3.4/33",
            "1.2.3.4/",
            "1.2.3.4/-1",
            "2001:db8::/129",
            "1.2.3.4/24/1",
        ] {
            assert!(Cidr::parse(bad).is_none(), "{bad} should not parse");
        }
        assert!(!Cidr::parse("0.0.0.0/0").unwrap().is_narrow_enough());
        assert!(!Cidr::parse("2001::/12").unwrap().is_narrow_enough());
        assert!(Cidr::parse("10.0.0.0/8").unwrap().is_narrow_enough());
    }

    #[test]
    fn quick_blocks_cover_the_neighbourhood() {
        assert_eq!(Cidr::quick_block("203.0.113.77").unwrap().to_string(), "203.0.113.0/24");
        assert_eq!(
            Cidr::quick_block("2001:db8:1:200::/56").unwrap().to_string(),
            "2001:db8:1:200::/56"
        );
        assert_eq!(
            Cidr::quick_block("2001:db8:1:2aa::/64").unwrap().to_string(),
            "2001:db8:1:200::/56"
        );
        assert_eq!(Cidr::quick_block("2001:db8::/48").unwrap().to_string(), "2001:db8::/48");
        assert!(Cidr::quick_block("garbage").is_none());
    }

    #[tokio::test]
    async fn blocks_apply_until_they_expire() {
        let (db, _dir) = test_db().await;
        let list = Blocklist::default();
        let now = now();
        db.add_block(&Cidr::parse("203.0.113.0/24").unwrap(), "spam", None)
            .await
            .unwrap();
        db.add_block(&Cidr::parse("2001:db8:1::/56").unwrap(), "spam", Some(now + 60))
            .await
            .unwrap();
        list.reload(&db).await.unwrap();
        assert!(list.blocks(ip("203.0.113.9"), now));
        assert!(list.blocks(ip("2001:db8:1:ff::1"), now));
        assert!(!list.blocks(ip("2001:db8:1:ff::1"), now + 60), "expired");
        assert!(!list.blocks(ip("198.51.100.1"), now));
        // Blocking the same network again updates it instead of adding a row.
        db.add_block(&Cidr::parse("203.0.113.5/24").unwrap(), "still spam", Some(now + 5))
            .await
            .unwrap();
        let blocks = db.blocks().await.unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            blocks
                .iter()
                .find(|block| block.cidr == "203.0.113.0/24")
                .unwrap()
                .reason,
            "still spam"
        );
        assert_eq!(db.delete_expired_blocks(now + 70).await.unwrap(), 2);
        list.reload(&db).await.unwrap();
        assert!(!list.blocks(ip("203.0.113.9"), now));
    }
}
