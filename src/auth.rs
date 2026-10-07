//! Passwords (argon2id), usernames, and session tokens.

use std::collections::HashSet;
use std::sync::LazyLock;

use anyhow::{Result, anyhow};
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};

use crate::util::{random_token, sha256};

pub(crate) const MIN_PASSWORD_LEN: usize = 10;
const MAX_PASSWORD_LEN: usize = 256;

/// Names nobody can register, so the operator and common mailbox names stay free.
const RESERVED_USERNAMES: &[&str] = &[
    "abuse",
    "admin",
    "administrator",
    "api",
    "assets",
    "auth",
    "billing",
    "faucet",
    "help",
    "hostmaster",
    "info",
    "lnurl",
    "lnurlp",
    "login",
    "logout",
    "mail",
    "metrics",
    "noreply",
    "no-reply",
    "nostr",
    "operator",
    "postmaster",
    "root",
    "security",
    "settings",
    "signup",
    "staff",
    "support",
    "system",
    "wallet",
    "webmaster",
    "well-known",
    "www",
];

/// Checked when a username is unknown, so a failed login takes as long either way.
pub(crate) static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password(&random_token()).expect("hashing a random password works"));

/// argon2id with the crate's defaults (19 MiB, two passes) and a random salt.
pub(crate) fn hash_password(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|error| anyhow!("random source failed: {error}"))?;
    let salt = SaltString::encode_b64(&salt).map_err(|error| anyhow!("cannot encode salt: {error}"))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow!("cannot hash password: {error}"))?;
    Ok(hash.to_string())
}

pub(crate) fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
}

pub(crate) fn check_new_password(password: &str, confirm: &str) -> Result<(), &'static str> {
    let length = password.chars().count();
    if length < MIN_PASSWORD_LEN {
        return Err("Use at least 10 characters for the password.");
    }
    if length > MAX_PASSWORD_LEN {
        return Err("That password is too long.");
    }
    if password != confirm {
        return Err("The passwords do not match.");
    }
    Ok(())
}

/// Lightning Address local parts: 3-32 of `a-z 0-9 . _ -`, starting and
/// ending with a letter or digit. Input is lowercased first.
pub(crate) fn normalize_username(input: &str, reserved: &HashSet<String>) -> Result<String, &'static str> {
    let name = input.trim().to_ascii_lowercase();
    let bytes = name.as_bytes();
    let edge_ok = |byte: Option<&u8>| byte.is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if !(3..=32).contains(&bytes.len())
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-'))
        || !edge_ok(bytes.first())
        || !edge_ok(bytes.last())
        || name.contains("..")
    {
        return Err(
            "Usernames are 3 to 32 letters, digits, dots, dashes, or underscores, starting and ending with a letter or digit.",
        );
    }
    if RESERVED_USERNAMES.contains(&name.as_str()) || reserved.contains(&name) {
        return Err("That username is reserved.");
    }
    Ok(name)
}

/// A new session: the cookie value, the hash stored in the database, and its CSRF token.
pub(crate) struct NewSession {
    pub(crate) cookie: String,
    pub(crate) token_hash: String,
    pub(crate) csrf: String,
}

pub(crate) fn new_session() -> NewSession {
    let cookie = random_token();
    NewSession {
        token_hash: token_hash(&cookie),
        cookie,
        csrf: random_token(),
    }
}

pub(crate) fn token_hash(cookie: &str) -> String {
    hex::encode(sha256(cookie.as_bytes()))
}

/// Request keys make a repeated form submission return the first result.
pub(crate) fn valid_request_key(key: &str) -> bool {
    (16..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_and_verify() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "correct horse battery"));
        assert!(!verify_password(&hash, "wrong password"));
        assert!(!verify_password("not a hash", "anything"));
        assert!(check_new_password("short", "short").is_err());
        assert!(check_new_password("long enough pw", "long enough px").is_err());
        assert!(check_new_password("long enough pw", "long enough pw").is_ok());
    }

    #[test]
    fn username_rules() {
        let reserved: HashSet<String> = ["carol".to_owned()].into();
        assert_eq!(normalize_username(" Alice ", &reserved).unwrap(), "alice");
        assert_eq!(normalize_username("bob.smith-2", &reserved).unwrap(), "bob.smith-2");
        for bad in [
            "ab",
            ".alice",
            "alice.",
            "al..ice",
            "al ice",
            "alice@x",
            "älice",
            &"a".repeat(33),
        ] {
            assert!(normalize_username(bad, &reserved).is_err(), "{bad} should be rejected");
        }
        assert!(normalize_username("admin", &reserved).is_err());
        assert!(normalize_username("carol", &reserved).is_err());
    }

    #[test]
    fn session_tokens_are_random_and_hashed() {
        let first = new_session();
        let second = new_session();
        assert_ne!(first.cookie, second.cookie);
        assert_eq!(first.token_hash, token_hash(&first.cookie));
        assert_ne!(first.token_hash, first.cookie);
        assert!(valid_request_key(&first.csrf));
        assert!(!valid_request_key("short"));
        assert!(!valid_request_key("has spaces in it here"));
    }
}
