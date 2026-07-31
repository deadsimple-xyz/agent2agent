//! End-to-end delivery between two daemons over a real QUIC connection.

mod common;

use std::time::Duration;

use agent2agent::config::{Peer, Peers};
use agent2agent::wire::MAX_FRAME;
use common::{linked_pair, offline_endpoint, peers_knowing, start_node, PATIENCE};
use iroh::SecretKey;

#[tokio::test]
async fn a_message_travels_from_one_daemon_to_the_other() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    let (peer, id) = claude
        .daemon
        .send(None, "how is the refactor going?")
        .await
        .expect("send should succeed");
    assert_eq!(peer, "codex", "resolved via the default peer");

    let received = codex
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .expect("message should arrive");

    assert_eq!(received.body, "how is the refactor going?");
    assert_eq!(
        received.peer, "claude",
        "tagged with the local name of the sender"
    );
    assert_eq!(received.id, id, "the sender's message id is preserved");
}

#[tokio::test]
async fn messages_flow_in_both_directions_over_one_pairing() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    claude.daemon.send(None, "ping").await.unwrap();
    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "ping");

    codex.daemon.send(None, "pong").await.unwrap();
    let got = claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();
    assert_eq!(got.body, "pong");
    assert_eq!(got.peer, "codex");
}

#[tokio::test]
async fn many_messages_arrive_in_order_on_a_reused_connection() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    for i in 0..20 {
        claude
            .daemon
            .send(None, &format!("message {i}"))
            .await
            .unwrap_or_else(|e| panic!("send {i} failed: {e:#}"));
    }

    for i in 0..20 {
        let got = codex
            .daemon
            .inbox()
            .pop_wait(None, PATIENCE)
            .await
            .unwrap_or_else(|| panic!("message {i} never arrived"));
        assert_eq!(got.body, format!("message {i}"), "delivery order");
    }
}

#[tokio::test]
async fn an_explicit_peer_name_selects_the_route() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    let (peer, _) = claude.daemon.send(Some("codex"), "explicit").await.unwrap();
    assert_eq!(peer, "codex");

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "explicit");
}

#[tokio::test]
async fn sending_to_an_unknown_peer_name_fails_locally() {
    let (claude, _codex) = linked_pair("claude", "codex").await;

    let err = claude
        .daemon
        .send(Some("nobody"), "hello?")
        .await
        .expect_err("unknown peers must not be dialled");
    assert!(
        err.to_string().contains("nobody"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn a_stranger_cannot_deliver_a_message() {
    // codex knows claude. A third endpoint knows codex, but codex does not know it.
    let claude_endpoint = offline_endpoint(SecretKey::generate()).await;
    let codex_endpoint = offline_endpoint(SecretKey::generate()).await;
    let stranger_endpoint = offline_endpoint(SecretKey::generate()).await;

    let codex = start_node(codex_endpoint, peers_knowing("claude", &claude_endpoint));
    let stranger = start_node(
        stranger_endpoint,
        peers_knowing("codex", codex.daemon.endpoint()),
    );

    let result = stranger.daemon.send(None, "let me in").await;
    assert!(
        result.is_err(),
        "codex must refuse an endpoint id that is not in its peer list"
    );

    // And nothing reached the inbox.
    assert!(
        codex
            .daemon
            .inbox()
            .pop_wait(None, Duration::from_millis(500))
            .await
            .is_none(),
        "an unauthorized message must never be queued"
    );
    let _ = claude_endpoint;
}

#[tokio::test]
async fn authorization_follows_the_endpoint_id_not_the_peer_name() {
    // codex has an entry called "claude", but it holds a different key than the sender.
    let real_claude = offline_endpoint(SecretKey::generate()).await;
    let impostor = offline_endpoint(SecretKey::generate()).await;
    let codex_endpoint = offline_endpoint(SecretKey::generate()).await;

    // Point the entry at the real claude's id, but at the impostor's address.
    let mut codex_peers = Peers::default();
    codex_peers.peers.insert(
        "claude".to_string(),
        Peer {
            id: real_claude.id().to_string(),
            addrs: common::pinned_addrs(&impostor),
        },
    );
    let codex = start_node(codex_endpoint, codex_peers);

    let impostor_node = start_node(impostor, peers_knowing("codex", codex.daemon.endpoint()));
    let result = impostor_node
        .daemon
        .send(None, "trust me, I am claude")
        .await;

    assert!(
        result.is_err(),
        "holding the address is not enough; the key must match"
    );
    assert!(codex.daemon.inbox().is_empty());
    let _ = real_claude;
}

#[tokio::test]
async fn delivery_fails_clearly_when_the_peer_is_not_listening() {
    let claude_endpoint = offline_endpoint(SecretKey::generate()).await;
    let codex_endpoint = offline_endpoint(SecretKey::generate()).await;

    // Note codex's address, then close it so nothing is listening there.
    let peers = peers_knowing("codex", &codex_endpoint);
    codex_endpoint.close().await;

    let claude = start_node(claude_endpoint, peers);
    let err = claude
        .daemon
        .send(None, "anyone home?")
        .await
        .expect_err("delivery to a dead peer must fail");

    // The point is that it surfaces as an error rather than being silently queued.
    assert!(
        err.to_string().contains("codex"),
        "the error should name the peer: {err:#}"
    );
}

#[tokio::test]
async fn a_large_message_within_the_frame_limit_survives() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    // Comfortably large, still under MAX_FRAME once JSON-encoded.
    let body = "x".repeat(MAX_FRAME / 2);
    claude.daemon.send(None, &body).await.unwrap();

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body.len(), body.len());
    assert_eq!(got.body, body);
}

#[tokio::test]
async fn unicode_and_control_characters_round_trip_unchanged() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    let body = "привет 🌍\ttabbed\nnewline\r\n\"quoted\" \\backslash\\ \u{0007}";
    claude.daemon.send(None, body).await.unwrap();

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, body);
}

#[tokio::test]
async fn reload_picks_up_a_peer_added_to_the_file_afterwards() {
    let claude_endpoint = offline_endpoint(SecretKey::generate()).await;
    let codex_endpoint = offline_endpoint(SecretKey::generate()).await;

    // claude starts knowing nobody.
    let claude = start_node(claude_endpoint, Peers::default());
    let codex = start_node(
        codex_endpoint,
        peers_knowing("claude", claude.daemon.endpoint()),
    );

    assert!(
        claude.daemon.send(None, "too early").await.is_err(),
        "no peers configured yet"
    );

    // Write the peer list the way `agent2agent peer add` would, then reload.
    let peers = peers_knowing("codex", codex.daemon.endpoint());
    peers.save(&claude.paths.peers()).unwrap();
    let count = claude.daemon.reload_peers().await.unwrap();
    assert_eq!(count, 1);

    claude.daemon.send(None, "now it works").await.unwrap();
    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "now it works");
}

#[tokio::test]
async fn a_reloaded_peer_list_also_governs_who_may_connect() {
    let claude_endpoint = offline_endpoint(SecretKey::generate()).await;
    let codex_endpoint = offline_endpoint(SecretKey::generate()).await;

    // codex starts knowing nobody, so claude is a stranger.
    let codex = start_node(codex_endpoint, Peers::default());
    let claude = start_node(
        claude_endpoint,
        peers_knowing("codex", codex.daemon.endpoint()),
    );

    assert!(
        claude.daemon.send(None, "before").await.is_err(),
        "codex has not authorized claude yet"
    );

    // Authorize, reload, retry.
    let peers = peers_knowing("claude", claude.daemon.endpoint());
    peers.save(&codex.paths.peers()).unwrap();
    codex.daemon.reload_peers().await.unwrap();

    claude.daemon.send(None, "after").await.unwrap();
    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "after");
}

#[tokio::test]
async fn inbox_filtering_separates_two_senders() {
    // One receiver, two senders, so `recv --from` has something to discriminate.
    let hub_endpoint = offline_endpoint(SecretKey::generate()).await;
    let claude_endpoint = offline_endpoint(SecretKey::generate()).await;
    let codex_endpoint = offline_endpoint(SecretKey::generate()).await;

    let mut hub_peers = Peers::default();
    hub_peers
        .peers
        .insert("claude".into(), common::peer_entry(&claude_endpoint));
    hub_peers
        .peers
        .insert("codex".into(), common::peer_entry(&codex_endpoint));
    let hub = start_node(hub_endpoint, hub_peers);

    let claude = start_node(claude_endpoint, peers_knowing("hub", hub.daemon.endpoint()));
    let codex = start_node(codex_endpoint, peers_knowing("hub", hub.daemon.endpoint()));

    claude.daemon.send(None, "from claude").await.unwrap();
    codex.daemon.send(None, "from codex").await.unwrap();

    let from_codex = hub
        .daemon
        .inbox()
        .pop_wait(Some("codex"), PATIENCE)
        .await
        .expect("codex message");
    assert_eq!(from_codex.body, "from codex");

    let from_claude = hub
        .daemon
        .inbox()
        .pop_wait(Some("claude"), PATIENCE)
        .await
        .expect("claude message");
    assert_eq!(from_claude.body, "from claude");
}
