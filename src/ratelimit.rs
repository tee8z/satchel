//! Fixed-window rate limits per client address and per account, in memory.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_KEYS: usize = 100_000;

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    length: Duration,
    count: u32,
}

#[derive(Debug)]
pub(crate) struct RateLimiter {
    windows: Mutex<HashMap<String, Window>>,
    max_keys: usize,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::with_capacity(MAX_KEYS)
    }
}

impl RateLimiter {
    pub(crate) fn with_capacity(max_keys: usize) -> Self {
        Self {
            windows: Mutex::default(),
            max_keys: max_keys.max(1),
        }
    }

    /// Counts one attempt under `key`; false once `limit` attempts happened in `window`.
    pub(crate) fn allow(&self, key: &str, limit: u32, window: Duration) -> bool {
        if limit == 0 {
            return false;
        }
        let mut windows = self.windows.lock().expect("rate limiter lock is not poisoned");
        let now = Instant::now();
        if windows.len() >= self.max_keys && !windows.contains_key(key) {
            evict(&mut windows, now, self.max_keys);
        }
        let fresh = Window {
            start: now,
            length: window,
            count: 0,
        };
        let entry = windows.entry(key.to_owned()).or_insert(fresh);
        if now.duration_since(entry.start) >= window {
            *entry = fresh;
        }
        if entry.count >= limit {
            return false;
        }
        entry.count += 1;
        true
    }

    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        self.windows.lock().unwrap().len()
    }
}

/// Makes room in a full table: windows that have ended go first, then the
/// oldest quarter. A full table never refuses everyone; at worst an attacker
/// with many keys shortens other keys' memory.
fn evict(windows: &mut HashMap<String, Window>, now: Instant, max_keys: usize) {
    windows.retain(|_, window| now.duration_since(window.start) < window.length);
    if windows.len() < max_keys {
        return;
    }
    let mut starts: Vec<Instant> = windows.values().map(|window| window.start).collect();
    let cut = (starts.len() / 4).max(1);
    let (_, newest_evicted, _) = starts.select_nth_unstable(cut - 1);
    let newest_evicted = *newest_evicted;
    windows.retain(|_, window| window.start > newest_evicted);
}

/// The key per-address limits count under: an IPv4 address, or the IPv6
/// prefix of `ipv6_prefix` bits that one client or site usually holds.
pub(crate) fn client_key(ip: IpAddr, ipv6_prefix: u8) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let prefix = ipv6_prefix.min(128);
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            format!("{}/{prefix}", Ipv6Addr::from(u128::from(v6) & mask))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_each_key_separately_and_resets() {
        let limiter = RateLimiter::default();
        for _ in 0..3 {
            assert!(limiter.allow("login:1.2.3.4", 3, Duration::from_secs(60)));
        }
        assert!(!limiter.allow("login:1.2.3.4", 3, Duration::from_secs(60)));
        assert!(limiter.allow("login:5.6.7.8", 3, Duration::from_secs(60)));
        assert!(limiter.allow("burst", 1, Duration::ZERO));
        assert!(limiter.allow("burst", 1, Duration::ZERO));
        assert!(!limiter.allow("never", 0, Duration::from_secs(60)));
    }

    #[test]
    fn a_full_table_evicts_instead_of_refusing() {
        let limiter = RateLimiter::with_capacity(8);
        let minute = Duration::from_secs(60);
        for index in 0..8 {
            assert!(limiter.allow(&format!("signup:{index}"), 1, minute));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(limiter.tracked(), 8);
        // Every window is live and used up, yet a new key is still counted.
        assert!(limiter.allow("signup:new", 1, minute));
        assert!(limiter.tracked() <= 8);
        assert!(
            !limiter.allow("signup:new", 1, minute),
            "the new key is limited as usual"
        );
        // The oldest windows made room; the newest are still remembered.
        assert!(limiter.allow("signup:0", 1, minute));
        assert!(!limiter.allow("signup:7", 1, minute));
        // A table full of windows that have ended is simply cleared out.
        let limiter = RateLimiter::with_capacity(4);
        for index in 0..4 {
            assert!(limiter.allow(&format!("burst:{index}"), 1, Duration::ZERO));
        }
        for index in 0..100 {
            assert!(limiter.allow(&format!("other:{index}"), 1, minute));
            assert!(limiter.tracked() <= 4);
        }
    }

    #[test]
    fn groups_ipv6_by_prefix() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        let same_56: IpAddr = "2001:db8:1:ff:1::1".parse().unwrap();
        let next_56: IpAddr = "2001:db8:1:100::1".parse().unwrap();
        assert_eq!(client_key(a, 56), "2001:db8:1::/56");
        assert_eq!(client_key(a, 56), client_key(b, 56));
        assert_eq!(client_key(a, 56), client_key(same_56, 56));
        assert_ne!(client_key(a, 56), client_key(next_56, 56));
        assert_eq!(client_key(next_56, 56), "2001:db8:1:100::/56");
        assert_ne!(client_key(a, 64), client_key(same_56, 64));
        assert_eq!(client_key(a, 64), "2001:db8:1:2::/64");
        assert_eq!(client_key(a, 128), "2001:db8:1:2:aaaa::1/128");
        assert_eq!(client_key("::ffff:1.2.3.4".parse().unwrap(), 56), "1.2.3.4");
        assert_eq!(client_key("1.2.3.4".parse().unwrap(), 56), "1.2.3.4");
    }
}
