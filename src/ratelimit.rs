//! Fixed-window rate limits per client address and per account, in memory.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_KEYS: usize = 100_000;

#[derive(Debug, Default)]
pub(crate) struct RateLimiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimiter {
    /// Counts one attempt under `key`; false once `limit` attempts happened in `window`.
    pub(crate) fn allow(&self, key: &str, limit: u32, window: Duration) -> bool {
        if limit == 0 {
            return false;
        }
        let mut windows = self.windows.lock().expect("rate limiter lock is not poisoned");
        let now = Instant::now();
        if windows.len() >= MAX_KEYS {
            // Old windows no longer limit anything; drop them before growing further.
            windows.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(3600));
            if windows.len() >= MAX_KEYS {
                return false;
            }
        }
        let entry = windows.entry(key.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        if entry.1 >= limit {
            return false;
        }
        entry.1 += 1;
        true
    }
}

/// IPv6 clients usually hold a whole /64, so limit by that prefix.
pub(crate) fn client_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let segments = v6.segments();
                format!(
                    "{:x}:{:x}:{:x}:{:x}::/64",
                    segments[0], segments[1], segments[2], segments[3]
                )
            }
        },
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
    fn groups_ipv6_by_prefix() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        assert_eq!(client_key(a), client_key(b));
        assert_eq!(client_key("::ffff:1.2.3.4".parse().unwrap()), "1.2.3.4");
    }
}
