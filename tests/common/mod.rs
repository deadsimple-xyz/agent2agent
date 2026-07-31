//! Helpers for spinning up daemons inside the test process.
//!
//! The endpoints built here are deliberately offline: no relays and no discovery, with
//! peers addressed by pinned loopback addresses. That keeps the suite hermetic — it
//! passes on a machine with no network — while still exercising the real QUIC path,
//! the real authentication, and the real framing.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use agent2agent::config::{Paths, Peer, Peers};
use agent2agent::daemon::{Daemon, Options};
use agent2agent::wire::ALPN;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointId, RelayMode, SecretKey};
use tempfile::TempDir;

/// How long tests wait for something that should happen promptly on loopback.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// Bind an endpoint on IPv4 loopback with no relay and no discovery.
pub async fn offline_endpoint(secret_key: SecretKey) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .secret_key(secret_key)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .expect("loopback is a valid bind address")
        .bind()
        .await
        .expect("binding an offline endpoint")
}

/// The endpoint's bound addresses, formatted for `peers.toml`.
pub fn pinned_addrs(endpoint: &Endpoint) -> Vec<String> {
    let addrs: Vec<String> = endpoint
        .bound_sockets()
        .into_iter()
        .map(|addr| addr.to_string())
        .collect();
    assert!(
        !addrs.is_empty(),
        "endpoint reported no bound sockets, tests cannot pin an address"
    );
    addrs
}

/// A peer entry pinned to an endpoint's current addresses.
pub fn peer_entry(endpoint: &Endpoint) -> Peer {
    Peer {
        id: endpoint.id().to_string(),
        addrs: pinned_addrs(endpoint),
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
