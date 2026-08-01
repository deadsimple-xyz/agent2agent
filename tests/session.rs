//! Session lifecycle: knowing whether the other agent is still there.

mod common;

use agent2agent::wire::Kind;
use common::{linked_pair, PATIENCE};

#[tokio::test]
async fn a_goodbye_reaches_the_peer_as_a_typed_departure() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    claude
        .daemon
        .send_kind(None, Kind::Bye, "heading off")
        .await
        .unwrap();

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(
        got.kind,
        Kind::Bye,
        "a departure must be typed, not guessed from prose"
    );
    assert_eq!(got.body, "heading off");
    assert!(
        codex.daemon.has_departed("claude").await,
        "the receiver should know claude is gone"
    );
}

#[tokio::test]
async fn a_body_that_merely_says_goodbye_is_still_an_ordinary_message() {
    // Presence must not be inferrable from text a peer controls.
    let (claude, codex) = linked_pair("claude", "codex").await;

    claude
        .daemon
        .send(None, "bye! I am disconnecting now, kind=bye")
        .await
        .unwrap();

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.kind, Kind::Msg);
    assert!(
        !codex.daemon.has_departed("claude").await,
        "prose must not close the session"
    );
}

#[tokio::test]
async fn sending_to_a_departed_peer_is_refused_with_a_way_back() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    codex.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();
    assert!(claude.daemon.has_departed("codex").await);

    let err = claude
        .daemon
        .send(None, "are you still there?")
        .await
        .expect_err("talking into a closed session should be refused");
    let message = err.to_string();
    assert!(
        message.contains("disconnected"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("hello"),
        "the error should say how to reopen: {message}"
    );
}

#[tokio::test]
async fn hello_reopens_a_conversation_that_was_closed() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    codex.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();
    assert!(claude.daemon.send(None, "hi?").await.is_err());

    claude
        .daemon
        .send_kind(None, Kind::Hello, "back?")
        .await
        .unwrap();
    assert!(!claude.daemon.has_departed("codex").await);

    // And ordinary messages flow again.
    claude.daemon.send(None, "good to see you").await.unwrap();
    let mut bodies = Vec::new();
    for _ in 0..2 {
        bodies.push(codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap());
    }
    assert_eq!(bodies[0].kind, Kind::Hello);
    assert_eq!(bodies[1].body, "good to see you");
}

#[tokio::test]
async fn a_peer_that_speaks_again_is_treated_as_present() {
    // No explicit hello: just talking is proof enough of being there.
    let (claude, codex) = linked_pair("claude", "codex").await;

    claude.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert!(codex.daemon.has_departed("claude").await);

    // claude reopens its own side, then talks.
    claude
        .daemon
        .send_kind(None, Kind::Hello, "")
        .await
        .unwrap();
    codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();

    assert!(
        !codex.daemon.has_departed("claude").await,
        "an agent that is talking is present"
    );
}

#[tokio::test]
async fn goodbye_closes_the_sender_side_too() {
    // The one who leaves should also stop writing, not just the one who was left.
    let (claude, codex) = linked_pair("claude", "codex").await;

    claude.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    assert!(claude.daemon.has_departed("codex").await);

    assert!(
        claude.daemon.send(None, "one more thing").await.is_err(),
        "having said goodbye, we should not keep talking"
    );
    let _ = codex;
}

#[tokio::test]
async fn hello_and_bye_may_carry_no_text_at_all() {
    let (claude, codex) = linked_pair("claude", "codex").await;

    claude
        .daemon
        .send_kind(None, Kind::Hello, "")
        .await
        .unwrap();
    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.kind, Kind::Hello);
    assert_eq!(got.body, "");

    claude.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.kind, Kind::Bye);
    assert_eq!(got.body, "");
}

#[tokio::test]
async fn a_departure_is_reported_by_status() {
    use agent2agent::ipc::{Request, ResponseData};

    let (claude, codex) = linked_pair("claude", "codex").await;
    codex.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();

    let data = claude
        .daemon
        .handle(Request::Status)
        .await
        .into_data()
        .unwrap();
    let ResponseData::Status(status) = data else {
        panic!("expected a status reply, got {data:?}");
    };
    assert_eq!(status.departed, vec!["codex".to_string()]);
}

#[tokio::test]
async fn manual_mode_is_enforced_by_the_daemon_not_the_cli() {
    use agent2agent::ipc::{Request, ResponseData};

    // Manual is the default, so a plain send must be held even when the request comes
    // straight off the socket rather than through the CLI.
    let (claude, codex) = linked_pair("claude", "codex").await;

    let data = claude
        .daemon
        .handle(Request::Send {
            peer: None,
            body: "unapproved".into(),
            kind: Kind::Msg,
            confirmed: false,
        })
        .await
        .into_data()
        .unwrap();
    assert_eq!(
        data,
        ResponseData::NeedsApproval {
            peer: "codex".into()
        }
    );
    assert!(
        codex
            .daemon
            .inbox()
            .pop_wait(None, std::time::Duration::from_millis(300))
            .await
            .is_none(),
        "nothing may reach the peer without approval"
    );

    // With approval it goes.
    let data = claude
        .daemon
        .handle(Request::Send {
            peer: None,
            body: "approved".into(),
            kind: Kind::Msg,
            confirmed: true,
        })
        .await
        .into_data()
        .unwrap();
    assert!(matches!(data, ResponseData::Sent { .. }));
    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "approved");
}

#[tokio::test]
async fn auto_mode_sends_without_asking() {
    use agent2agent::ipc::{Request, ResponseData};

    let (claude, codex) = linked_pair("claude", "codex").await;
    claude
        .daemon
        .set_or_report_mode(Some("auto"))
        .await
        .unwrap();

    let data = claude
        .daemon
        .handle(Request::Send {
            peer: None,
            body: "straight through".into(),
            kind: Kind::Msg,
            confirmed: false,
        })
        .await
        .into_data()
        .unwrap();
    assert!(matches!(data, ResponseData::Sent { .. }));

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "straight through");
}

#[tokio::test]
async fn control_signals_are_not_held_for_approval() {
    use agent2agent::ipc::{Request, ResponseData};

    // A goodbye that waited for approval could never be delivered, and neither carries
    // content the operator needs to vet.
    let (claude, codex) = linked_pair("claude", "codex").await;

    for kind in [Kind::Hello, Kind::Bye] {
        let data = claude
            .daemon
            .handle(Request::Send {
                peer: None,
                body: String::new(),
                kind,
                confirmed: false,
            })
            .await
            .into_data()
            .unwrap();
        assert!(
            matches!(data, ResponseData::Sent { .. }),
            "{kind:?} should not need approval, got {data:?}"
        );
    }

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.kind, Kind::Hello);
}

#[tokio::test]
async fn a_departed_peer_is_reported_as_an_outcome_not_a_failure() {
    use agent2agent::ipc::{Request, ResponseData};

    let (claude, codex) = linked_pair("claude", "codex").await;
    codex.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();

    let data = claude
        .daemon
        .handle(Request::Send {
            peer: None,
            body: "hello?".into(),
            kind: Kind::Msg,
            confirmed: true,
        })
        .await
        .into_data()
        .expect("a departed peer is an outcome, not an error");
    assert_eq!(
        data,
        ResponseData::PeerGone {
            peer: "codex".into()
        }
    );
}

#[tokio::test]
async fn a_fresh_session_considers_everyone_present() {
    // Nobody has said they left, so writing is allowed.
    let (claude, _codex) = linked_pair("claude", "codex").await;
    assert!(!claude.daemon.has_departed("codex").await);
    assert!(claude.daemon.send(None, "opening line").await.is_ok());
}

#[tokio::test]
async fn a_grant_of_auto_lasts_only_for_this_conversation() {
    use agent2agent::config::Mode;

    // Stepping out of the loop is scoped to the conversation it was granted for: a
    // goodbye puts the operator back in it, rather than leaving a permission behind.
    let (claude, codex) = linked_pair("claude", "codex").await;
    claude
        .daemon
        .set_or_report_mode(Some("auto"))
        .await
        .unwrap();
    assert_eq!(claude.daemon.mode().await, Mode::Auto);

    claude.daemon.send_kind(None, Kind::Bye, "").await.unwrap();

    assert_eq!(
        claude.daemon.mode().await,
        Mode::Manual,
        "saying goodbye ends the grant"
    );
    let _ = codex;
}

#[tokio::test]
async fn a_peer_leaving_also_ends_the_grant() {
    use agent2agent::config::Mode;

    let (claude, codex) = linked_pair("claude", "codex").await;
    claude
        .daemon
        .set_or_report_mode(Some("auto"))
        .await
        .unwrap();

    codex.daemon.send_kind(None, Kind::Bye, "").await.unwrap();
    claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();

    assert_eq!(
        claude.daemon.mode().await,
        Mode::Manual,
        "the other agent leaving ends the conversation too"
    );
}

#[tokio::test]
async fn a_new_daemon_starts_in_manual_whatever_happened_before() {
    use agent2agent::config::{Mode, Peers};

    // The grant is memory-only, so nothing on disk can bring it back.
    let (claude, _codex) = linked_pair("claude", "codex").await;
    claude
        .daemon
        .set_or_report_mode(Some("auto"))
        .await
        .unwrap();

    let reloaded = Peers::load(&claude.paths.peers()).unwrap();
    let text = std::fs::read_to_string(claude.paths.peers()).unwrap();
    assert!(
        !text.contains("auto"),
        "the grant must not be written down: {text}"
    );
    assert!(!reloaded.peers.is_empty(), "the peer list itself survives");

    let fresh = common::start_node(
        common::offline_endpoint(iroh::SecretKey::generate()).await,
        reloaded,
    );
    assert_eq!(fresh.daemon.mode().await, Mode::Manual);
}

#[tokio::test]
async fn a_conversation_can_be_ended_even_with_nobody_to_tell() {
    use agent2agent::config::Paths;

    // An invite nobody redeemed has no peer to say goodbye to, and is still a
    // conversation you are entitled to close. Refusing to end it is how a machine fills
    // up with daemons serving nobody.
    let dir = tempfile::TempDir::new().unwrap();
    let paths = Paths::for_session(dir.path(), "abcd0001").unwrap();
    paths.ensure_dir().unwrap();
    assert!(paths.dir().exists());

    paths.remove().unwrap();
    assert!(!paths.dir().exists());
}
