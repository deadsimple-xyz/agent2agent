//! Command-line surface. Both agents drive the same binary, so there is nothing
//! Claude-specific or Codex-specific here — each side just runs shell commands.

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::{load_or_create_secret_key, Paths, Peers};
use crate::daemon;
use crate::ipc::{self, Request, ResponseData};
use crate::render::{render_json, render_message};

/// Exit code for `recv` reaching its deadline with no message. Distinct from a real
/// failure so a calling script can tell "nothing yet" from "something broke".
pub const EXIT_NO_MESSAGE: u8 = 3;

/// Slack added to the IPC deadline on top of a long-polling `recv`.
const IPC_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(
    name = "agent2agent",
    version,
    about = "Encrypted peer-to-peer message channel between terminal AI agents",
    long_about = "Encrypted peer-to-peer message channel between terminal AI agents.\n\n\
                  Identity is an ed25519 public key: the string printed by `agent2agent id`\n\
                  is both the address and the key, so there is nothing to verify separately\n\
                  and no server to trust. Pair once by exchanging those strings."
)]
pub struct Cli {
    /// State directory (default: $AGENT2AGENT_HOME, else ~/.config/agent2agent).
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print this node's endpoint id — give it to the peer to pair.
    Id {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Manage the peer list.
    Peer {
        #[command(subcommand)]
        action: PeerAction,
    },

    /// Send a message to a peer.
    Send {
        /// Peer name. Defaults to the default peer.
        #[arg(long, short = 't', value_name = "NAME")]
        to: Option<String>,

        /// Message text. Read from stdin when omitted.
        #[arg(trailing_var_arg = true)]
        message: Vec<String>,
    },

    /// Take one message from the inbox.
    Recv {
        /// Only accept messages from this peer.
        #[arg(long, short = 'f', value_name = "NAME")]
        from: Option<String>,

        /// Seconds to wait for a message. 0 returns immediately.
        #[arg(long, short = 'w', default_value_t = 0, value_name = "SECS")]
        wait: u64,

        /// Print as JSON instead of the delimited human form.
        #[arg(long)]
        json: bool,
    },

    /// Show identity, peers and queue depth.
    Status {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Run the daemon in the foreground.
    Daemon,
}

#[derive(Debug, Subcommand)]
pub enum PeerAction {
    /// Add or update a peer.
    Add {
        /// Local name for the peer, e.g. `codex`.
        name: String,
        /// The peer's endpoint id, from its `agent2agent id`.
        id: String,
    },
    /// List configured peers.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Remove a peer.
    Remove { name: String },
    /// Set the peer used when `--to`/`--from` is omitted.
    Default { name: String },
}

impl Cli {
    fn paths(&self) -> Result<Paths> {
        match &self.home {
            Some(dir) => Ok(Paths::from_dir(dir)),
            None => Paths::resolve(),
        }
    }
}

/// Run a parsed command.
pub async fn run(cli: Cli) -> Result<ExitCode> {
    let paths = cli.paths()?;

    match cli.command {
        Command::Daemon => {
            init_tracing();
            daemon::run(paths).await?;
            Ok(ExitCode::SUCCESS)
        }

        Command::Id { json } => {
            let key = load_or_create_secret_key(&paths)?;
            let id = key.public().to_string();
            if json {
                println!("{}", serde_json::json!({ "id": id }));
            } else {
                println!("{id}");
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Peer { action } => run_peer(&paths, action).await,

        Command::Send { to, message } => {
            let body = collect_message(message)?;
            let response = ipc::request(
                &paths.socket(),
                &Request::Send { peer: to, body },
                daemon_send_timeout(),
            )
            .await?;

            match response.into_data()? {
                ResponseData::Sent { peer, id } => {
                    eprintln!("sent to {peer} ({id})");
                    Ok(ExitCode::SUCCESS)
                }
                other => bail!("unexpected reply from the daemon: {other:?}"),
            }
        }

        Command::Recv { from, wait, json } => {
            let wait = Duration::from_secs(wait);
            let response = ipc::request(
                &paths.socket(),
                &Request::Recv {
                    peer: from,
                    wait_ms: wait.as_millis() as u64,
                },
                wait + IPC_GRACE,
            )
            .await?;

            match response.into_data()? {
                ResponseData::Message { message } => {
                    if json {
                        println!("{}", render_json(&message)?);
                    } else {
                        println!("{}", render_message(&message));
                    }
                    Ok(ExitCode::SUCCESS)
                }
                ResponseData::NoMessage => {
                    eprintln!("no message");
                    Ok(ExitCode::from(EXIT_NO_MESSAGE))
                }
                other => bail!("unexpected reply from the daemon: {other:?}"),
            }
        }

        Command::Status { json } => {
            let response =
                ipc::request(&paths.socket(), &Request::Status, Duration::from_secs(10)).await?;

            match response.into_data()? {
                ResponseData::Status(status) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&status)?);
                    } else {
                        println!("id:      {}", status.id);
                        println!("default: {}", status.default_peer.as_deref().unwrap_or("-"));
                        println!("queued:  {}", status.queued_total);
                        if status.peers.is_empty() {
                            println!("peers:   none configured");
                        } else {
                            println!("peers:");
                            for (name, id) in &status.peers {
                                let queued = status.queued.get(name).copied().unwrap_or(0);
                                println!("  {name}  {id}  ({queued} queued)");
                            }
                        }
                    }
                    Ok(ExitCode::SUCCESS)
                }
                other => bail!("unexpected reply from the daemon: {other:?}"),
            }
        }
    }
}

async fn run_peer(paths: &Paths, action: PeerAction) -> Result<ExitCode> {
    let path = paths.peers();
    let mut peers = Peers::load(&path)?;

    match action {
        PeerAction::Add { name, id } => {
            peers.add(&name, &id)?;
            peers.save(&path)?;
            eprintln!("added peer {name}");
            nudge_daemon(paths).await;
        }
        PeerAction::Remove { name } => {
            if !peers.remove(&name) {
                bail!("unknown peer {name:?}");
            }
            peers.save(&path)?;
            eprintln!("removed peer {name}");
            nudge_daemon(paths).await;
        }
        PeerAction::Default { name } => {
            peers.set_default(&name)?;
            peers.save(&path)?;
            eprintln!("default peer is now {name}");
            nudge_daemon(paths).await;
        }
        PeerAction::List { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&peers)?);
            } else if peers.peers.is_empty() {
                println!("no peers configured");
            } else {
                for (name, peer) in &peers.peers {
                    let marker = if peers.default.as_deref() == Some(name.as_str()) {
                        "*"
                    } else {
                        " "
                    };
                    println!("{marker} {name}  {}", peer.id);
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Ask a running daemon to re-read `peers.toml`. Best effort: editing the peer list
/// with no daemon running is perfectly normal.
async fn nudge_daemon(paths: &Paths) {
    let _ = ipc::request(&paths.socket(), &Request::Reload, Duration::from_secs(5)).await;
}

/// Join the message arguments, or read stdin when none were given.
fn collect_message(parts: Vec<String>) -> Result<String> {
    let body = if parts.is_empty() {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .context("reading the message from stdin")?;
        buffer.trim_end_matches('\n').to_string()
    } else {
        parts.join(" ")
    };

    if body.is_empty() {
        bail!("refusing to send an empty message");
    }
    Ok(body)
}

/// `send` has to outlast the daemon's own delivery timeout, or the CLI would give up
/// first and report a timeout for a message that was still in flight.
fn daemon_send_timeout() -> Duration {
    Duration::from_secs(45)
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("AGENT2AGENT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("agent2agent=info,warn"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn send_joins_trailing_arguments() {
        let cli = Cli::try_parse_from(["agent2agent", "send", "hello", "there", "friend"]).unwrap();
        match cli.command {
            Command::Send { to, message } => {
                assert_eq!(to, None);
                assert_eq!(collect_message(message).unwrap(), "hello there friend");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn send_accepts_a_target_peer() {
        let cli = Cli::try_parse_from(["agent2agent", "send", "--to", "codex", "hi"]).unwrap();
        match cli.command {
            Command::Send { to, .. } => assert_eq!(to.as_deref(), Some("codex")),
            other => panic!("parsed as {other:?}"),
        }

        let short = Cli::try_parse_from(["agent2agent", "send", "-t", "codex", "hi"]).unwrap();
        match short.command {
            Command::Send { to, .. } => assert_eq!(to.as_deref(), Some("codex")),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn recv_defaults_to_no_waiting() {
        let cli = Cli::try_parse_from(["agent2agent", "recv"]).unwrap();
        match cli.command {
            Command::Recv { from, wait, json } => {
                assert_eq!(from, None);
                assert_eq!(wait, 0);
                assert!(!json);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn recv_accepts_a_wait_and_a_source() {
        let cli = Cli::try_parse_from([
            "agent2agent",
            "recv",
            "--from",
            "codex",
            "--wait",
            "120",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Command::Recv { from, wait, json } => {
                assert_eq!(from.as_deref(), Some("codex"));
                assert_eq!(wait, 120);
                assert!(json);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn home_is_accepted_before_or_after_the_subcommand() {
        let before = Cli::try_parse_from(["agent2agent", "--home", "/tmp/a", "status"]).unwrap();
        assert_eq!(before.home.unwrap(), PathBuf::from("/tmp/a"));

        let after = Cli::try_parse_from(["agent2agent", "status", "--home", "/tmp/b"]).unwrap();
        assert_eq!(after.home.unwrap(), PathBuf::from("/tmp/b"));
    }

    #[test]
    fn home_overrides_the_resolved_default() {
        let cli =
            Cli::try_parse_from(["agent2agent", "--home", "/tmp/explicit", "status"]).unwrap();
        assert_eq!(cli.paths().unwrap().dir(), PathBuf::from("/tmp/explicit"));
    }

    #[test]
    fn peer_subcommands_parse() {
        let cli = Cli::try_parse_from(["agent2agent", "peer", "add", "codex", "abc123"]).unwrap();
        match cli.command {
            Command::Peer {
                action: PeerAction::Add { name, id },
            } => {
                assert_eq!(name, "codex");
                assert_eq!(id, "abc123");
            }
            other => panic!("parsed as {other:?}"),
        }

        assert!(Cli::try_parse_from(["agent2agent", "peer", "remove", "codex"]).is_ok());
        assert!(Cli::try_parse_from(["agent2agent", "peer", "default", "codex"]).is_ok());
        assert!(Cli::try_parse_from(["agent2agent", "peer", "list"]).is_ok());
    }

    #[test]
    fn peer_add_requires_both_arguments() {
        assert!(Cli::try_parse_from(["agent2agent", "peer", "add", "codex"]).is_err());
        assert!(Cli::try_parse_from(["agent2agent", "peer", "add"]).is_err());
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(Cli::try_parse_from(["agent2agent"]).is_err());
        assert!(Cli::try_parse_from(["agent2agent", "nonsense"]).is_err());
    }

    #[test]
    fn empty_messages_are_refused() {
        // Only the argument path is exercised here: the empty-argument case reads stdin,
        // which a test runner must not be made to block on.
        assert!(collect_message(vec![String::new()]).is_err());
        assert!(collect_message(vec![String::new(), String::new()]).is_ok_and(|s| s == " "));
    }

    #[test]
    fn message_words_are_joined_with_single_spaces() {
        assert_eq!(
            collect_message(vec!["a".into(), "b".into(), "c".into()]).unwrap(),
            "a b c"
        );
    }

    #[test]
    fn no_message_exit_code_is_distinct_from_success_and_failure() {
        assert_ne!(EXIT_NO_MESSAGE, 0);
        assert_ne!(EXIT_NO_MESSAGE, 1);
    }
}
