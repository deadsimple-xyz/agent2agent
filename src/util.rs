//! Small shared helpers: hex encoding, timestamps, random identifiers.

use anyhow::{bail, Result};

/// Encode bytes as lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).expect("nibble is < 16"));
        out.push(char::from_digit((b & 0x0f) as u32, 16).expect("nibble is < 16"));
    }
    out
}

/// Decode a lowercase-or-uppercase hex string.
pub fn from_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        bail!("hex string has odd length ({})", s.len());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| anyhow::anyhow!("invalid hex digit {:?}", pair[0] as char))?;
        let lo = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| anyhow::anyhow!("invalid hex digit {:?}", pair[1] as char))?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

/// Seconds since the Unix epoch. Saturates rather than panicking on a clock before 1970.
pub fn now_ts() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A random hex identifier of `bytes` random bytes (so `bytes * 2` characters).
///
/// Used both for message ids and for the delimiter token in [`crate::render`]. The
/// render token must be unpredictable to the remote peer, so this has to be a CSPRNG
/// draw, not a counter.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    to_hex(&buf)
}

/// A fresh message id.
pub fn new_message_id() -> String {
    random_hex(16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let cases: &[&[u8]] = &[b"", b"\x00", b"\xff", b"hello world", &[0, 1, 2, 250, 255]];
        for case in cases {
            let encoded = to_hex(case);
            assert_eq!(from_hex(&encoded).unwrap(), *case, "roundtrip for {case:?}");
        }
    }

    #[test]
    fn hex_encodes_known_values() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn hex_accepts_uppercase() {
        assert_eq!(from_hex("DEADBEEF").unwrap(), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn hex_rejects_odd_length() {
        assert!(from_hex("abc").is_err());
    }

    #[test]
    fn hex_rejects_non_hex_digits() {
        assert!(from_hex("zz").is_err());
        assert!(from_hex("00gg").is_err());
    }

    #[test]
    fn random_hex_has_expected_length_and_varies() {
        let a = random_hex(16);
        let b = random_hex(16);
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b, "two draws must not collide");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn now_ts_is_plausible() {
        // Later than 2020-01-01 and earlier than 2100-01-01.
        let ts = now_ts();
        assert!(ts > 1_577_836_800, "timestamp {ts} is before 2020");
        assert!(ts < 4_102_444_800, "timestamp {ts} is after 2100");
    }
}
