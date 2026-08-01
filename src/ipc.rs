//! The local control channel between the `agent2agent` CLI and its daemon.
//!
//! A unix socket in the state directory, one request per connection, newline-delimited
//! JSON. The daemon holds the connection open for the duration of a long-polling
//! `recv`, so the CLI blocks in a single read instead of spinning.
//!
//! This socket is a local trust boundary only: anyone who can open it can send messages
//! as this agent. It lives in a `0700` directory, which is the same protection the
//! secret key gets.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::inbox::Message;
use crate::wire::Kind;

/// A request from the CLI to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Deliver a message to a peer. `peer` absent means the default peer.
    Send {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer: Option<String>,
        body: String,
        /// Conversation, an arrival, or a departure.
        #[serde(default)]
        kind: Kind,
        /// The operator has already approved this message.
        ///
        /// Manual mode is enforced here rather than in the CLI: it is the default posture,
        /// and a guard only the CLI applies is bypassed by anything else that opens the
        /// socket.
        #[serde(default)]
        confirmed: bool,
    },
    /// Take one message from the inbox, waiting up to `wait_ms`.
    Recv {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer: Option<String>,
        #[serde(default)]
        wait_ms: u64,
    },
    /// Open a pairing invite and return the code to hand to the other agent.
    Invite {
        /// What we call ourselves in the code.
        name: String,
        /// Message delivered to the joiner the moment pairing succeeds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        greeting: Option<String>,
        #[serde(default)]
        ttl_secs: u64,
    },
    /// Redeem an invite code produced by the other agent.
    Join {
        code: String,
        /// What we call ourselves to the inviter.
        name: String,
    },
    /// Report identity, peers and queue depth.
    Status,
    /// Read or change whether messages wait for operator approval.
    Mode {
        /// Absent means "report the current mode".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<String>,
    },
    /// Re-read `peers.toml` from disk.
    Reload,
    /// Stop the daemon. Sent when a conversation ends and its state is discarded.
    Shutdown,
}

/// The daemon's reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok { data: ResponseData },
    Error { message: String },
}

impl Response {
    pub fn error(message: impl std::fmt::Display) -> Self {
        Self::Error {
            message: message.to_string(),
        }
    }

    pub fn ok(data: ResponseData) -> Self {
        Self::Ok { data }
    }

    /// Unwrap into the payload, turning a daemon-side error into a local one.
    pub fn into_data(self) -> Result<ResponseData> {
        match self {
            Response::Ok { data } => Ok(data),
            Response::Error { message } => bail!(message),
        }
    }
}

/// Payloads the daemon can return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseData {
    /// A message was accepted by the peer.
    Sent { peer: String, id: String },
    /// Manual mode: nothing was sent, the operator has to agree first.
    NeedsApproval { peer: String },
    /// Nothing was sent: that peer said goodbye and is not reading replies.
    PeerGone { peer: String },
    /// A message was taken off the inbox.
    Message { message: Message },
    /// `recv` reached its deadline with nothing queued.
    NoMessage,
    /// An invite was opened.
    Invite { code: String },
    /// An invite was redeemed; `peer` is the local name of the other agent.
    Joined { peer: String },
    /// The current mode, after any change.
    Mode { mode: String },
    /// Daemon state.
    Status(StatusInfo),
    /// Acknowledgement with nothing to report.
    Done,
}

/// What `agent2agent status` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    /// This node's endpoint id.
    pub id: String,
    /// Configured peers, name to endpoint id.
    pub peers: BTreeMap<String, String>,
    /// Default peer, if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_peer: Option<String>,
    /// `auto` or `manual`.
    #[serde(default)]
    pub mode: String,
    /// Whether an invite is currently redeemable.
    #[serde(default)]
    pub invite_open: bool,
    /// Peers that have said goodbye and are not reading replies.
    #[serde(default)]
    pub departed: Vec<String>,
    /// Queued messages per peer.
    pub queued: BTreeMap<String, usize>,
    /// Total queued messages.
    pub queued_total: usize,
}

/// Send one request to the daemon and read its reply.
///
/// `timeout` bounds the whole exchange, so it must exceed the `wait_ms` of a long-polling
/// [`Request::Recv`].
pub async fn request(socket: &Path, req: &Request, timeout: Duration) -> Result<Response> {
    tokio::time::timeout(timeout, exchange(socket, req))
        .await
        .map_err(|_| anyhow::anyhow!("daemon did not reply within {timeout:?}"))?
}

async fn exchange(socket: &Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "cannot reach the daemon at {}; is it running? (`agent2agent daemon` or `brew services start agent2agent`)",
            socket.display()
        )
    })?;
    let (read_half, mut write_half) = stream.into_split();

    let mut line = serde_json::to_string(req).context("serializing request")?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut response = String::new();
    let read = reader.read_line(&mut response).await?;
    if read == 0 {
        bail!("daemon closed the connection without replying");
    }
    serde_json::from_str(response.trim_end())
        .with_context(|| format!("parsing daemon reply {:?}", response.trim_end()))
}

/// Whether a daemon is listening on `socket`.
///
/// Used both to refuse a second daemon and to tell a live socket from a stale file left
/// behind by a crash.
pub async fn is_daemon_running(socket: &Path) -> bool {
    if !socket.exists() {
        return false;
    }
    matches!(
        request(socket, &Request::Status, Duration::from_secs(2)).await,
        Ok(Response::Ok { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn requests_roundtrip() {
        let cases = vec![
            Request::Send {
                peer: Some("codex".into()),
                body: "hello".into(),
                kind: Kind::Msg,
                confirmed: true,
            },
            Request::Send {
                peer: None,
                body: String::new(),
                kind: Kind::Bye,
                confirmed: true,
            },
            Request::Recv {
                peer: None,
                wait_ms: 0,
            },
            Request::Recv {
                peer: Some("gemini".into()),
                wait_ms: 120_000,
            },
            Request::Status,
            Request::Reload,
        ];
        for case in cases {
            assert_eq!(roundtrip(&case), case);
        }
    }

    #[test]
    fn requests_are_tagged_by_cmd() {
        let json = serde_json::to_string(&Request::Status).unwrap();
        assert_eq!(json, r#"{"cmd":"status"}"#);

        let json = serde_json::to_string(&Request::Send {
            peer: None,
            body: "hi".into(),
            kind: Kind::Msg,
            confirmed: true,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"cmd":"send","body":"hi","kind":"msg","confirmed":true}"#
        );
    }

    #[test]
    fn recv_defaults_are_optional_on_the_wire() {
        let parsed: Request = serde_json::from_str(r#"{"cmd":"recv"}"#).unwrap();
        assert_eq!(
            parsed,
            Request::Recv {
                peer: None,
                wait_ms: 0
            }
        );
    }

    #[test]
    fn unknown_command_is_rejected_rather_than_silently_accepted() {
        let parsed: Result<Request, _> = serde_json::from_str(r#"{"cmd":"selfdestruct"}"#);
        assert!(parsed.is_err());
    }

    #[test]
    fn responses_roundtrip() {
        let message = Message {
            peer: "codex".into(),
            id: "id1".into(),
            ts: 42,
            kind: Kind::Msg,
            body: "body".into(),
        };
        let cases = vec![
            Response::ok(ResponseData::Sent {
                peer: "codex".into(),
                id: "id1".into(),
            }),
            Response::ok(ResponseData::Message { message }),
            Response::ok(ResponseData::NoMessage),
            Response::ok(ResponseData::NeedsApproval {
                peer: "codex".into(),
            }),
            Response::ok(ResponseData::PeerGone {
                peer: "codex".into(),
            }),
            Response::ok(ResponseData::Done),
            Response::ok(ResponseData::Status(StatusInfo {
                id: "abc".into(),
                peers: BTreeMap::from([("codex".to_string(), "xyz".to_string())]),
                default_peer: Some("codex".into()),
                mode: "auto".into(),
                invite_open: true,
                departed: vec!["codex".to_string()],
                queued: BTreeMap::from([("codex".to_string(), 2)]),
                queued_total: 2,
            })),
            Response::error("boom"),
        ];
        for case in cases {
            assert_eq!(roundtrip(&case), case);
        }
    }

    #[test]
    fn into_data_surfaces_daemon_errors() {
        let err = Response::error("no such peer").into_data().unwrap_err();
        assert_eq!(err.to_string(), "no such peer");

        let data = Response::ok(ResponseData::Done).into_data().unwrap();
        assert_eq!(data, ResponseData::Done);
    }

    #[test]
    fn responses_stay_on_one_line_even_with_multiline_bodies() {
        // The framing is newline-delimited, so an embedded newline must be escaped.
        let response = Response::ok(ResponseData::Message {
            message: Message {
                peer: "codex".into(),
                id: "id".into(),
                ts: 0,
                kind: Kind::Msg,
                body: "first\nsecond".into(),
            },
        });
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json.lines().count(), 1);
        assert_eq!(roundtrip(&response), response);
    }

    #[tokio::test]
    async fn request_reports_a_missing_daemon_clearly() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("absent.sock");

        let err = request(&socket, &Request::Status, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot reach the daemon"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn is_daemon_running_is_false_for_a_missing_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!is_daemon_running(&dir.path().join("absent.sock")).await);
    }

    #[tokio::test]
    async fn is_daemon_running_is_false_for_a_stale_socket_file() {
        // A crashed daemon leaves the file behind with nobody listening.
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("stale.sock");
        std::fs::write(&socket, b"").unwrap();
        assert!(!is_daemon_running(&socket).await);
    }
}
