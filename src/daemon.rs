//! The long-running process: one iroh endpoint outward, one unix socket inward.
//!
//! It exists so that hole punching happens once rather than on every `send`, and so a
//! message that arrives while no CLI is attached still lands somewhere.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::endpoint::{presets, Connection, Incoming, RecvStream, SendStream, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::config::{load_or_create_secret_key, Paths, Peers};
use crate::inbox::{Inbox, Message};
use crate::ipc::{self, Request, Response, ResponseData, StatusInfo};
use crate::wire::{read_json, write_json, Ack, WireMsg, ALPN, PROTOCOL_VERSION};

/// QUIC close code for a connection from an endpoint id we do not know.
const CLOSE_UNAUTHORIZED: u32 = 1;

/// Tunables. The defaults are what the daemon runs with; tests shorten the timeout so a
/// deliberately unreachable peer does not cost half a minute of wall clock.
#[derive(Debug, Clone)]
pub struct Options {
    /// Messages held before the oldest are dropped.
    pub inbox_capacity: usize,
    /// How long a single outbound delivery may take, including connection setup.
    pub send_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            inbox_capacity: crate::inbox::DEFAULT_CAPACITY,
            send_timeout: Duration::from_secs(30),
        }
    }
}

/// Shared daemon state.
#[derive(Debug)]
pub struct Daemon {
    paths: Paths,
    endpoint: Endpoint,
    inbox: Arc<Inbox>,
    peers: RwLock<Peers>,
    options: Options,
    /// Live outbound connections, keyed by peer. Reused so that only the first message
    /// to a peer pays for hole punching.
    connections: Mutex<HashMap<EndpointId, Connection>>,
}

impl Daemon {
    /// Assemble a daemon around an already-bound endpoint.
    ///
    /// Takes the endpoint rather than building one so tests can supply a relay-less,
    /// discovery-less endpoint and run entirely offline.
    pub fn new(paths: Paths, endpoint: Endpoint, peers: Peers) -> Arc<Self> {
        Self::with_options(paths, endpoint, peers, Options::default())
    }

    pub fn with_options(
        paths: Paths,
        endpoint: Endpoint,
        peers: Peers,
        options: Options,
    ) -> Arc<Self> {
        Arc::new(Self {
            paths,
            endpoint,
            inbox: Arc::new(Inbox::new(options.inbox_capacity)),
            peers: RwLock::new(peers),
            options,
            connections: Mutex::new(HashMap::new()),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// This node's endpoint id, the string peers put in their `peers.toml`.
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    // ---------------------------------------------------------------- inbound

    /// Accept iroh connections until the endpoint closes.
    pub fn spawn_accept_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(incoming) = this.endpoint.accept().await {
                let this = this.clone();
                tokio::spawn(async move { this.handle_incoming(incoming).await });
            }
            debug!("accept loop finished");
        })
    }

    async fn handle_incoming(self: Arc<Self>, incoming: Incoming) {
        let connection = match incoming.await {
            Ok(connection) => connection,
            Err(e) => {
                debug!(error = %e, "inbound connection failed during handshake");
                return;
            }
        };

        // Authorization. iroh has already proved the remote holds the private key for
        // this id; the only question left is whether we chose to talk to it.
        let remote = connection.remote_id();
        let Some(peer_name) = self.peers.read().await.name_for(&remote) else {
            warn!(
                remote = %remote,
                "refused connection from an endpoint id that is not in peers.toml"
            );
            connection.close(
                VarInt::from_u32(CLOSE_UNAUTHORIZED),
                b"not an authorized peer",
            );
            return;
        };

        info!(peer = %peer_name, "peer connected");
        loop {
            match connection.accept_bi().await {
                Ok((send, recv)) => {
                    let this = self.clone();
                    let peer_name = peer_name.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.handle_stream(&peer_name, send, recv).await {
                            warn!(peer = %peer_name, error = %e, "failed to handle inbound message");
                        }
                    });
                }
                Err(e) => {
                    debug!(peer = %peer_name, error = %e, "peer connection closed");
                    break;
                }
            }
        }
    }

    async fn handle_stream(
        &self,
        peer_name: &str,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<()> {
        let Some(message) = read_json::<_, WireMsg>(&mut recv).await? else {
            // Peer opened a stream and closed it without sending anything.
            return Ok(());
        };

        if message.v != PROTOCOL_VERSION {
            let ack = Ack::error(format!(
                "unsupported protocol version {} (this daemon speaks {PROTOCOL_VERSION})",
                message.v
            ));
            write_json(&mut send, &ack).await?;
            let _ = send.finish();
            bail!("peer sent protocol version {}", message.v);
        }

        let evicted = self.inbox.push(Message {
            peer: peer_name.to_string(),
            id: message.id.clone(),
            ts: message.ts,
            body: message.body,
        });
        if let Some(evicted) = evicted {
            warn!(
                peer = %evicted.peer,
                id = %evicted.id,
                "inbox is full, dropped the oldest message"
            );
        }

        write_json(&mut send, &Ack::ok()).await?;
        let _ = send.finish();
        info!(peer = %peer_name, id = %message.id, "message received");
        Ok(())
    }

    // --------------------------------------------------------------- outbound

    /// Deliver `body` to a peer. Returns the resolved peer name and the message id.
    pub async fn send(&self, peer: Option<&str>, body: &str) -> Result<(String, String)> {
        let (name, addr) = {
            let peers = self.peers.read().await;
            let (name, peer) = peers.resolve(peer)?;
            (name, peer.endpoint_addr()?)
        };

        let message = WireMsg::new(body);
        let timeout = self.options.send_timeout;
        let deliver = self.deliver(&addr, &message);
        tokio::time::timeout(timeout, deliver)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "timed out after {timeout:?} delivering to peer {name:?}; is its daemon running?"
                )
            })?
            .with_context(|| format!("delivering to peer {name:?}"))?;

        Ok((name, message.id))
    }

    /// One delivery, retried once so that a cached connection the peer has since dropped
    /// costs a reconnect rather than a user-visible error.
    async fn deliver(&self, addr: &EndpointAddr, message: &WireMsg) -> Result<()> {
        let mut last_error = None;
        for attempt in 0..2 {
            let connection = self.connection_for(addr).await?;
            match Self::deliver_once(&connection, message).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    debug!(attempt, error = %e, "delivery attempt failed");
                    self.forget_connection(&addr.id).await;
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.expect("loop runs at least once and only exits here on error"))
    }

    async fn deliver_once(connection: &Connection, message: &WireMsg) -> Result<()> {
        let (mut send, mut recv) = connection.open_bi().await?;
        write_json(&mut send, message).await?;
        send.finish()?;

        let ack: Ack = read_json(&mut recv)
            .await?
            .context("peer closed the stream without acknowledging")?;
        if !ack.ok {
            bail!(
                "peer rejected the message: {}",
                ack.error.as_deref().unwrap_or("no reason given")
            );
        }
        Ok(())
    }

    async fn connection_for(&self, addr: &EndpointAddr) -> Result<Connection> {
        if let Some(connection) = self.cached_connection(&addr.id).await {
            return Ok(connection);
        }
        let connection = self
            .endpoint
            .connect(addr.clone(), ALPN)
            .await
            .context("cannot reach the peer")?;
        self.connections
            .lock()
            .await
            .insert(addr.id, connection.clone());
        Ok(connection)
    }

    async fn cached_connection(&self, id: &EndpointId) -> Option<Connection> {
        let mut connections = self.connections.lock().await;
        match connections.get(id) {
            Some(connection) if connection.close_reason().is_none() => Some(connection.clone()),
            Some(_) => {
                connections.remove(id);
                None
            }
            None => None,
        }
    }

    async fn forget_connection(&self, id: &EndpointId) {
        self.connections.lock().await.remove(id);
    }

    // -------------------------------------------------------------- local IPC

    /// Apply one CLI request.
    pub async fn handle(&self, request: Request) -> Response {
        match request {
            Request::Send { peer, body } => match self.send(peer.as_deref(), &body).await {
                Ok((peer, id)) => Response::ok(ResponseData::Sent { peer, id }),
                Err(e) => Response::error(format!("{e:#}")),
            },

            Request::Recv { peer, wait_ms } => {
                // Waiting on a peer that is not configured would block until the
                // deadline for no reason; say so straight away instead.
                if let Some(name) = peer.as_deref() {
                    if !self.peers.read().await.peers.contains_key(name) {
                        return Response::error(format!("unknown peer {name:?}"));
                    }
                }
                match self
                    .inbox
                    .pop_wait(peer.as_deref(), Duration::from_millis(wait_ms))
                    .await
                {
                    Some(message) => Response::ok(ResponseData::Message { message }),
                    None => Response::ok(ResponseData::NoMessage),
                }
            }

            Request::Status => {
                let peers = self.peers.read().await;
                Response::ok(ResponseData::Status(StatusInfo {
                    id: self.id().to_string(),
                    peers: peers
                        .peers
                        .iter()
                        .map(|(name, peer)| (name.clone(), peer.id.clone()))
                        .collect(),
                    default_peer: peers.default.clone(),
                    queued: self.inbox.counts(),
                    queued_total: self.inbox.len(),
                }))
            }

            Request::Reload => match self.reload_peers().await {
                Ok(count) => {
                    info!(peers = count, "reloaded peer list");
                    Response::ok(ResponseData::Done)
                }
                Err(e) => Response::error(format!("{e:#}")),
            },
        }
    }

    /// Re-read `peers.toml`. Returns how many peers are now configured.
    pub async fn reload_peers(&self) -> Result<usize> {
        let loaded = Peers::load(&self.paths.peers())?;
        let count = loaded.peers.len();
        *self.peers.write().await = loaded;
        Ok(count)
    }

    /// Serve CLI connections until the listener fails.
    pub async fn serve_ipc(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .context("accepting a CLI connection")?;
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.serve_one(stream).await {
                    debug!(error = %e, "CLI connection ended with an error");
                }
            });
        }
    }

    async fn serve_one(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(()); // Client hung up before saying anything.
        }

        let response = match serde_json::from_str::<Request>(line.trim_end()) {
            Ok(request) => self.handle(request).await,
            Err(e) => Response::error(format!("malformed request: {e}")),
        };

        let mut encoded = serde_json::to_string(&response).context("serializing reply")?;
        encoded.push('\n');
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.flush().await?;
        Ok(())
    }
}

/// Build the production endpoint: n0 discovery and relays, so a bare endpoint id is
/// enough to reach a peer from anywhere.
pub async fn build_endpoint(secret_key: SecretKey) -> Result<Endpoint> {
    Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("binding the iroh endpoint")
}

/// Bind the CLI socket, refusing to start next to a daemon that is already running and
/// clearing away a socket file left by one that crashed.
pub async fn bind_ipc(paths: &Paths) -> Result<UnixListener> {
    paths.ensure_dir()?;
    let socket = paths.socket();
    check_socket_path_length(&socket)?;

    if ipc::is_daemon_running(&socket).await {
        bail!(
            "a daemon is already running for {} (socket {})",
            paths.dir().display(),
            socket.display()
        );
    }
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }

    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    restrict_socket(&socket)?;
    Ok(listener)
}

/// Unix socket paths live in a fixed-size `sun_path` field: 104 bytes on macOS, 108 on
/// Linux, including the trailing NUL. Exceeding it fails deep inside `bind` with an
/// opaque OS error, so check up front and say what to do about it.
const MAX_SOCKET_PATH: usize = 103;

fn check_socket_path_length(socket: &Path) -> Result<()> {
    let len = socket.as_os_str().as_encoded_bytes().len();
    if len > MAX_SOCKET_PATH {
        bail!(
            "socket path {} is {len} bytes, over the {MAX_SOCKET_PATH} byte limit for unix \
             sockets; point AGENT2AGENT_HOME (or --home) at a shorter directory",
            socket.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_socket(_path: &Path) -> Result<()> {
    Ok(())
}

/// Run the daemon until interrupted.
pub async fn run(paths: Paths) -> Result<()> {
    paths.ensure_dir()?;
    let secret_key = load_or_create_secret_key(&paths)?;
    let peers = Peers::load(&paths.peers())?;
    let peer_count = peers.peers.len();

    // Bind the socket before the endpoint so a duplicate daemon fails fast and cheap.
    let listener = bind_ipc(&paths).await?;
    let endpoint = build_endpoint(secret_key).await?;

    let daemon = Daemon::new(paths.clone(), endpoint, peers);
    daemon.spawn_accept_loop();

    info!(
        id = %daemon.id(),
        peers = peer_count,
        socket = %paths.socket().display(),
        "agent2agent daemon ready"
    );
    if peer_count == 0 {
        warn!("no peers configured; add one with `agent2agent peer add <name> <id>`");
    }

    let result = tokio::select! {
        result = daemon.clone().serve_ipc(listener) => result,
        _ = tokio::signal::ctrl_c() => {
            info!("interrupted, shutting down");
            Ok(())
        }
    };

    // Leave no stale socket behind for the next start to trip over.
    let _ = std::fs::remove_file(paths.socket());
    daemon.endpoint.close().await;
    result
}
