//! The over-the-wire protocol spoken between two `agent2agent` daemons.
//!
//! Everything here rides inside an iroh QUIC stream, which is already authenticated
//! (the remote's [`iroh::EndpointId`] *is* its public key) and encrypted with TLS 1.3.
//! So this layer carries no cryptography of its own — it only frames messages.
//!
//! Framing is a 4-byte big-endian length prefix followed by that many bytes of JSON.

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::util::{new_message_id, now_ts};

/// ALPN identifier negotiated on every connection. Bump the suffix on a breaking change.
pub const ALPN: &[u8] = b"agent2agent/1";

/// Protocol version carried in each message, so a peer can reject what it cannot parse.
pub const PROTOCOL_VERSION: u8 = 1;

/// Largest frame we will write or accept, in bytes.
pub const MAX_FRAME: usize = 1024 * 1024;

/// What a message is for.
///
/// A typed field rather than a marker in the body: leaving-the-conversation has to be
/// something a peer states, not something its prose can be mistaken for. It also means a
/// receiving agent can branch on it without parsing text it is told never to trust.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Ordinary conversation.
    #[default]
    Msg,
    /// "I am here" — opens, or reopens, the session.
    Hello,
    /// "I am leaving" — the sender will not be reading replies.
    Bye,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Msg => "msg",
            Kind::Hello => "hello",
            Kind::Bye => "bye",
        }
    }
}

/// A message sent from one agent to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMsg {
    /// Protocol version, see [`PROTOCOL_VERSION`].
    pub v: u8,
    /// Unique message id, assigned by the sender.
    pub id: String,
    /// Unix timestamp in seconds, assigned by the sender. Advisory only.
    pub ts: i64,
    /// What this message is for. Absent on the wire means [`Kind::Msg`], so a peer
    /// running an older build still parses.
    #[serde(default)]
    pub kind: Kind,
    /// The message text. May be empty for a bare `hello` or `bye`.
    pub body: String,
}

impl WireMsg {
    /// Build a new outgoing message with a fresh id and the current timestamp.
    pub fn new(body: impl Into<String>) -> Self {
        Self::of_kind(Kind::Msg, body)
    }

    pub fn of_kind(kind: Kind, body: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: new_message_id(),
            ts: now_ts(),
            kind,
            body: body.into(),
        }
    }
}

/// Sent back by the receiver once a message has been accepted into its inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub v: u8,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Ack {
    pub fn ok() -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            error: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            error: Some(message.into()),
        }
    }
}

/// Write one length-prefixed frame.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME {
        bail!(
            "frame of {} bytes exceeds the {MAX_FRAME} byte limit",
            payload.len()
        );
    }
    let len = u32::try_from(payload.len()).expect("checked against MAX_FRAME above");
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one length-prefixed frame.
///
/// Returns `Ok(None)` when the stream ended cleanly at a frame boundary — that is the
/// normal way a peer signals "no more messages". A stream that ends *mid-frame* is an
/// error, not a clean close.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    // Read the header a byte at a time rather than with `read_exact`, which cannot tell
    // "the stream ended cleanly at a boundary" from "the stream died three bytes into a
    // header" — both surface as UnexpectedEof. Conflating them would report a peer that
    // crashed mid-frame as a normal end of conversation, silently losing the message.
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < len_buf.len() {
        let read = reader
            .read(&mut len_buf[filled..])
            .await
            .context("reading frame length")?;
        if read == 0 {
            if filled == 0 {
                return Ok(None);
            }
            bail!("stream ended after {filled} of the 4 header bytes (peer died mid-frame)");
        }
        filled += read;
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        bail!("peer announced a {len} byte frame, over the {MAX_FRAME} byte limit");
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("reading frame payload (stream ended mid-frame)")?;
    Ok(Some(payload))
}

/// Serialize a value as JSON and write it as one frame.
pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value).context("serializing frame")?;
    write_frame(writer, &bytes).await
}

/// Read one frame and deserialize it from JSON. `Ok(None)` on a clean end of stream.
pub async fn read_json<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<T>> {
    match read_frame(reader).await? {
        None => Ok(None),
        Some(bytes) => {
            let value = serde_json::from_slice(&bytes).context("deserializing frame")?;
            Ok(Some(value))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").await.unwrap();
        write_frame(&mut buf, b"").await.unwrap();
        write_frame(&mut buf, b"second message").await.unwrap();

        let mut reader = Cursor::new(buf);
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"hello");
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"");
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap(),
            b"second message"
        );
        assert!(
            read_frame(&mut reader).await.unwrap().is_none(),
            "clean end of stream reads as None"
        );
    }

    #[tokio::test]
    async fn frame_prefix_is_big_endian_length() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"abc").await.unwrap();
        assert_eq!(&buf[..4], &[0, 0, 0, 3]);
        assert_eq!(&buf[4..], b"abc");
    }

    #[tokio::test]
    async fn write_rejects_oversized_frame() {
        let mut buf = Vec::new();
        let too_big = vec![0u8; MAX_FRAME + 1];
        assert!(write_frame(&mut buf, &too_big).await.is_err());
        assert!(
            buf.is_empty(),
            "nothing is written when the frame is refused"
        );
    }

    #[tokio::test]
    async fn write_accepts_frame_at_exactly_the_limit() {
        let mut buf = Vec::new();
        let exact = vec![7u8; MAX_FRAME];
        write_frame(&mut buf, &exact).await.unwrap();
        let mut reader = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap().len(),
            MAX_FRAME
        );
    }

    #[tokio::test]
    async fn read_rejects_oversized_announced_length() {
        // A hostile peer announces a huge frame; we must refuse before allocating.
        let len = (MAX_FRAME as u32 + 1).to_be_bytes();
        let mut reader = Cursor::new(len.to_vec());
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(
            err.to_string().contains("over the"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn read_rejects_truncated_payload() {
        // Announce 10 bytes, supply 3.
        let mut buf = 10u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"abc");
        let mut reader = Cursor::new(buf);
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn read_rejects_truncated_length_prefix() {
        let mut reader = Cursor::new(vec![0u8, 0u8]);
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn json_roundtrip() {
        let msg = WireMsg::new("grüße 🌍 こんにちは");
        let mut buf = Vec::new();
        write_json(&mut buf, &msg).await.unwrap();

        let mut reader = Cursor::new(buf);
        let decoded: WireMsg = read_json(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn json_read_reports_none_at_clean_eof() {
        let mut reader = Cursor::new(Vec::new());
        let decoded: Option<WireMsg> = read_json(&mut reader).await.unwrap();
        assert!(decoded.is_none());
    }

    #[tokio::test]
    async fn json_read_rejects_malformed_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{not json").await.unwrap();
        let mut reader = Cursor::new(buf);
        let decoded: Result<Option<WireMsg>> = read_json(&mut reader).await;
        assert!(decoded.is_err());
    }

    #[test]
    fn new_message_has_current_version_and_unique_id() {
        let a = WireMsg::new("x");
        let b = WireMsg::new("x");
        assert_eq!(a.v, PROTOCOL_VERSION);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn ack_error_serializes_message_and_ok_omits_it() {
        let ok = serde_json::to_string(&Ack::ok()).unwrap();
        assert!(
            !ok.contains("error"),
            "ok ack must not carry an error field: {ok}"
        );

        let err = serde_json::to_string(&Ack::error("nope")).unwrap();
        assert!(err.contains("nope"));

        let parsed: Ack = serde_json::from_str(&err).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.error.as_deref(), Some("nope"));
    }
}
