//! One-shot pairing, so that connecting two agents costs the operator one paste.
//!
//! Without this, pairing needs three hops: each side must learn the other's endpoint id,
//! and an id can only travel by hand. An invite closes the loop in one direction — the
//! code carries the inviter's id *and* a single-use token, so the joiner can both reach
//! the inviter and prove it was invited. The inviter learns the joiner's id from the
//! authenticated connection itself and writes it down on the spot.
//!
//! The token matters because an endpoint id is permanent and public. Anyone who ever saw
//! a code could otherwise race a later pairing window. A token is redeemed once and
//! burned, so an old code buys nothing.
//!
//! Pairing rides on its own ALPN, which is the only path into the daemon that skips the
//! peer list — necessarily, since the whole point is that the caller is not in it yet.

use anyhow::{bail, Context, Result};
use data_encoding::BASE64URL_NOPAD;
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::config::validate_name;

/// ALPN for the pairing handshake, distinct from the message ALPN so the accept loop can
/// route on it and apply completely different admission rules.
pub const PAIR_ALPN: &[u8] = b"agent2agent/pair/1";

/// Marker that opens every invite code, so a pasted string identifies itself.
const CODE_PREFIX: &str = "a2a1";

/// Bytes of entropy in a pairing token.
///
/// 96 bits, redeemable once and only for an hour. The code is copied by hand between two
/// chats, so every character of it is a character someone has to move.
pub const TOKEN_BYTES: usize = 12;

/// How long an invite stays redeemable by default.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// This build's version, carried in every code it mints.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The string a user copies from one agent's chat into another's.
///
/// Wire form: `a2a1.<name>.<endpoint id>.<token>.<version>` — dot-separated, and none of
/// the parts can contain a dot, so parsing is unambiguous. The version uses dashes for
/// exactly that reason.
///
/// It rides along so the joiner can tell it is behind before anything mysterious happens:
/// the two sides follow the same written guide, and a guide describing flags the local
/// binary does not have is worse than a plain "upgrade first".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCode {
    /// What the inviter calls itself; the joiner files it under this name.
    pub name: String,
    /// The inviter's endpoint id, base64url. 43 characters instead of hex's 64.
    pub id: String,
    /// Single-use token, base64url.
    pub token: String,
    /// The inviter's version. Absent in codes minted before this was carried.
    pub version: Option<String>,
}

impl InviteCode {
    pub fn encode(&self) -> String {
        let mut out = format!("{CODE_PREFIX}.{}.{}.{}", self.name, self.id, self.token);
        if let Some(version) = &self.version {
            out.push('.');
            out.push_str(&version.replace('.', "-"));
        }
        out
    }

    pub fn decode(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let parts: Vec<&str> = trimmed.split('.').collect();
        if parts.len() < 4 || parts.len() > 5 {
            bail!(
                "invite code should have 4 or 5 dot-separated parts, found {}",
                parts.len()
            );
        }
        if parts[0] != CODE_PREFIX {
            bail!("not an agent2agent invite code (expected it to start with {CODE_PREFIX}.)");
        }

        let version = match parts.get(4) {
            None => None,
            Some(raw) => {
                if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit() || c == '-') {
                    bail!("invite code carries a malformed version {raw:?}");
                }
                Some(raw.replace('-', "."))
            }
        };

        let code = Self {
            name: parts[1].to_string(),
            id: parts[2].to_string(),
            token: parts[3].to_string(),
            version,
        };

        // Validate every part now, so a mistyped code fails here with a clear message
        // rather than somewhere inside the dial.
        validate_name(&code.name).context("invite code carries an invalid peer name")?;
        code.endpoint_id()
            .context("invite code carries an invalid endpoint id")?;
        let token = BASE64URL_NOPAD
            .decode(code.token.as_bytes())
            .map_err(|_| anyhow::anyhow!("invite code carries a malformed token"))?;
        if token.len() != TOKEN_BYTES {
            bail!(
                "invite code carries a {} byte token, expected {TOKEN_BYTES}",
                token.len()
            );
        }
        Ok(code)
    }

    /// The inviter's key, decoded.
    pub fn endpoint_id(&self) -> Result<EndpointId> {
        let bytes = BASE64URL_NOPAD
            .decode(self.id.as_bytes())
            .map_err(|_| anyhow::anyhow!("endpoint id is not valid base64url"))?;
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("endpoint id is {} bytes, expected 32", bytes.len()))?;
        EndpointId::from_bytes(&bytes).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Render a key for a code.
    pub fn encode_id(id: &EndpointId) -> String {
        BASE64URL_NOPAD.encode(id.as_bytes())
    }

    /// A fresh single-use token.
    pub fn new_token() -> String {
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::fill(&mut bytes[..]);
        BASE64URL_NOPAD.encode(&bytes)
    }
}

/// Sent by the joiner as the first frame of a pairing connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub v: u8,
    /// The token from the invite code, proving this connection was invited.
    pub token: String,
    /// What the joiner calls itself; the inviter files it under this name.
    pub name: String,
}

/// The inviter's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum JoinResponse {
    /// Paired. `name` is what the inviter calls itself, echoed for display.
    Ok {
        name: String,
    },
    Error {
        message: String,
    },
}

/// Split a dotted version into numbers, for comparison. Unparseable parts count as zero.
fn version_parts(version: &str) -> Vec<u32> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Whether `theirs` is newer than `ours`.
///
/// Only one direction matters. A joiner behind the inviter may be missing arguments the
/// shared guide tells it to pass; a joiner ahead is fine, since the protocol has not
/// changed under it.
pub fn is_newer(theirs: &str, ours: &str) -> bool {
    let (theirs, ours) = (version_parts(theirs), version_parts(ours));
    let width = theirs.len().max(ours.len());
    for index in 0..width {
        let (t, o) = (
            theirs.get(index).copied().unwrap_or(0),
            ours.get(index).copied().unwrap_or(0),
        );
        if t != o {
            return t > o;
        }
    }
    false
}

/// A name that is not the one the other side is already using.
///
/// Identity is remembered per directory, and two agents can perfectly well be working in
/// the same one — a user with Claude and Codex both open in their home directory gets the
/// same name on both sides of the channel. Asking which of them should change is a
/// question with no interesting answer, so the joiner just takes a different one.
pub fn distinct_from(preferred: &str, taken: &str) -> String {
    if !preferred.eq_ignore_ascii_case(taken) {
        return preferred.to_string();
    }
    for suffix in 2..100 {
        let candidate = format!("{preferred}{suffix}");
        if !candidate.eq_ignore_ascii_case(taken) {
            return candidate;
        }
    }
    // Unreachable in practice: `taken` is one name, so the first candidate already differs.
    format!("{preferred}-x")
}

/// Compare tokens without an early exit.
///
/// A 128-bit token is not realistically guessable byte by byte over a network, but a
/// constant-time compare costs nothing and removes the question.
pub fn tokens_match(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // An empty token is never a match. Nothing should ever store one, but this is an
    // admission check, and "empty unlocks empty" is not a property worth having.
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn sample() -> InviteCode {
        InviteCode {
            name: "kip".into(),
            id: InviteCode::encode_id(&SecretKey::generate().public()),
            token: InviteCode::new_token(),
            version: Some(VERSION.to_string()),
        }
    }

    #[test]
    fn code_roundtrips() {
        let code = sample();
        let encoded = code.encode();
        assert_eq!(InviteCode::decode(&encoded).unwrap(), code);
    }

    #[test]
    fn code_is_self_identifying_and_one_line() {
        let encoded = sample().encode();
        assert!(encoded.starts_with("a2a1."));
        assert_eq!(encoded.lines().count(), 1);
        assert!(!encoded.contains(char::is_whitespace));
    }

    #[test]
    fn decode_tolerates_surrounding_whitespace() {
        // Codes get copied out of chat transcripts, which adds stray whitespace.
        let code = sample();
        let padded = format!("  {}\n", code.encode());
        assert_eq!(InviteCode::decode(&padded).unwrap(), code);
    }

    #[test]
    fn decode_rejects_a_foreign_string() {
        assert!(InviteCode::decode("hello").is_err());
        assert!(InviteCode::decode("").is_err());
        assert!(InviteCode::decode("https://example.com/a.b.c").is_err());
    }

    #[test]
    fn decode_rejects_a_wrong_prefix() {
        let code = sample();
        let wrong = code.encode().replacen("a2a1", "a2a9", 1);
        let err = InviteCode::decode(&wrong).unwrap_err();
        assert!(err.to_string().contains("a2a1"), "unexpected error: {err}");
    }

    #[test]
    fn decode_rejects_a_wrong_part_count() {
        let code = sample();
        assert!(InviteCode::decode(&format!("{}.a.b", code.encode())).is_err());
        assert!(InviteCode::decode("a2a1.name.id").is_err());
    }

    #[test]
    fn a_code_carries_the_version_that_minted_it() {
        let code = sample();
        let decoded = InviteCode::decode(&code.encode()).unwrap();
        assert_eq!(decoded.version.as_deref(), Some(VERSION));
    }

    #[test]
    fn a_code_from_before_versions_were_carried_still_parses() {
        // Four parts, no version. It should read as "unknown", not as broken.
        let code = sample();
        let old = format!("a2a1.{}.{}.{}", code.name, code.id, code.token);
        let decoded = InviteCode::decode(&old).unwrap();
        assert_eq!(decoded.version, None);
    }

    #[test]
    fn decode_rejects_a_malformed_version() {
        let code = sample();
        let bad = format!("a2a1.{}.{}.{}.zz", code.name, code.id, code.token);
        assert!(InviteCode::decode(&bad).is_err());
    }

    #[test]
    fn only_a_newer_peer_counts_as_newer() {
        assert!(is_newer("0.3.0", "0.2.9"));
        assert!(is_newer("0.2.10", "0.2.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.9", "0.2.9"));
        assert!(!is_newer("0.2.8", "0.2.9"), "being ahead is not a problem");
        assert!(!is_newer("0.2", "0.2.0"), "missing parts read as zero");
        assert!(is_newer("0.2.1", "0.2"));
    }

    #[test]
    fn the_code_is_short_enough_to_paste() {
        // It is copied by hand between two chats, so every character costs something.
        let encoded = sample().encode();
        assert!(
            encoded.len() < 90,
            "invite code grew to {} characters: {encoded}",
            encoded.len()
        );
    }

    #[test]
    fn decode_rejects_a_bad_endpoint_id() {
        let token = InviteCode::new_token();
        assert!(InviteCode::decode(&format!("a2a1.kip.tooshort.{token}")).is_err());
    }

    #[test]
    fn decode_rejects_a_bad_name() {
        let code = sample();
        let bad = format!("a2a1.has space.{}.{}", code.id, code.token);
        assert!(InviteCode::decode(&bad).is_err());
    }

    #[test]
    fn an_endpoint_id_survives_the_round_trip() {
        let key = SecretKey::generate().public();
        let code = InviteCode {
            name: "kip".into(),
            id: InviteCode::encode_id(&key),
            token: InviteCode::new_token(),
            version: None,
        };
        assert_eq!(code.endpoint_id().unwrap(), key);
    }

    #[test]
    fn decode_rejects_a_malformed_token() {
        let code = sample();
        // Too short, and not base64url at all.
        assert!(InviteCode::decode(&format!("a2a1.kip.{}.abcd", code.id)).is_err());
        assert!(InviteCode::decode(&format!("a2a1.kip.{}.****************", code.id)).is_err());
    }

    #[test]
    fn a_joiner_sharing_a_name_takes_a_different_one() {
        // Two agents in one directory answer to the same name; the channel needs two.
        assert_eq!(distinct_from("Vale", "Vale"), "Vale2");
        assert_eq!(
            distinct_from("vale", "VALE"),
            "vale2",
            "case is not a difference"
        );
        assert_eq!(distinct_from("Vale2", "Vale2"), "Vale22");
    }

    #[test]
    fn a_name_that_is_already_distinct_is_left_alone() {
        assert_eq!(distinct_from("Kip", "Vale"), "Kip");
        assert_eq!(distinct_from("Kip", ""), "Kip");
    }

    #[test]
    fn tokens_match_only_on_equality() {
        let token = InviteCode::new_token();
        assert!(tokens_match(&token, &token.clone()));
        assert!(!tokens_match(&token, &InviteCode::new_token()));
        assert!(!tokens_match(&token, ""));
        assert!(!tokens_match("", ""), "an empty token never matches");
    }

    #[test]
    fn tokens_are_unpredictable() {
        let a = InviteCode::new_token();
        let b = InviteCode::new_token();
        assert_ne!(a, b);
        assert!(a.len() < TOKEN_BYTES * 2, "denser than hex: {a}");
    }

    #[test]
    fn pair_alpn_differs_from_the_message_alpn() {
        // Routing depends on these being distinct.
        assert_ne!(PAIR_ALPN, crate::wire::ALPN);
    }

    #[test]
    fn join_messages_roundtrip() {
        let request = JoinRequest {
            v: 1,
            token: InviteCode::new_token(),
            name: "codex".into(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<JoinRequest>(&json).unwrap(), request);

        for response in [
            JoinResponse::Ok {
                name: "claude".into(),
            },
            JoinResponse::Error {
                message: "no invite".into(),
            },
        ] {
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(
                serde_json::from_str::<JoinResponse>(&json).unwrap(),
                response
            );
        }
    }
}
