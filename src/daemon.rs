//! The long-running process: one iroh endpoint outward, one unix socket inward.
//!
//! It exists so that hole punching happens once rather than on every `send`, and so a
//! message that arrives while no CLI is attached still lands somewhere.

use std::collections::{HashMap, HashSet};
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

use crate::config::{load_or_create_secret_key, Mode, Paths, Peers};
use crate::inbox::{Inbox, Message};
use crate::ipc::{self, Request, Response, ResponseData, StatusInfo};
use crate::pairing::{
    distinct_from, is_newer, tokens_match, InviteCode, JoinRequest, JoinResponse, DEFAULT_TTL_SECS,
    PAIR_ALPN, VERSION,
};
use crate::wire::{read_json, write_json, Ack, Kind, WireMsg, ALPN, PROTOCOL_VERSION};

/// QUIC close code for a connection from an endpoint id we do not know.
const CLOSE_UNAUTHORIZED: u32 = 1;

/// How long the inviter holds a pairing connection open after replying, waiting for the
/// joiner to hang up.
const HANDSHAKE_LINGER: Duration = Duration::from_secs(10);

/// How long to wait at startup for the endpoint to become reachable.
///
/// Binding a socket is instant; being findable is not. Until discovery has published this
/// node, a peer handed its id cannot resolve an address for it — and the whole flow is
/// "invite, paste, join", which happens in seconds.
const ONLINE_TIMEOUT: Duration = Duration::from_secs(20);

/// An invite waiting to be redeemed. At most one is outstanding at a time: a second
/// `invite` replaces the first, so a code left lying around stops working.
#[derive(Debug, Clone)]
struct PendingInvite {
    token: String,
    /// What we call ourselves in the code, echoed back to the joiner.
    my_name: String,
    /// Sent to the joiner the moment pairing succeeds, so the conversation opens itself.
    greeting: Option<String>,
    expires_at: tokio::time::Instant,
}

/// Tunables. The defaults are what the daemon runs with; tests shorten the timeout so a
/// deliberately unreachable peer does not cost half a minute of wall clock.
#[derive(Debug, Clone)]
pub struct Options {
    /// Messages held before the oldest are dropped.
    pub inbox_capacity: usize,
    /// How long a single outbound delivery may take, including connection setup.
    pub send_timeout: Duration,
    /// How long `invite` and `join` wait for this node to become findable.
    ///
    /// Zero for tests: their endpoints are deliberately unreachable from the outside and
    /// would otherwise sit out the whole wait before talking over loopback anyway.
    pub online_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            inbox_capacity: crate::inbox::DEFAULT_CAPACITY,
            send_timeout: Duration::from_secs(30),
            online_timeout: ONLINE_TIMEOUT,
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
    /// The outstanding invite, if `invite` has been run and not yet redeemed.
    invite: Mutex<Option<PendingInvite>>,
    /// Raised when the conversation ends and this daemon should stop.
    shutdown: tokio::sync::Notify,
    /// Whether the operator has stepped out of the loop for the current conversation.
    ///
    /// Not persisted and not permanent: a goodbye clears it, and so does a restart. The
    /// grant is scoped to the conversation it was given for.
    mode: RwLock<Mode>,
    /// Peers that have said goodbye, or that we have said goodbye to.
    ///
    /// Held in memory rather than on disk: a daemon restart is not a departure, and after
    /// one the honest answer is "nobody has told us they left". Any traffic from a peer
    /// clears its entry — an agent that is talking is plainly still there.
    departed: RwLock<HashSet<String>>,
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
            invite: Mutex::new(None),
            shutdown: tokio::sync::Notify::new(),
            mode: RwLock::new(Mode::default()),
            departed: RwLock::new(HashSet::new()),
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

        // Pairing runs on its own ALPN and is the one path that does not require the
        // caller to be on the peer list — it is how callers get onto it.
        if connection.alpn() == PAIR_ALPN {
            if let Err(e) = self.handle_pairing(connection).await {
                warn!(error = %e, "pairing attempt failed");
            }
            return;
        }

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

        // Track presence before queueing. A peer that says anything at all is here; one
        // that says goodbye is not, and `send` must stop pretending otherwise.
        match message.kind {
            Kind::Bye => {
                self.departed.write().await.insert(peer_name.to_string());
                info!(peer = %peer_name, "peer said goodbye");
                self.end_of_conversation().await;
            }
            Kind::Msg | Kind::Hello => {
                self.departed.write().await.remove(peer_name);
            }
        }

        let evicted = self.inbox.push(Message {
            peer: peer_name.to_string(),
            id: message.id.clone(),
            ts: message.ts,
            kind: message.kind,
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

    // ---------------------------------------------------------------- pairing

    /// Open an invite and return the code to hand to the other agent.
    ///
    /// Only one invite is outstanding at a time; opening a new one silently retires the
    /// previous code.
    pub async fn create_invite(
        &self,
        my_name: &str,
        greeting: Option<String>,
        ttl: Duration,
    ) -> Result<String> {
        crate::config::validate_name(my_name)?;
        self.wait_until_reachable().await;
        let token = InviteCode::new_token();
        let code = InviteCode {
            name: my_name.to_string(),
            id: InviteCode::encode_id(&self.id()),
            token: token.clone(),
            version: Some(VERSION.to_string()),
        };

        *self.invite.lock().await = Some(PendingInvite {
            token,
            my_name: my_name.to_string(),
            greeting,
            expires_at: tokio::time::Instant::now() + ttl,
        });
        Ok(code.encode())
    }

    /// Wait until peers could actually find us.
    ///
    /// Binding a socket is instant; being published is not. Only the two commands that
    /// need reachability pay this — making the daemon withhold its socket until then meant
    /// a slow network looked like a daemon that had failed to start.
    async fn wait_until_reachable(&self) {
        let timeout = self.options.online_timeout;
        if timeout.is_zero() {
            return;
        }
        if tokio::time::timeout(timeout, self.endpoint.online())
            .await
            .is_err()
        {
            warn!(
                "still not reachable after {timeout:?}; carrying on, but a peer may not be \
                 able to find this node yet"
            );
        }
    }

    /// Whether an invite is currently redeemable.
    pub async fn invite_is_open(&self) -> bool {
        match self.invite.lock().await.as_ref() {
            Some(pending) => tokio::time::Instant::now() < pending.expires_at,
            None => false,
        }
    }

    async fn handle_pairing(self: Arc<Self>, connection: Connection) -> Result<()> {
        let remote = connection.remote_id();
        let (mut send, mut recv) = connection.accept_bi().await?;
        let request: JoinRequest = read_json(&mut recv)
            .await?
            .context("joiner opened a pairing stream and sent nothing")?;

        let outcome = self
            .redeem_invite(&request.token, remote, &request.name)
            .await;

        let paired = match &outcome {
            Ok((my_name, peer_name, greeting)) => {
                write_json(
                    &mut send,
                    &JoinResponse::Ok {
                        name: my_name.clone(),
                    },
                )
                .await?;
                Some((peer_name.clone(), greeting.clone()))
            }
            Err(e) => {
                warn!(remote = %remote, error = %e, "refused a pairing attempt");
                write_json(
                    &mut send,
                    &JoinResponse::Error {
                        message: e.to_string(),
                    },
                )
                .await?;
                None
            }
        };
        let _ = send.finish();

        // Wait for the joiner to hang up before letting this connection drop. Dropping a
        // QUIC connection discards anything still in flight, which would lose the reply
        // we just wrote.
        let _ = tokio::time::timeout(HANDSHAKE_LINGER, connection.closed()).await;

        if let Some((peer_name, greeting)) = paired {
            info!(peer = %peer_name, "paired");

            // Open the conversation so the joiner finds something already waiting
            // instead of an empty channel. The joiner authorized us before dialling,
            // so this is not a race.
            // The body is whatever the agent gave us and nothing else: the words in this
            // conversation belong to the agents, in whatever language they are speaking,
            // and are not ours to author.
            let opening = greeting.unwrap_or_default();
            if let Err(e) = self
                .send_kind(Some(&peer_name), Kind::Hello, &opening)
                .await
            {
                warn!(peer = %peer_name, error = %e, "could not announce myself");
            }
        }
        Ok(())
    }

    /// Validate a token and, if it is good, record the caller as a peer.
    ///
    /// Returns our own name, the name we filed the caller under, and the opening message.
    async fn redeem_invite(
        &self,
        token: &str,
        remote: EndpointId,
        preferred_name: &str,
    ) -> Result<(String, String, Option<String>)> {
        let pending = {
            let mut slot = self.invite.lock().await;
            let Some(pending) = slot.clone() else {
                bail!("no invite is open on this machine");
            };
            if tokio::time::Instant::now() >= pending.expires_at {
                *slot = None;
                bail!("the invite has expired");
            }
            if !tokens_match(&pending.token, token) {
                bail!("invite token does not match");
            }
            // Burn it: an invite is good for exactly one pairing.
            *slot = None;
            pending
        };

        let peer_name = {
            let mut peers = self.peers.write().await;
            let name = peers.add_paired(preferred_name, &remote)?;
            peers.save(&self.paths.peers())?;
            name
        };
        Ok((pending.my_name, peer_name, pending.greeting))
    }

    /// Redeem someone else's invite code.
    ///
    /// Returns the name we filed them under and the name we ended up going by, which is
    /// not always the one asked for.
    pub async fn join(
        &self,
        raw_code: &str,
        my_name: &str,
        introduction: Option<&str>,
    ) -> Result<(String, String)> {
        crate::config::validate_name(my_name)?;
        let code = InviteCode::decode(raw_code)?;

        // Both sides answering to the same name makes the transcript unreadable, and it
        // happens whenever two agents share a working directory. Take a different one
        // rather than asking which of us should.
        let my_name = distinct_from(my_name, &code.name);

        // A code minted by a newer build may assume arguments this one does not have, and
        // both sides follow the same written guide. Better a plain "upgrade" than a guide
        // describing flags the local binary will reject.
        if let Some(theirs) = &code.version {
            if is_newer(theirs, VERSION) {
                bail!(
                    "the other agent is on agent2agent {theirs} and this is {VERSION}; \
                     upgrade first (`brew upgrade agent2agent`) and then join again"
                );
            }
        }

        self.wait_until_reachable().await;
        let inviter = code.endpoint_id()?;
        if inviter == self.id() {
            bail!("that invite code was produced by this machine — it is for the other agent");
        }

        // Authorize the inviter *before* dialling: it sends an opening message the moment
        // pairing succeeds, and an unauthorized inbound connection would be refused.
        let (peer_name, was_already_known, addr) = {
            let mut peers = self.peers.write().await;
            let was_already_known = peers.name_for(&inviter).is_some();
            let name = peers.add_paired(&code.name, &inviter)?;
            peers.save(&self.paths.peers())?;
            let addr = peers
                .peers
                .get(&name)
                .expect("just inserted")
                .endpoint_addr()?;
            (name, was_already_known, addr)
        };

        match self.perform_join(addr, &code.token, &my_name).await {
            Ok(_) => {
                // Answer straight away: the inviter is already listening, and leaving it
                // to stare at an empty channel until this agent gets round to replying
                // makes a completed handshake look like a failed one. The arrival itself
                // is the signal; any words are the agent's own.
                if let Err(e) = self
                    .send_kind(
                        Some(&peer_name),
                        Kind::Hello,
                        introduction.unwrap_or_default(),
                    )
                    .await
                {
                    warn!(peer = %peer_name, error = %e, "could not announce myself");
                }
                Ok((peer_name, my_name))
            }
            Err(e) => {
                // Leave no half-authorized peer behind, unless it predates this attempt.
                if !was_already_known {
                    let mut peers = self.peers.write().await;
                    peers.remove(&peer_name);
                    let _ = peers.save(&self.paths.peers());
                }
                Err(e)
            }
        }
    }

    async fn perform_join(
        &self,
        inviter: EndpointAddr,
        token: &str,
        my_name: &str,
    ) -> Result<String> {
        // Retry rather than failing on the first attempt: an invite is usually redeemed
        // seconds after it was minted, and discovery may not have caught up with the
        // inviter yet. "No addressing information" then means "not yet", not "never".
        let deadline = tokio::time::Instant::now() + self.options.send_timeout;
        let connection = loop {
            match self.endpoint.connect(inviter.clone(), PAIR_ALPN).await {
                Ok(connection) => break connection,
                Err(e) if tokio::time::Instant::now() < deadline => {
                    debug!(error = %e, "cannot reach the inviting agent yet, retrying");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    return Err(e).context("cannot reach the inviting agent");
                }
            }
        };

        let (mut send, mut recv) = connection.open_bi().await?;
        write_json(
            &mut send,
            &JoinRequest {
                v: PROTOCOL_VERSION,
                token: token.to_string(),
                name: my_name.to_string(),
            },
        )
        .await?;
        send.finish()?;

        let response: JoinResponse = read_json(&mut recv)
            .await?
            .context("the inviting agent closed the stream without answering")?;
        match response {
            JoinResponse::Ok { name } => Ok(name),
            JoinResponse::Error { message } => {
                bail!("the inviting agent refused the invite code: {message}")
            }
        }
    }

    // --------------------------------------------------------------- outbound

    /// Deliver `body` to a peer. Returns the resolved peer name and the message id.
    pub async fn send(&self, peer: Option<&str>, body: &str) -> Result<(String, String)> {
        self.send_kind(peer, Kind::Msg, body).await
    }

    /// Deliver a message of a given kind.
    ///
    /// Ordinary messages to a peer that has said goodbye are refused: an agent should not
    /// be left talking to someone who told it they had gone. `hello` is how you reopen.
    pub async fn send_kind(
        &self,
        peer: Option<&str>,
        kind: Kind,
        body: &str,
    ) -> Result<(String, String)> {
        let (name, addr) = {
            let peers = self.peers.read().await;
            let (name, peer) = peers.resolve(peer)?;
            (name, peer.endpoint_addr()?)
        };

        if kind == Kind::Msg && self.departed.read().await.contains(&name) {
            bail!(
                "{name} has disconnected and is not reading replies; \
                 reopen the conversation with `agent2agent hello --to {name}` if you think they are back"
            );
        }

        // Saying hello reopens locally; saying goodbye closes locally. Either way the
        // local view updates whether or not the peer is currently reachable.
        match kind {
            Kind::Hello => {
                self.departed.write().await.remove(&name);
            }
            Kind::Bye => {
                self.departed.write().await.insert(name.clone());
                self.end_of_conversation().await;
            }
            Kind::Msg => {}
        }

        let message = WireMsg::of_kind(kind, body);
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
            Request::Send {
                peer,
                body,
                kind,
                confirmed,
            } => {
                // Resolve first, so both refusals below can name the peer.
                let name = match self.peers.read().await.resolve(peer.as_deref()) {
                    Ok((name, _)) => name,
                    Err(e) => return Response::error(format!("{e:#}")),
                };

                // Ordinary messages only: hello and bye are control signals, and a
                // departure that needed approval could never be delivered.
                if kind == Kind::Msg {
                    if self.departed.read().await.contains(&name) {
                        return Response::ok(ResponseData::PeerGone { peer: name });
                    }
                    if !confirmed && self.mode().await.is_manual() {
                        return Response::ok(ResponseData::NeedsApproval { peer: name });
                    }
                }

                match self.send_kind(Some(&name), kind, &body).await {
                    Ok((peer, id)) => Response::ok(ResponseData::Sent { peer, id }),
                    Err(e) => Response::error(format!("{e:#}")),
                }
            }

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

            Request::Invite {
                name,
                greeting,
                ttl_secs,
            } => {
                let ttl = Duration::from_secs(if ttl_secs == 0 {
                    DEFAULT_TTL_SECS
                } else {
                    ttl_secs
                });
                match self.create_invite(&name, greeting, ttl).await {
                    Ok(code) => Response::ok(ResponseData::Invite { code }),
                    Err(e) => Response::error(format!("{e:#}")),
                }
            }

            Request::Join {
                code,
                name,
                greeting,
            } => match self.join(&code, &name, greeting.as_deref()).await {
                Ok((peer, name)) => Response::ok(ResponseData::Joined { peer, name }),
                Err(e) => Response::error(format!("{e:#}")),
            },

            Request::Mode { set } => match self.set_or_report_mode(set.as_deref()).await {
                Ok(mode) => Response::ok(ResponseData::Mode {
                    mode: mode.to_string(),
                }),
                Err(e) => Response::error(format!("{e:#}")),
            },

            Request::Status => {
                let invite_open = self.invite_is_open().await;
                let mode = self.mode().await;
                let mut departed: Vec<String> =
                    self.departed.read().await.iter().cloned().collect();
                departed.sort();
                let peers = self.peers.read().await;
                Response::ok(ResponseData::Status(StatusInfo {
                    id: self.id().to_string(),
                    peers: peers
                        .peers
                        .iter()
                        .map(|(name, peer)| (name.clone(), peer.id.clone()))
                        .collect(),
                    default_peer: peers.default.clone(),
                    mode: mode.to_string(),
                    invite_open,
                    departed,
                    queued: self.inbox.counts(),
                    queued_total: self.inbox.len(),
                }))
            }

            Request::Shutdown => {
                info!("shutting down at the end of a conversation");
                self.shutdown.notify_waiters();
                Response::ok(ResponseData::Done)
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

    /// Whether a peer has told us it is gone.
    pub async fn has_departed(&self, peer: &str) -> bool {
        self.departed.read().await.contains(peer)
    }

    /// Report the mode, or change it for the rest of this conversation.
    pub async fn set_or_report_mode(&self, set: Option<&str>) -> Result<Mode> {
        let mut mode = self.mode.write().await;
        if let Some(raw) = set {
            *mode = raw.parse::<Mode>()?;
        }
        Ok(*mode)
    }

    /// The current mode.
    pub async fn mode(&self) -> Mode {
        *self.mode.read().await
    }

    /// Put the operator back in the loop.
    ///
    /// Called whenever a conversation ends, so a grant of `auto` cannot silently carry
    /// over into the next one.
    async fn end_of_conversation(&self) {
        let mut mode = self.mode.write().await;
        if *mode != Mode::Manual {
            info!("conversation ended, back to approving each message");
            *mode = Mode::Manual;
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
        .alpns(vec![ALPN.to_vec(), PAIR_ALPN.to_vec()])
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
        _ = daemon.shutdown.notified() => {
            // Give the reply to `shutdown` a moment to reach the caller.
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        }
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
