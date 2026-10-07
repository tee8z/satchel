//! Proof of work for creating accounts. The server hands out stateless,
//! HMAC-signed challenges; the browser finds a nonce in a Web Worker
//! (`assets/pow-worker.js`); the server checks it with one hash.
//!
//! Challenge bytes (41, base64url without padding): 16 random bytes,
//! `expires_at` (u64 big-endian), `difficulty` (u8), then the first 16 bytes
//! of HMAC-SHA256(secret, the preceding 25 bytes). A nonce (u64) solves it
//! when SHA-256(challenge bytes ‖ nonce big-endian) starts with at least
//! `difficulty` zero bits. Each solved challenge is accepted once.
//!
//! Difficulty is global, never per address, so a crowd behind one address
//! pays the same as anyone.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config;
use crate::util::constant_time_eq;

/// How long a challenge may be solved and used.
pub(crate) const CHALLENGE_TTL_SECS: i64 = 600;
const CHALLENGE_LEN: usize = 41;
/// The random part, expiry, and difficulty: what the HMAC covers.
const SIGNED_LEN: usize = 25;
const TAG_LEN: usize = 16;
/// Solved challenges remembered until they expire. Each one costs a solved
/// puzzle and the global sign-up cap applies first, so this is never reached
/// in practice.
const MAX_CONSUMED: usize = 100_000;

/// Why a solution was refused. Messages are for the person signing up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PowError {
    Missing,
    Malformed,
    Forged,
    Expired,
    TooEasy,
    Unsolved,
    Reused,
    Busy,
}

impl PowError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Missing => "Your browser did not finish the sign-up check. Turn on JavaScript and try again.",
            Self::Malformed | Self::Forged | Self::Unsolved => {
                "The sign-up check did not pass. Reload the page and try again."
            }
            Self::Expired => "The sign-up check expired. Try again.",
            Self::TooEasy => "Many wallets are being created right now, so the sign-up check got harder. Try again.",
            Self::Reused => "That sign-up check was already used. Try again.",
            Self::Busy => "Too many sign-ups right now. Try again shortly.",
        }
    }
}

/// What `POST /auth/pow` returns.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Challenge {
    pub(crate) challenge: String,
    pub(crate) difficulty: u8,
    pub(crate) expires_at: i64,
}

pub(crate) struct Pow {
    pub(crate) enabled: bool,
    base_bits: u8,
    max_bits: u8,
    step_signups: u32,
    /// Per process: a restart only invalidates challenges being solved.
    secret: [u8; 32],
    /// Random parts of accepted challenges and when each expires.
    consumed: Mutex<HashMap<[u8; 16], i64>>,
}

impl Pow {
    pub(crate) fn new(config: &config::Pow) -> Self {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("the operating system random source is available");
        Self {
            enabled: config.enabled,
            base_bits: config.base_bits,
            max_bits: config.max_bits,
            step_signups: config.step_signups.max(1),
            secret,
            consumed: Mutex::default(),
        }
    }

    /// Bits required when `recent` accounts were created in the last hour.
    /// With proof of work off, challenges are trivial and never checked.
    pub(crate) fn difficulty(&self, recent: u64) -> u8 {
        if !self.enabled {
            return 0;
        }
        let bits = u64::from(self.base_bits).saturating_add(recent / u64::from(self.step_signups));
        u8::try_from(bits.min(u64::from(self.max_bits))).unwrap_or(self.max_bits)
    }

    pub(crate) fn issue(&self, difficulty: u8, now: i64) -> Challenge {
        let mut bytes = [0u8; CHALLENGE_LEN];
        getrandom::fill(&mut bytes[..16]).expect("the operating system random source is available");
        let expires_at = now + CHALLENGE_TTL_SECS;
        bytes[16..24].copy_from_slice(&u64::try_from(expires_at).unwrap_or(0).to_be_bytes());
        bytes[24] = difficulty;
        let tag = hmac_sha256(&self.secret, &bytes[..SIGNED_LEN]);
        bytes[SIGNED_LEN..].copy_from_slice(&tag[..TAG_LEN]);
        Challenge {
            challenge: URL_SAFE_NO_PAD.encode(bytes),
            difficulty,
            expires_at,
        }
    }

    /// Checks a solution against the difficulty required now and uses up its
    /// challenge. `nonce` is decimal, as the form sends it.
    pub(crate) fn verify(&self, challenge: &str, nonce: &str, required: u8, now: i64) -> Result<(), PowError> {
        let (challenge, nonce) = (challenge.trim(), nonce.trim());
        if challenge.is_empty() || nonce.is_empty() {
            return Err(PowError::Missing);
        }
        let bytes = decode(challenge).ok_or(PowError::Malformed)?;
        let nonce = parse_nonce(nonce).ok_or(PowError::Malformed)?;
        let tag = hmac_sha256(&self.secret, &bytes[..SIGNED_LEN]);
        if !constant_time_eq(&tag[..TAG_LEN], &bytes[SIGNED_LEN..]) {
            return Err(PowError::Forged);
        }
        let expires_at = expires_at(&bytes);
        if expires_at <= now {
            return Err(PowError::Expired);
        }
        let difficulty = bytes[24];
        if difficulty < required {
            return Err(PowError::TooEasy);
        }
        if leading_zero_bits(&solution_hash(&bytes, nonce)) < u32::from(difficulty) {
            return Err(PowError::Unsolved);
        }
        self.consume(&bytes, expires_at, now)
    }

    fn consume(&self, bytes: &[u8; CHALLENGE_LEN], expires_at: i64, now: i64) -> Result<(), PowError> {
        let mut consumed = self.consumed.lock().expect("proof-of-work lock is not poisoned");
        let mut key = [0u8; 16];
        key.copy_from_slice(&bytes[..16]);
        if consumed.contains_key(&key) {
            return Err(PowError::Reused);
        }
        if consumed.len() >= MAX_CONSUMED {
            consumed.retain(|_, expires| *expires > now);
            if consumed.len() >= MAX_CONSUMED {
                return Err(PowError::Busy);
            }
        }
        consumed.insert(key, expires_at);
        Ok(())
    }

    /// Forgets accepted challenges that have expired anyway.
    pub(crate) fn prune(&self, now: i64) {
        self.consumed
            .lock()
            .expect("proof-of-work lock is not poisoned")
            .retain(|_, expires| *expires > now);
    }
}

fn decode(challenge: &str) -> Option<[u8; CHALLENGE_LEN]> {
    let bytes = URL_SAFE_NO_PAD.decode(challenge).ok()?;
    <[u8; CHALLENGE_LEN]>::try_from(bytes).ok()
}

fn parse_nonce(nonce: &str) -> Option<u64> {
    if nonce.len() > 20 || !nonce.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    nonce.parse().ok()
}

fn expires_at(bytes: &[u8; CHALLENGE_LEN]) -> i64 {
    let mut expiry = [0u8; 8];
    expiry.copy_from_slice(&bytes[16..24]);
    i64::try_from(u64::from_be_bytes(expiry)).unwrap_or(0)
}

/// SHA-256(challenge bytes ‖ nonce as u64 big-endian).
fn solution_hash(bytes: &[u8], nonce: u64) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.update(nonce.to_be_bytes());
    hasher.finalize().into()
}

fn leading_zero_bits(hash: &[u8; 32]) -> u32 {
    let mut bits = 0;
    for byte in hash {
        bits += byte.leading_zeros();
        if *byte != 0 {
            break;
        }
    }
    bits
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&crate::util::sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let inner: [u8; 32] = inner.finalize().into();
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// The smallest nonce that solves a challenge, as the browser's worker finds it.
#[cfg(test)]
pub(crate) fn solve(challenge: &str) -> u64 {
    let bytes = decode(challenge).expect("a challenge from issue()");
    let difficulty = u32::from(bytes[24]);
    (0..)
        .find(|nonce| leading_zero_bits(&solution_hash(&bytes, *nonce)) >= difficulty)
        .expect("a nonce exists")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pow(base_bits: u8, max_bits: u8, step_signups: u32) -> Pow {
        Pow::new(&config::Pow {
            enabled: true,
            base_bits,
            max_bits,
            step_signups,
        })
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex::encode(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Keys longer than a block are hashed first (RFC 4231 test case 6).
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// The same vector is in `assets/sha256.js`, so the browser and the
    /// server agree on the byte layout and the bit count.
    #[test]
    fn shared_test_vector() {
        let bytes: Vec<u8> = (0..41).collect();
        assert_eq!(
            URL_SAFE_NO_PAD.encode(&bytes),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJyg"
        );
        let hash = solution_hash(&bytes, 1457);
        assert_eq!(
            hex::encode(hash),
            "000010261cce78dd115e49ee09495c4c6d009ee79b4168c3f5ff1f6cee166e91"
        );
        assert_eq!(leading_zero_bits(&hash), 19);
        let first = (0..).find(|nonce| leading_zero_bits(&solution_hash(&bytes, *nonce)) >= 16);
        assert_eq!(first, Some(1457));
        let first = (0..).find(|nonce| leading_zero_bits(&solution_hash(&bytes, *nonce)) >= 12);
        assert_eq!(first, Some(1063));
    }

    /// The 5day4cast coordinator implements the same challenges; this is its
    /// vector, so a challenge issued by either verifies the same way.
    #[test]
    fn coordinator_test_vector() {
        let mut bytes = [0u8; CHALLENGE_LEN];
        for (index, byte) in bytes[..16].iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        bytes[16..24].copy_from_slice(&1_791_400_600_u64.to_be_bytes());
        bytes[24] = 16;
        let tag = hmac_sha256(&[0x42; 32], &bytes[..SIGNED_LEN]);
        bytes[SIGNED_LEN..].copy_from_slice(&tag[..TAG_LEN]);
        assert_eq!(
            URL_SAFE_NO_PAD.encode(bytes),
            "AAECAwQFBgcICQoLDA0ODwAAAABqxpqYEAMhk91h_us6MSi5qOUB7I8"
        );
        let first = (0..).find(|nonce| leading_zero_bits(&solution_hash(&bytes, *nonce)) >= 16);
        assert_eq!(first, Some(91_039));
        assert!(hex::encode(solution_hash(&bytes, 91_039)).starts_with("0000ef4d"));
        // A nonce above 2^32 exercises the high word of the big-endian u64.
        assert!(leading_zero_bits(&solution_hash(&bytes, 4_294_971_180)) >= 12);
    }

    #[test]
    fn accepts_a_solution_once() {
        let pow = pow(6, 10, 200);
        let now = 1_791_400_000;
        let issued = pow.issue(pow.difficulty(0), now);
        assert_eq!(issued.difficulty, 6);
        assert_eq!(issued.expires_at, now + CHALLENGE_TTL_SECS);
        assert_eq!(issued.challenge.len(), 55);
        let nonce = solve(&issued.challenge).to_string();
        assert_eq!(pow.verify(&issued.challenge, &nonce, 6, now), Ok(()));
        assert_eq!(pow.verify(&issued.challenge, &nonce, 6, now), Err(PowError::Reused));
    }

    #[test]
    fn refuses_bad_solutions() {
        let pow = pow(8, 10, 200);
        let now = 1_791_400_000;
        let issued = pow.issue(8, now);
        let nonce = solve(&issued.challenge);
        let check = |challenge: &str, nonce: &str, required: u8, at: i64| pow.verify(challenge, nonce, required, at);
        assert_eq!(check("", "", 8, now), Err(PowError::Missing));
        assert_eq!(check(&issued.challenge, "", 8, now), Err(PowError::Missing));
        assert_eq!(check("not base64!", "1", 8, now), Err(PowError::Malformed));
        assert_eq!(check(&issued.challenge, "-1", 8, now), Err(PowError::Malformed));
        assert_eq!(check(&issued.challenge, "0x10", 8, now), Err(PowError::Malformed));
        // A nonce that does not solve it: the first one below the solution
        // that misses (the solution is the smallest that works).
        if nonce > 0 {
            assert_eq!(check(&issued.challenge, "0", 8, now), Err(PowError::Unsolved));
        }
        // Expired challenges are refused, solved or not.
        assert_eq!(
            check(&issued.challenge, &nonce.to_string(), 8, issued.expires_at),
            Err(PowError::Expired)
        );
        // The service now asks for more than this challenge was issued with.
        assert_eq!(
            check(&issued.challenge, &nonce.to_string(), 9, now),
            Err(PowError::TooEasy)
        );
        // Lowering the difficulty byte breaks the HMAC.
        let mut bytes = decode(&issued.challenge).unwrap();
        bytes[24] = 0;
        let tampered = URL_SAFE_NO_PAD.encode(bytes);
        assert_eq!(check(&tampered, "0", 0, now), Err(PowError::Forged));
        // So does a challenge signed by another process.
        let other = self::pow(8, 10, 200).issue(8, now);
        let other_nonce = solve(&other.challenge).to_string();
        assert_eq!(check(&other.challenge, &other_nonce, 8, now), Err(PowError::Forged));
        // None of that used up the real challenge.
        assert_eq!(check(&issued.challenge, &nonce.to_string(), 8, now), Ok(()));
    }

    #[test]
    fn difficulty_rises_with_recent_signups() {
        let pow = pow(18, 22, 200);
        assert_eq!(pow.difficulty(0), 18);
        assert_eq!(pow.difficulty(199), 18);
        assert_eq!(pow.difficulty(200), 19);
        assert_eq!(pow.difficulty(799), 21);
        assert_eq!(pow.difficulty(800), 22);
        assert_eq!(pow.difficulty(1_000_000), 22);
        let off = Pow::new(&config::Pow {
            enabled: false,
            ..config::Pow::default()
        });
        assert_eq!(off.difficulty(1_000), 0);
    }

    #[test]
    fn pruning_forgets_only_expired_challenges() {
        let pow = pow(1, 1, 200);
        let now = 1_791_400_000;
        let issued = pow.issue(1, now);
        let nonce = solve(&issued.challenge).to_string();
        pow.verify(&issued.challenge, &nonce, 1, now).unwrap();
        pow.prune(now);
        assert_eq!(pow.verify(&issued.challenge, &nonce, 1, now), Err(PowError::Reused));
        pow.prune(issued.expires_at);
        assert!(pow.consumed.lock().unwrap().is_empty());
    }
}
