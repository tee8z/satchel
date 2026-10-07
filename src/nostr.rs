//! Nostr login. A NIP-07 browser signer signs a NIP-98-style HTTP auth event
//! (kind 27235) naming this server's URL, the method, and a one-time challenge.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use bech32::{Bech32, Hrp};
use secp256k1::schnorr::Signature;
use secp256k1::{Secp256k1, VerifyOnly, XOnlyPublicKey};
use serde::Deserialize;
use serde_json::json;

use crate::util::{random_token, sha256};

pub(crate) const AUTH_KIND: u64 = 27235;
/// How far a signed login or handoff event's time may be from ours.
pub(crate) const MAX_CLOCK_SKEW_SECS: u64 = 120;
const CHALLENGE_TTL: Duration = Duration::from_secs(300);
const MAX_CHALLENGES: usize = 10_000;

static SECP: LazyLock<Secp256k1<VerifyOnly>> = LazyLock::new(Secp256k1::verification_only);

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Event {
    pub(crate) id: String,
    pub(crate) pubkey: String,
    pub(crate) created_at: i64,
    pub(crate) kind: u64,
    pub(crate) tags: Vec<Vec<String>>,
    pub(crate) content: String,
    pub(crate) sig: String,
}

/// Challenges handed out and not yet used, held in memory for five minutes.
#[derive(Debug, Default)]
pub(crate) struct Challenges {
    issued: Mutex<HashMap<String, Instant>>,
}

impl Challenges {
    /// `None` when too many challenges are outstanding.
    pub(crate) fn issue(&self) -> Option<String> {
        let mut issued = self.issued.lock().expect("challenge lock is not poisoned");
        issued.retain(|_, at| at.elapsed() < CHALLENGE_TTL);
        if issued.len() >= MAX_CHALLENGES {
            return None;
        }
        let challenge = random_token();
        issued.insert(challenge.clone(), Instant::now());
        Some(challenge)
    }

    fn consume(&self, challenge: &str) -> bool {
        let mut issued = self.issued.lock().expect("challenge lock is not poisoned");
        issued.remove(challenge).is_some_and(|at| at.elapsed() < CHALLENGE_TTL)
    }
}

/// The NIP-01 event id: SHA-256 of `[0, pubkey, created_at, kind, tags, content]`.
pub(crate) fn event_id(event: &Event) -> [u8; 32] {
    let serialized = json!([0, event.pubkey, event.created_at, event.kind, event.tags, event.content]).to_string();
    sha256(serialized.as_bytes())
}

pub(crate) fn tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|tag| tag.first().is_some_and(|first| first == name))
        .and_then(|tag| tag.get(1))
        .map(String::as_str)
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Checks a NIP-98 HTTP auth event for `method url`: the kind, a time within
/// `max_skew_secs` of `now`, the `u` and `method` tags, the id, and the BIP-340
/// signature. Returns the signer's public key (lowercase hex).
pub(crate) fn verify_http_auth(
    event: &Event,
    url: &str,
    method: &str,
    max_skew_secs: u64,
    now: i64,
) -> Result<String, &'static str> {
    if event.kind != AUTH_KIND {
        return Err("The signed event has the wrong kind.");
    }
    if event.created_at.abs_diff(now) > max_skew_secs {
        return Err("The signed event's time is too far from now. Check your clock.");
    }
    if tag(event, "u") != Some(url) || tag(event, "method") != Some(method) {
        return Err("The signed event is for another page.");
    }
    if !is_lower_hex(&event.pubkey, 64) || !is_lower_hex(&event.id, 64) || !is_lower_hex(&event.sig, 128) {
        return Err("The signed event is malformed.");
    }
    let id = event_id(event);
    if hex::encode(id) != event.id {
        return Err("The signed event's id does not match its content.");
    }
    let mut pubkey = [0u8; 32];
    let mut signature = [0u8; 64];
    hex::decode_to_slice(&event.pubkey, &mut pubkey).map_err(|_| "The signed event is malformed.")?;
    hex::decode_to_slice(&event.sig, &mut signature).map_err(|_| "The signed event is malformed.")?;
    let pubkey = XOnlyPublicKey::from_byte_array(pubkey).map_err(|_| "The signer's public key is invalid.")?;
    SECP.verify_schnorr(&Signature::from_byte_array(signature), &id, &pubkey)
        .map_err(|_| "The signature is invalid.")?;
    Ok(event.pubkey.clone())
}

/// Checks a signed login event and uses up its challenge. Returns the signer's
/// public key (lowercase hex).
pub(crate) fn verify_login(
    event: &Event,
    url: &str,
    challenges: &Challenges,
    now: i64,
) -> Result<String, &'static str> {
    let challenge = tag(event, "challenge").ok_or("The signed event has no challenge.")?;
    let pubkey = verify_http_auth(event, url, "POST", MAX_CLOCK_SKEW_SECS, now)?;
    if !challenges.consume(challenge) {
        return Err("The login challenge expired. Try again.");
    }
    Ok(pubkey)
}

/// `npub1...` for display.
pub(crate) fn npub(pubkey_hex: &str) -> String {
    let mut bytes = [0u8; 32];
    if hex::decode_to_slice(pubkey_hex, &mut bytes).is_err() {
        return pubkey_hex.to_owned();
    }
    bech32::encode::<Bech32>(Hrp::parse_unchecked("npub"), &bytes).unwrap_or_else(|_| pubkey_hex.to_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use secp256k1::{Keypair, SecretKey};

    use super::*;
    use crate::util::now;

    pub(crate) const URL: &str = "https://wallet.example.org/auth/nostr";

    /// An event signed with the key `[secret; 32]`, as a NIP-07 extension would return it.
    pub(crate) fn sign(secret: u8, created_at: i64, tags: Vec<Vec<String>>) -> Event {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_byte_array([secret; 32]).unwrap());
        let mut event = Event {
            id: String::new(),
            pubkey: hex::encode(keypair.x_only_public_key().0.serialize()),
            created_at,
            kind: AUTH_KIND,
            tags,
            content: String::new(),
            sig: String::new(),
        };
        let id = event_id(&event);
        event.id = hex::encode(id);
        event.sig = hex::encode(secp.sign_schnorr_no_aux_rand(&id, &keypair).to_byte_array());
        event
    }

    /// A NIP-98 event for `method url`.
    pub(crate) fn http_auth(secret: u8, url: &str, method: &str, created_at: i64) -> Event {
        sign(
            secret,
            created_at,
            vec![vec!["u".into(), url.into()], vec!["method".into(), method.into()]],
        )
    }

    /// A signed login event.
    pub(crate) fn signed_event(secret: u8, url: &str, challenge: &str, created_at: i64) -> Event {
        sign(
            secret,
            created_at,
            vec![
                vec!["u".into(), url.into()],
                vec!["method".into(), "POST".into()],
                vec!["challenge".into(), challenge.into()],
            ],
        )
    }

    #[test]
    fn checks_http_auth_events_without_a_challenge() {
        let url = "https://wallet.example.org/api/v1/address";
        let event = http_auth(5, url, "GET", now());
        assert_eq!(verify_http_auth(&event, url, "GET", 60, now()).unwrap(), event.pubkey);
        assert!(verify_http_auth(&event, url, "POST", 60, now()).is_err());
        assert!(verify_http_auth(&event, "https://wallet.example.org/other", "GET", 60, now()).is_err());
        assert!(verify_http_auth(&event, url, "GET", 60, now() + 61).is_err());
        let mut far = http_auth(5, url, "GET", i64::MIN);
        assert!(verify_http_auth(&far, url, "GET", 60, now()).is_err());
        far.created_at = i64::MAX;
        assert!(verify_http_auth(&far, url, "GET", 60, now()).is_err());
        let mut forged = http_auth(5, url, "GET", now());
        forged.sig = http_auth(6, url, "GET", now()).sig;
        assert_eq!(
            verify_http_auth(&forged, url, "GET", 60, now()),
            Err("The signature is invalid.")
        );
    }

    #[test]
    fn accepts_a_signed_challenge_once() {
        let challenges = Challenges::default();
        let challenge = challenges.issue().unwrap();
        let event = signed_event(7, URL, &challenge, now());
        let pubkey = verify_login(&event, URL, &challenges, now()).unwrap();
        assert_eq!(pubkey, event.pubkey);
        assert!(
            verify_login(&event, URL, &challenges, now()).is_err(),
            "challenges are single use"
        );
        assert!(npub(&pubkey).starts_with("npub1"));
    }

    #[test]
    fn rejects_tampering_wrong_urls_and_stale_events() {
        let challenges = Challenges::default();
        let challenge = challenges.issue().unwrap();
        let mut tampered = signed_event(7, URL, &challenge, now());
        tampered.content = "changed".into();
        assert!(verify_login(&tampered, URL, &challenges, now()).is_err());
        let mut forged = signed_event(7, URL, &challenge, now());
        forged.pubkey = signed_event(8, URL, &challenge, now()).pubkey;
        forged.id = hex::encode(event_id(&forged));
        assert!(verify_login(&forged, URL, &challenges, now()).is_err());
        let other_site = signed_event(7, "https://evil.example/auth/nostr", &challenge, now());
        assert!(verify_login(&other_site, URL, &challenges, now()).is_err());
        let stale = signed_event(7, URL, &challenge, now() - 600);
        assert!(verify_login(&stale, URL, &challenges, now()).is_err());
        let unknown = signed_event(7, URL, "not-issued", now());
        assert!(verify_login(&unknown, URL, &challenges, now()).is_err());
        // None of the rejected events used up the real challenge.
        let good = signed_event(7, URL, &challenge, now());
        assert!(verify_login(&good, URL, &challenges, now()).is_ok());
    }
}
