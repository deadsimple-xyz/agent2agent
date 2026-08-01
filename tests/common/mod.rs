//! Helpers for spinning up daemons inside the test process.
//!
//! The endpoints built here are deliberately offline: no relays, and no discovery beyond
//! a [`MemoryLookup`] shared by every endpoint the suite creates. Each endpoint publishes
//! its loopback address into that lookup as it binds, so any test node can dial any other
//! by endpoint id alone — the same shape as production, where n0's discovery resolves the
//! id instead. Nothing here touches the network, so the suite passes on an offline
//! machine while still exercising real QUIC, real authentication and real framing.

#![allow(dead_code)]

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent2agent::config::{Paths, Peer, Peers};
use agent2agent::daemon::{Daemon, Options};
use agent2agent::pairing::PAIR_ALPN;
use agent2agent::wire::ALPN;
use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr};
use tempfile::TempDir;

/// How long tests wait for something that should happen promptly on loopback.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// The address book every test endpoint publishes into and resolves from.
fn directory() -> &'static MemoryLookup {
    static DIRECTORY: OnceLock<MemoryLookup> = OnceLock::new();
    DIRECTORY.get_or_init(MemoryLookup::new)
}

/// Bind an endpoint on IPv4 loopback with no relay, reachable by id through the shared
/// in-process directory.
pub async fn offline_endpoint(secret_key: SecretKey) -> Endpoint {
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(secret_key)
        .alpns(vec![ALPN.to_vec(), PAIR_ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .address_lookup(directory().clone())
        .bind_addr("127.0.0.1:0")
        .expect("loopback is a valid bind address")
        .bind()
        .await
        .expect("binding an offline endpoint");

    // Publish ourselves, so peers can resolve this id without being told an address.
    let addrs = endpoint.bound_sockets().into_iter().map(TransportAddr::Ip);
    let addr = EndpointAddr::from_parts(endpoint.id(), addrs);
    assert!(
        !addr.is_empty(),
        "endpoint reported no bound sockets, peers could never reach it"
    );
    directory().add_endpoint_info(addr);

    endpoint
}

/// The endpoint's bound addresses, formatted for `peers.toml`.
pub fn pinned_addrs(endpoint: &Endpoint) -> Vec<String> {
    endpoint
        .bound_sockets()
        .into_iter()
        .map(|addr| addr.to_string())
        .collect()
}

/// A peer entry addressed by id alone, the way `peer add` writes one.
pub fn peer_entry(endpoint: &Endpoint) -> Peer {
    Peer {
        id: endpoint.id().to_string(),
        addrs: Vec::new(),
    }
}

/// A daemon plus the temporary directory backing it.
///
/// Keep the whole struct alive for the duration of a test: dropping it removes the
/// state directory.
pub struct TestNode {
    pub daemon: Arc<Daemon>,
    pub paths: Paths,
    _dir: TempDir,
}

impl TestNode {
    pub fn id(&self) -> EndpointId {
        self.daemon.id()
    }
}

/// Build a daemon around `endpoint`, knowing the given peers, and start accepting.
pub fn start_node(endpoint: Endpoint, peers: Peers) -> TestNode {
    let dir = TempDir::new().expect("creating a temp state dir");
    let paths = Paths::from_dir(dir.path());
    paths.ensure_dir().expect("preparing the state dir");
    peers
        .save(&paths.peers())
        .expect("writing the initial peer list");

    // Everything here is loopback, so a delivery that has not landed in a few seconds
    // is not going to. The production default is 30s.
    let options = Options {
        send_timeout: Duration::from_secs(3),
        ..Options::default()
    };
    let daemon = Daemon::with_options(paths.clone(), endpoint, peers, options);
    daemon.spawn_accept_loop();

    TestNode {
        daemon,
        paths,
        _dir: dir,
    }
}

/// A peer list containing one entry pointing at `endpoint`, set as the default.
pub fn peers_knowing(name: &str, endpoint: &Endpoint) -> Peers {
    let mut peers = Peers::default();
    peers.peers.insert(name.to_string(), peer_entry(endpoint));
    peers.default = Some(name.to_string());
    peers
}

/// Two nodes that know each other by the given names.
pub async fn linked_pair(left_name: &str, right_name: &str) -> (TestNode, TestNode) {
    let left_endpoint = offline_endpoint(SecretKey::generate()).await;
    let right_endpoint = offline_endpoint(SecretKey::generate()).await;

    // Each side's peer list names the *other* endpoint.
    let left_peers = peers_knowing(right_name, &right_endpoint);
    let right_peers = peers_knowing(left_name, &left_endpoint);

    let left = start_node(left_endpoint, left_peers);
    let right = start_node(right_endpoint, right_peers);
    (left, right)
}
