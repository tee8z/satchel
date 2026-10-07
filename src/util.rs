//! Small helpers shared by every module.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Seconds since the Unix epoch.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// 32 random bytes, URL-safe base64 without padding (43 characters).
pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system random source is available");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compares secrets without stopping at the first different byte.
pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// "1,234,567" for whole sats, with a millisat remainder when there is one.
pub(crate) fn format_msat(msat: i64) -> String {
    let sign = if msat < 0 { "-" } else { "" };
    let msat = msat.unsigned_abs();
    let sats = msat / 1000;
    let digits = sats.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    match msat % 1000 {
        0 => format!("{sign}{grouped}"),
        rest => format!("{sign}{grouped}.{rest:03}"),
    }
}

/// "2026-10-07 14:03 UTC" without a date library (civil-from-days).
pub(crate) fn format_time(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3600,
        (seconds % 3600) / 60
    )
}

/// Parses a whole number of sats typed by a person ("21", "1,000", "10_000").
pub(crate) fn parse_sats(input: &str) -> Option<u64> {
    let cleaned: String = input.trim().chars().filter(|c| !matches!(c, ',' | '_' | ' ')).collect();
    if cleaned.is_empty() || !cleaned.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    cleaned.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_amounts_and_times() {
        assert_eq!(format_msat(0), "0");
        assert_eq!(format_msat(1_234_567_000), "1,234,567");
        assert_eq!(format_msat(1_999), "1.999");
        assert_eq!(format_msat(-21_000), "-21");
        assert_eq!(format_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_time(1_791_381_780), "2026-10-07 14:03 UTC");
    }

    #[test]
    fn parses_typed_sats() {
        assert_eq!(parse_sats(" 1,000 "), Some(1000));
        assert_eq!(parse_sats("10_000"), Some(10_000));
        assert_eq!(parse_sats(""), None);
        assert_eq!(parse_sats("-5"), None);
        assert_eq!(parse_sats("1.5"), None);
    }

    #[test]
    fn compares_secrets() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
