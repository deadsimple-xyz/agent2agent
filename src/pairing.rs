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
use serde::{Deserialize, Serialize};

use crate::config::{parse_endpoint_id, validate_name};

/// ALPN for the pairing handshake, distinct from the message ALPN so the accept loop can
/// route on it and apply completely different admission rules.
pub const PAIR_ALPN: &[u8] = b"agent2agent/pair/1";

/// Marker that opens every invite code, so a pasted string identifies itself.
const CODE_PREFIX: &str = "a2a1";

/// Bytes of entropy in a pairing token.
pub const TOKEN_BYTES: usize = 16;

/// How long an invite stays redeemable by default.
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// The string a user copies from one agent's chat into another's.
///
/// Wire form: `a2a1.<name>.<endpoint id>.<token>` — dot-separated, and none of the parts
/// can contain a dot, so parsing is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteCode {
    /// What the inviter calls itself; the joiner files it under this name.
    pub name: String,
    /// The inviter's endpoint id, in hex.
    pub id: String,
    /// Single-use token, in hex.
    pub token: String,
}

impl InviteCode {
    pub fn encode(&self) -> String {
        format!("{CODE_PREFIX}.{}.{}.{}", self.name, self.id, self.token)
    }

    pub fn decode(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let parts: Vec<&str> = trimmed.split('.').collect();
        if parts.len() != 4 {
            bail!(
                "invite code should have 4 dot-separated parts, found {}",
                parts.len()
            );
        }
        if parts[0] != CODE_PREFIX {
            bail!("not an agent2agent invite code (expected it to start with {CODE_PREFIX}.)");
        }

        let code = Self {
            name: parts[1].to_string(),
            id: parts[2].to_string(),
            token: parts[3].to_string(),
        };

        // Validate every part now, so a mistyped code fails here with a clear message
        // rather than somewhere inside the dial.
        validate_name(&code.name).context("invite code carries an invalid peer name")?;
        parse_endpoint_id(&code.id).context("invite code carries an invalid endpoint id")?;
        if code.token.len() != TOKEN_BYTES * 2 || !code.token.chars().all(|c| c.is_ascii_hexdigit())
        {
            bail!("invite code carries a malformed token");
        }
        Ok(code)
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
    use crate::util::random_hex;
    use iroh::SecretKey;

    fn sample() -> InviteCode {
        InviteCode {
            name: "claude".into(),
            id: SecretKey::generate().public().to_string(),
            token: random_hex(TOKEN_BYTES),
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
        assert!(InviteCode::decode(&format!("{}.extra", code.encode())).is_err());
        assert!(InviteCode::decode("a2a1.name.id").is_err());
    }

    #[test]
    fn decode_rejects_a_bad_endpoint_id() {
        let token = random_hex(TOKEN_BYTES);
        assert!(InviteCode::decode(&format!("a2a1.claude.not-a-key.{token}")).is_err());
    }

    #[test]
    fn decode_rejects_a_bad_name() {
        let code = sample();
        let bad = format!("a2a1.has space.{}.{}", code.id, code.token);
        assert!(InviteCode::decode(&bad).is_err());
    }

    #[test]
    fn decode_rejects_a_malformed_token() {
        let code = sample();
        // Too short, and non-hex of the right length.
        assert!(InviteCode::decode(&format!("a2a1.claude.{}.abcd", code.id)).is_err());
        let wrong = "z".repeat(TOKEN_BYTES * 2);
        assert!(InviteCode::decode(&format!("a2a1.claude.{}.{wrong}", code.id)).is_err());
    }

    #[test]
    fn tokens_match_only_on_equality() {
        let token = random_hex(TOKEN_BYTES);
        assert!(tokens_match(&token, &token.clone()));
        assert!(!tokens_match(&token, &random_hex(TOKEN_BYTES)));
        assert!(!tokens_match(&token, ""));
        assert!(!tokens_match("", ""), "an empty token never matches");
    }

    #[test]
    fn tokens_are_unpredictable() {
        let a = random_hex(TOKEN_BYTES);
        let b = random_hex(TOKEN_BYTES);
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_BYTES * 2);
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
            token: random_hex(TOKEN_BYTES),
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
