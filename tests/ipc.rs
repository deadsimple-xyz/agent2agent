//! The local CLI-to-daemon socket: the path every command actually takes.

mod common;

use std::time::Duration;

use agent2agent::config::Peers;
use agent2agent::daemon::{bind_ipc, Daemon};
use agent2agent::inbox::Message;
use agent2agent::ipc::{self, Request, Response, ResponseData};
use agent2agent::wire::Kind;
use common::{linked_pair, offline_endpoint, peers_knowing, start_node, TestNode, PATIENCE};
use iroh::SecretKey;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Start serving the CLI socket for a node and return its path.
async fn serve(node: &TestNode) -> std::path::PathBuf {
    let socket = node.paths.socket();
    let listener = bind_ipc(&node.paths).await.expect("binding the CLI socket");
    let daemon = node.daemon.clone();
    tokio::spawn(async move {
        let _ = daemon.serve_ipc(listener).await;
    });
    socket
}

async fn call(socket: &std::path::Path, request: Request) -> Response {
    ipc::request(socket, &request, Duration::from_secs(30))
        .await
        .expect("the daemon should reply")
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_identity_and_peers() {
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let socket = serve(&claude).await;

    let data = call(&socket, Request::Status).await.into_data().unwrap();
    let ResponseData::Status(status) = data else {
        panic!("expected a status reply, got {data:?}");
    };

    assert_eq!(status.id, claude.id().to_string());
    assert!(status.peers.contains_key("codex"));
    assert_eq!(status.default_peer.as_deref(), Some("codex"));
    assert_eq!(status.queued_total, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn send_and_receive_across_the_socket() {
    let (claude, codex) = linked_pair("claude", "codex").await;
    let claude_socket = serve(&claude).await;
    let codex_socket = serve(&codex).await;

    // Send the way `agent2agent send` does.
    let data = call(
        &claude_socket,
        Request::Send {
            peer: None,
            body: "over the socket".into(),
            kind: Kind::Msg,
            confirmed: true,
        },
    )
    .await
    .into_data()
    .unwrap();
    let ResponseData::Sent { peer, id } = data else {
        panic!("expected a sent reply, got {data:?}");
    };
    assert_eq!(peer, "codex");
    assert!(!id.is_empty());

    // Receive the way `agent2agent recv --wait` does.
    let data = call(
        &codex_socket,
        Request::Recv {
            peer: None,
            wait_ms: PATIENCE.as_millis() as u64,
        },
    )
    .await
    .into_data()
    .unwrap();
    let ResponseData::Message { message } = data else {
        panic!("expected a message, got {data:?}");
    };
    assert_eq!(message.body, "over the socket");
    assert_eq!(message.peer, "claude");
    assert_eq!(message.id, id, "the id reported to the sender matches");
}

#[tokio::test(flavor = "multi_thread")]
async fn recv_with_no_wait_returns_no_message_rather_than_blocking() {
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let socket = serve(&claude).await;

    let data = call(
        &socket,
        Request::Recv {
            peer: None,
            wait_ms: 0,
        },
    )
    .await
    .into_data()
    .unwrap();
    assert_eq!(data, ResponseData::NoMessage);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_poll_is_answered_by_a_message_that_arrives_later() {
    let (claude, codex) = linked_pair("claude", "codex").await;
    let codex_socket = serve(&codex).await;

    // Park a long poll first, the way an agent waiting for its turn would.
    let waiter = tokio::spawn({
        let socket = codex_socket.clone();
        async move {
            call(
                &socket,
                Request::Recv {
                    peer: None,
                    wait_ms: PATIENCE.as_millis() as u64,
                },
            )
            .await
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    claude.daemon.send(None, "woke you up").await.unwrap();

    let data = waiter.await.unwrap().into_data().unwrap();
    let ResponseData::Message { message } = data else {
        panic!("expected a message, got {data:?}");
    };
    assert_eq!(message.body, "woke you up");
}

#[tokio::test(flavor = "multi_thread")]
async fn recv_from_an_unknown_peer_is_refused_immediately() {
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let socket = serve(&claude).await;

    let started = std::time::Instant::now();
    let response = call(
        &socket,
        Request::Recv {
            peer: Some("nobody".into()),
            // A long wait: the point is that we do not sit through it.
            wait_ms: 30_000,
        },
    )
    .await;

    let Response::Error { message } = response else {
        panic!("expected an error, got {response:?}");
    };
    assert!(message.contains("nobody"), "unexpected error: {message}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "should fail fast, took {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sending_with_no_peers_configured_explains_what_to_do() {
    let endpoint = offline_endpoint(SecretKey::generate()).await;
    let lonely = start_node(endpoint, Peers::default());
    let socket = serve(&lonely).await;

    let response = call(
        &socket,
        Request::Send {
            peer: None,
            body: "into the void".into(),
            kind: Kind::Msg,
            confirmed: true,
        },
    )
    .await;

    let Response::Error { message } = response else {
        panic!("expected an error, got {response:?}");
    };
    assert!(
        message.contains("peer add"),
        "the error should point at the fix: {message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_over_the_socket_picks_up_a_new_peer() {
    let claude_endpoint = offline_endpoint(SecretKey::generate()).await;
    let codex_endpoint = offline_endpoint(SecretKey::generate()).await;

    let claude = start_node(claude_endpoint, Peers::default());
    let codex = start_node(
        codex_endpoint,
        peers_knowing("claude", claude.daemon.endpoint()),
    );
    let socket = serve(&claude).await;

    peers_knowing("codex", codex.daemon.endpoint())
        .save(&claude.paths.peers())
        .unwrap();

    let data = call(&socket, Request::Reload).await.into_data().unwrap();
    assert_eq!(data, ResponseData::Done);

    let data = call(
        &socket,
        Request::Send {
            peer: None,
            body: "after reload".into(),
            kind: Kind::Msg,
            confirmed: true,
        },
    )
    .await
    .into_data()
    .unwrap();
    assert!(matches!(data, ResponseData::Sent { .. }));

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "after reload");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_request_gets_an_error_not_a_dropped_connection() {
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let socket = serve(&claude).await;

    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(b"this is not json\n").await.unwrap();
    write_half.flush().await.unwrap();

    let mut line = String::new();
    BufReader::new(read_half)
        .read_line(&mut line)
        .await
        .unwrap();

    let response: Response = serde_json::from_str(line.trim_end()).unwrap();
    let Response::Error { message } = response else {
        panic!("expected an error, got {response:?}");
    };
    assert!(message.contains("malformed"), "unexpected error: {message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_keeps_serving_after_a_bad_request() {
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let socket = serve(&claude).await;

    let stream = UnixStream::connect(&socket).await.unwrap();
    let (_read, mut write) = stream.into_split();
    write.write_all(b"garbage\n").await.unwrap();
    drop(write);

    // A well-formed request still works.
    let data = call(&socket, Request::Status).await.into_data().unwrap();
    assert!(matches!(data, ResponseData::Status(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_hangs_up_without_speaking_is_harmless() {
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let socket = serve(&claude).await;

    UnixStream::connect(&socket).await.unwrap(); // dropped immediately
    tokio::time::sleep(Duration::from_millis(50)).await;

    let data = call(&socket, Request::Status).await.into_data().unwrap();
    assert!(matches!(data, ResponseData::Status(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_on_the_same_directory_is_refused() {
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let _socket = serve(&claude).await;

    let err = bind_ipc(&claude.paths)
        .await
        .expect_err("a second daemon must not take over the socket");
    assert!(
        err.to_string().contains("already running"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_socket_file_left_by_a_crash_is_cleared_away() {
    let dir = tempfile::TempDir::new().unwrap();
    let paths = agent2agent::config::Paths::from_dir(dir.path());
    paths.ensure_dir().unwrap();

    // Simulate the leftovers of a killed daemon: the file exists, nobody listens.
    std::fs::write(paths.socket(), b"").unwrap();

    let listener = bind_ipc(&paths)
        .await
        .expect("should reclaim a stale socket");
    drop(listener);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_overlong_state_directory_is_reported_before_bind_fails_obscurely() {
    let dir = tempfile::TempDir::new().unwrap();
    // Nest until the socket path passes the sun_path limit.
    let deep = dir.path().join("d".repeat(120));
    let paths = agent2agent::config::Paths::from_dir(&deep);

    let err = bind_ipc(&paths)
        .await
        .expect_err("an unbindable path must be diagnosed, not passed to the OS");
    assert!(
        err.to_string().contains("AGENT2AGENT_HOME"),
        "the error should say how to fix it: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_counts_queued_messages_per_peer() {
    let (claude, codex) = linked_pair("claude", "codex").await;
    let codex_socket = serve(&codex).await;

    claude.daemon.send(None, "one").await.unwrap();
    claude.daemon.send(None, "two").await.unwrap();

    // Wait for both to land before asking.
    for _ in 0..50 {
        if codex.daemon.inbox().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let data = call(&codex_socket, Request::Status)
        .await
        .into_data()
        .unwrap();
    let ResponseData::Status(status) = data else {
        panic!("expected a status reply, got {data:?}");
    };
    assert_eq!(status.queued_total, 2);
    assert_eq!(status.queued.get("claude"), Some(&2));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_clients_are_served_independently() {
    let (claude, codex) = linked_pair("claude", "codex").await;
    let codex_socket = serve(&codex).await;

    // Three parked readers, three messages, each reader gets exactly one.
    let mut readers = Vec::new();
    for _ in 0..3 {
        let socket = codex_socket.clone();
        readers.push(tokio::spawn(async move {
            call(
                &socket,
                Request::Recv {
                    peer: None,
                    wait_ms: PATIENCE.as_millis() as u64,
                },
            )
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    for body in ["a", "b", "c"] {
        claude.daemon.send(None, body).await.unwrap();
    }

    let mut bodies = Vec::new();
    for reader in readers {
        let data = reader.await.unwrap().into_data().unwrap();
        let ResponseData::Message { message } = data else {
            panic!("expected a message, got {data:?}");
        };
        bodies.push(message.body);
    }
    bodies.sort();
    assert_eq!(bodies, vec!["a", "b", "c"], "no message delivered twice");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_side_error_reaches_the_client_as_a_response() {
    // Not a transport failure: the daemon answers, and the answer is an error.
    let (claude, _codex) = linked_pair("claude", "codex").await;
    let socket = serve(&claude).await;

    let response = call(
        &socket,
        Request::Send {
            peer: Some("ghost".into()),
            body: "hello".into(),
            kind: Kind::Msg,
            confirmed: true,
        },
    )
    .await;
    assert!(matches!(response, Response::Error { .. }));

    // The daemon is still healthy afterwards.
    let data = call(&socket, Request::Status).await.into_data().unwrap();
    assert!(matches!(data, ResponseData::Status(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn message_bodies_with_newlines_survive_the_line_framing() {
    let (claude, codex) = linked_pair("claude", "codex").await;
    let codex_socket = serve(&codex).await;

    let body = "first line\nsecond line\n\nfourth";
    claude.daemon.send(None, body).await.unwrap();

    let data = call(
        &codex_socket,
        Request::Recv {
            peer: None,
            wait_ms: PATIENCE.as_millis() as u64,
        },
    )
    .await
    .into_data()
    .unwrap();

    let ResponseData::Message { message } = data else {
        panic!("expected a message, got {data:?}");
    };
    assert_eq!(message.body, body);
}

/// A compile-time reminder that `Daemon` stays shareable across tasks; the IPC server
/// and the accept loop both hold one.
#[allow(dead_code)]
fn daemon_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<std::sync::Arc<Daemon>>();
    assert_send_sync::<Message>();
}
