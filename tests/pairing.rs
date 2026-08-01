//! One-shot pairing: the handshake that lets the operator connect two agents with a
//! single copied string.

mod common;

use std::time::Duration;

use agent2agent::config::Peers;
use agent2agent::pairing::InviteCode;
use common::{offline_endpoint, start_node, TestNode, PATIENCE};
use iroh::SecretKey;

/// Two strangers: neither has ever heard of the other. This is the state the pairing
/// handshake is supposed to resolve, and the only thing that will cross between them is
/// the invite code.
async fn inviter_and_joiner() -> (TestNode, TestNode) {
    let inviter = start_node(
        offline_endpoint(SecretKey::generate()).await,
        Peers::default(),
    );
    let joiner = start_node(
        offline_endpoint(SecretKey::generate()).await,
        Peers::default(),
    );
    (inviter, joiner)
}

#[tokio::test]
async fn one_code_pairs_both_directions() {
    let (claude, codex) = inviter_and_joiner().await;

    let code = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();

    let peer = codex.daemon.join(&code, "codex", None).await.unwrap();
    assert_eq!(
        peer, "claude",
        "the joiner files the inviter under the code's name"
    );

    // Joining introduces itself first; step past that to the real message.
    let introduction = claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();
    assert_eq!(introduction.kind, agent2agent::wire::Kind::Hello);

    // Both sides now authorize each other, so messages flow without further setup.
    codex.daemon.send(Some("claude"), "joined").await.unwrap();
    let got = claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();
    assert_eq!(got.body, "joined");
    assert_eq!(
        got.peer, "codex",
        "the inviter learned the joiner's chosen name"
    );

    // The inviter announced itself on pairing too, so step past that as well.
    let announcement = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(announcement.kind, agent2agent::wire::Kind::Hello);

    claude.daemon.send(Some("codex"), "welcome").await.unwrap();
    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "welcome");
}

#[tokio::test]
async fn the_greeting_is_waiting_the_moment_pairing_completes() {
    let (claude, codex) = inviter_and_joiner().await;

    let code = claude
        .daemon
        .create_invite(
            "claude",
            Some("hey, what's up".to_string()),
            Duration::from_secs(60),
        )
        .await
        .unwrap();

    codex.daemon.join(&code, "codex", None).await.unwrap();

    let got = codex
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .expect("the opening message should arrive unprompted");
    assert_eq!(got.body, "hey, what's up");
    assert_eq!(got.peer, "claude");
}

#[tokio::test]
async fn a_code_works_only_once() {
    let (claude, codex) = inviter_and_joiner().await;
    let code = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();

    codex.daemon.join(&code, "codex", None).await.unwrap();

    // A third agent replaying the same code must be turned away.
    let intruder_endpoint = offline_endpoint(SecretKey::generate()).await;
    let intruder = start_node(intruder_endpoint, Peers::default());

    let err = intruder
        .daemon
        .join(&code, "intruder", None)
        .await
        .expect_err("a redeemed code must not pair a second agent");
    assert!(
        err.to_string().contains("no invite"),
        "unexpected error: {err:#}"
    );

    assert!(
        !claude.daemon.invite_is_open().await,
        "the invite is burned after one use"
    );
}

#[tokio::test]
async fn a_wrong_token_is_refused() {
    let (claude, codex) = inviter_and_joiner().await;
    let real = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();

    // Same inviter and id, token swapped for a plausible-looking one.
    let mut code = InviteCode::decode(&real).unwrap();
    code.token = "f".repeat(code.token.len());

    let err = codex
        .daemon
        .join(&code.encode(), "codex", None)
        .await
        .expect_err("knowing the id is not enough");
    assert!(
        err.to_string().contains("token"),
        "unexpected error: {err:#}"
    );

    assert!(
        claude.daemon.invite_is_open().await,
        "a failed attempt must not burn the real invite"
    );
}

#[tokio::test]
async fn a_failed_join_leaves_no_half_authorized_peer_behind() {
    let (claude, codex) = inviter_and_joiner().await;
    let real = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();

    let mut code = InviteCode::decode(&real).unwrap();
    code.token = "0".repeat(code.token.len());
    assert!(codex
        .daemon
        .join(&code.encode(), "codex", None)
        .await
        .is_err());

    // The inviter must not be left authorized on the joiner's side.
    let peers = Peers::load(&codex.paths.peers()).unwrap();
    assert!(
        peers.name_for(&claude.id()).is_none(),
        "a refused pairing must not leave the inviter on the peer list"
    );
}

#[tokio::test]
async fn an_expired_code_is_refused() {
    let (claude, codex) = inviter_and_joiner().await;
    let code = claude
        .daemon
        .create_invite("claude", None, Duration::from_millis(1))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let err = codex
        .daemon
        .join(&code, "codex", None)
        .await
        .expect_err("an expired code must not pair");
    assert!(
        err.to_string().contains("expired"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn a_new_invite_retires_the_previous_code() {
    let (claude, codex) = inviter_and_joiner().await;

    let first = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();
    let second = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();
    assert_ne!(first, second, "each invite draws a fresh token");

    assert!(
        codex.daemon.join(&first, "codex", None).await.is_err(),
        "the superseded code must stop working"
    );
    assert!(codex.daemon.join(&second, "codex", None).await.is_ok());
}

#[tokio::test]
async fn joining_without_an_open_invite_is_refused() {
    let (claude, codex) = inviter_and_joiner().await;

    // A well-formed code for a real endpoint that never opened an invite.
    let code = InviteCode {
        name: "claude".into(),
        id: InviteCode::encode_id(&claude.id()),
        token: InviteCode::new_token(),
        version: None,
    };

    let err = codex
        .daemon
        .join(&code.encode(), "codex", None)
        .await
        .expect_err("there is nothing to redeem");
    assert!(
        err.to_string().contains("no invite"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn a_malformed_code_fails_before_anything_is_dialled() {
    let (_claude, codex) = inviter_and_joiner().await;

    for bad in ["", "hello", "a2a1.only.three", "a2a9.claude.x.y"] {
        assert!(
            codex.daemon.join(bad, "codex", None).await.is_err(),
            "should reject {bad:?}"
        );
    }
}

#[tokio::test]
async fn an_agent_cannot_pair_with_itself() {
    let (claude, _codex) = inviter_and_joiner().await;
    let code = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();

    let err = claude
        .daemon
        .join(&code, "claude", None)
        .await
        .expect_err("pairing with yourself is a mistake worth naming");
    assert!(
        err.to_string().contains("this machine"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn pairing_survives_a_name_clash_on_the_inviter_side() {
    let inviter_endpoint = offline_endpoint(SecretKey::generate()).await;
    let joiner_endpoint = offline_endpoint(SecretKey::generate()).await;

    // The inviter already has an unrelated peer called "codex".
    let mut existing = Peers::default();
    existing
        .add("codex", &SecretKey::generate().public().to_string())
        .unwrap();

    let claude = start_node(inviter_endpoint, existing);
    let codex = start_node(joiner_endpoint, Peers::default());

    let code = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();
    codex.daemon.join(&code, "codex", None).await.unwrap();

    // The newcomer was filed under a free name rather than displacing the old entry.
    let peers = Peers::load(&claude.paths.peers()).unwrap();
    assert_eq!(peers.name_for(&codex.id()).as_deref(), Some("codex-2"));
    assert_eq!(peers.peers.len(), 2);
}

#[tokio::test]
async fn the_peer_list_is_persisted_so_pairing_survives_a_restart() {
    let (claude, codex) = inviter_and_joiner().await;
    let code = claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();
    codex.daemon.join(&code, "codex", None).await.unwrap();

    // Both sides wrote the pairing to disk, not just to memory.
    let on_disk = Peers::load(&claude.paths.peers()).unwrap();
    assert_eq!(on_disk.name_for(&codex.id()).as_deref(), Some("codex"));

    let on_disk = Peers::load(&codex.paths.peers()).unwrap();
    assert_eq!(on_disk.name_for(&claude.id()).as_deref(), Some("claude"));
}

#[tokio::test]
async fn pairing_does_not_open_a_hole_for_ordinary_messages() {
    // An agent that never paired must still be refused on the message ALPN, even though
    // the pairing ALPN accepts strangers by design.
    let claude_endpoint = offline_endpoint(SecretKey::generate()).await;
    let stranger_endpoint = offline_endpoint(SecretKey::generate()).await;

    let claude = start_node(claude_endpoint, Peers::default());
    let stranger = start_node(
        stranger_endpoint,
        common::peers_knowing("claude", claude.daemon.endpoint()),
    );

    // An invite being open does not make the message path permissive either.
    claude
        .daemon
        .create_invite("claude", None, Duration::from_secs(60))
        .await
        .unwrap();

    assert!(
        stranger.daemon.send(None, "let me in").await.is_err(),
        "an open invite must not authorize ordinary messages"
    );
    assert!(claude.daemon.inbox().is_empty());
}

#[tokio::test]
async fn joining_announces_the_arrival_without_inventing_words() {
    // The inviter is already listening when the code is redeemed, so the arrival has to
    // reach it immediately — but the words are the agent's, in whatever language the
    // conversation is in. The tool supplies none.
    let (claude, codex) = inviter_and_joiner().await;

    let code = claude
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();
    codex.daemon.join(&code, "mia", None).await.unwrap();

    let got = claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .expect("the arrival should reach the inviter unprompted");

    assert_eq!(got.peer, "mia", "who arrived is carried by the protocol");
    assert_eq!(got.kind, agent2agent::wire::Kind::Hello);
    assert_eq!(got.body, "", "no wording of ours goes into the channel");
}

#[tokio::test]
async fn a_joiner_may_bring_its_own_opening_words() {
    let (claude, codex) = inviter_and_joiner().await;

    let code = claude
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();
    codex
        .daemon
        .join(&code, "mia", Some("guten Tag, Mia hier"))
        .await
        .unwrap();

    let got = claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();
    assert_eq!(got.body, "guten Tag, Mia hier", "carried through untouched");
}

#[tokio::test]
async fn an_inviter_may_bring_its_own_opening_words() {
    let (claude, codex) = inviter_and_joiner().await;

    let code = claude
        .daemon
        .create_invite(
            "kip",
            Some("bonjour, ici Kip".into()),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    codex.daemon.join(&code, "mia", None).await.unwrap();

    let got = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(got.body, "bonjour, ici Kip");
    assert_eq!(got.kind, agent2agent::wire::Kind::Hello);
}

#[tokio::test]
async fn both_sides_end_up_knowing_each_other_by_name() {
    let (claude, codex) = inviter_and_joiner().await;

    let code = claude
        .daemon
        .create_invite(
            "Claude",
            Some("Hey, Claude here.".into()),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    codex.daemon.join(&code, "Codex", None).await.unwrap();

    // The joiner learns the inviter's name from the code...
    let greeting = codex.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
    assert_eq!(greeting.peer, "Claude");

    // ...and the inviter learns the joiner's from the authenticated connection.
    let introduction = claude
        .daemon
        .inbox()
        .pop_wait(None, PATIENCE)
        .await
        .unwrap();
    assert_eq!(introduction.peer, "Codex");
}
