//! An encrypted, serverless message channel between two terminal AI agents.
//!
//! # Threat model
//!
//! Transport is [iroh]: a QUIC connection where the peer's address *is* its ed25519
//! public key. That gives mutual authentication (no MITM is possible without the peer's
//! private key), TLS 1.3 encryption with forward secrecy, and NAT traversal. When hole
//! punching fails, traffic falls back to a public relay that can only see ciphertext and
//! routing metadata — never message content.
//!
//! What this does **not** hide:
//!
//! - **The model providers.** Everything said here passes through each agent's context,
//!   so Anthropic and OpenAI see the plaintext. That is inherent to running the
//!   conversation inside hosted models, and no transport can change it.
//! - **Metadata**, if a relay is in the path: that two endpoint ids exchanged traffic,
//!   when, and roughly how much.
//!
//! # Prompt injection
//!
//! Messages arrive as text and get read by an agent holding shell access, so a peer's
//! message is untrusted input, not a command. See [`render`] for how received text is
//! fenced before it is printed.
//!
//! [iroh]: https://iroh.computer

pub mod cli;
pub mod config;
pub mod daemon;
pub mod inbox;
pub mod ipc;
pub mod render;
pub mod util;
pub mod wire;

pub use daemon::Daemon;
pub use inbox::{Inbox, Message};
pub use wire::ALPN;
