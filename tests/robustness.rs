//! What happens when the input is wrong, late, repeated, or racing.
//!
//! Pairing is the part a user drives by copying a string between two chats, which means
//! it meets every kind of mangling a string can meet: truncated, stale, pasted twice,
//! pasted into three terminals at once. None of that may corrupt a conversation or leave
//! anything running.

mod common;

use std::time::Duration;

use agent2agent::config::Peers;
use agent2agent::pairing::InviteCode;
use agent2agent::wire::Kind;
use common::{offline_endpoint, start_node, TestNode, PATIENCE};
use iroh::SecretKey;

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

#[tokio::test(flavor = "multi_thread")]
async fn three_terminals_racing_one_code_produce_one_pairing() {
    // The code is copied by hand, so it can end up pasted in several places at once.
    // Exactly one redemption may win: the token is burned under a lock, not checked and
    // then burned.
    let inviter = start_node(
        offline_endpoint(SecretKey::generate()).await,
        Peers::default(),
    );
    let code = inviter
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();

    let mut joiners = Vec::new();
    for index in 0..3 {
        let node = start_node(
            offline_endpoint(SecretKey::generate()).await,
            Peers::default(),
        );
        let code = code.clone();
        joiners.push(tokio::spawn(async move {
            let outcome = node.daemon.join(&code, &format!("j{index}"), None).await;
            (node, outcome)
        }));
    }

    let mut winners = 0;
    let mut nodes = Vec::new();
    for joiner in joiners {
        let (node, outcome) = joiner.await.unwrap();
        if outcome.is_ok() {
            winners += 1;
        }
        nodes.push(node);
    }

    assert_eq!(winners, 1, "exactly one of three may redeem the code");
    assert!(!inviter.daemon.invite_is_open().await, "and it is burned");

    // The inviter ends up with one peer, not three.
    let peers = Peers::load(&inviter.paths.peers()).unwrap();
    assert_eq!(peers.peers.len(), 1, "one pairing, not three: {peers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_code_pasted_twice_in_a_row_is_refused_the_second_time() {
    let (inviter, joiner) = inviter_and_joiner().await;
    let code = inviter
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();

    assert!(joiner.daemon.join(&code, "mia", None).await.is_ok());

    let err = joiner
        .daemon
        .join(&code, "mia", None)
        .await
        .expect_err("a redeemed code is spent");
    assert!(
        err.to_string().contains("no invite"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn every_way_of_mangling_a_code_is_refused() {
    let (_inviter, joiner) = inviter_and_joiner().await;
    let good = InviteCode {
        name: "kip".into(),
        id: InviteCode::encode_id(&SecretKey::generate().public()),
        token: InviteCode::new_token(),
        version: None,
    }
    .encode();

    let mangled = [
        ("empty", String::new()),
        ("whitespace", "   ".to_string()),
        ("prose", "hello there".to_string()),
        ("a url", "https://example.com/a.b.c.d".to_string()),
        ("wrong prefix", good.replacen("a2a1", "a2a9", 1)),
        ("no prefix", good.trim_start_matches("a2a1.").to_string()),
        ("truncated", good[..good.len() / 2].to_string()),
        (
            "one part short",
            good.rsplit_once('.').unwrap().0.to_string(),
        ),
        ("extra parts", format!("{good}.x.y")),
        ("separators only", "....".to_string()),
        ("newline in the middle", good.replacen('.', ".\n", 1)),
        ("null byte", format!("{good}\0")),
    ];

    for (what, code) in mangled {
        assert!(
            joiner.daemon.join(&code, "mia", None).await.is_err(),
            "{what} should be refused: {code:?}"
        );
    }
}

#[tokio::test]
async fn a_code_naming_an_endpoint_that_never_existed_fails_without_hanging_forever() {
    let (_inviter, joiner) = inviter_and_joiner().await;

    // Well-formed, and for a key nobody is listening on.
    let code = InviteCode {
        name: "ghost".into(),
        id: InviteCode::encode_id(&SecretKey::generate().public()),
        token: InviteCode::new_token(),
        version: None,
    };

    let started = std::time::Instant::now();
    let err = joiner
        .daemon
        .join(&code.encode(), "mia", None)
        .await
        .expect_err("there is nobody there");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "gave up after {:?}, which is too long to sit in silence",
        started.elapsed()
    );
    assert!(
        err.to_string().contains("cannot reach"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
async fn an_expired_code_is_refused_even_though_it_is_well_formed() {
    let (inviter, joiner) = inviter_and_joiner().await;
    let code = inviter
        .daemon
        .create_invite("kip", None, Duration::from_millis(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let err = joiner.daemon.join(&code, "mia", None).await.unwrap_err();
    assert!(
        err.to_string().contains("expired"),
        "unexpected error: {err:#}"
    );
    assert!(
        Peers::load(&joiner.paths.peers()).unwrap().peers.is_empty(),
        "a refused pairing must not leave the inviter authorized"
    );
}

#[tokio::test]
async fn a_failed_pairing_leaves_neither_side_authorized() {
    let (inviter, joiner) = inviter_and_joiner().await;
    let real = inviter
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();

    let mut forged = InviteCode::decode(&real).unwrap();
    forged.token = InviteCode::new_token();
    assert!(joiner
        .daemon
        .join(&forged.encode(), "mia", None)
        .await
        .is_err());

    assert!(
        Peers::load(&joiner.paths.peers()).unwrap().peers.is_empty(),
        "the joiner keeps nobody"
    );
    assert!(
        Peers::load(&inviter.paths.peers())
            .unwrap()
            .peers
            .is_empty(),
        "and the inviter keeps nobody"
    );
    assert!(
        inviter.daemon.invite_is_open().await,
        "a forged attempt does not burn the real invite"
    );
}

#[tokio::test]
async fn a_second_invite_retires_the_first_even_mid_flight() {
    let (inviter, joiner) = inviter_and_joiner().await;
    let first = inviter
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();
    let second = inviter
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();

    assert!(joiner.daemon.join(&first, "mia", None).await.is_err());
    assert!(joiner.daemon.join(&second, "mia", None).await.is_ok());
}

#[tokio::test]
async fn a_name_that_would_not_survive_a_file_path_is_refused() {
    let (inviter, joiner) = inviter_and_joiner().await;
    let code = inviter
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();

    for name in ["", "../escape", "has space", "with/slash", &"x".repeat(200)] {
        assert!(
            joiner.daemon.join(&code, name, None).await.is_err(),
            "{name:?} should not be usable as a name"
        );
    }
    assert!(
        inviter.daemon.invite_is_open().await,
        "and none of those attempts spent the invite"
    );
}

#[tokio::test]
async fn an_invite_cannot_be_opened_under_a_name_that_is_not_one() {
    let (inviter, _joiner) = inviter_and_joiner().await;
    for name in ["", "two words", "slash/name"] {
        assert!(
            inviter
                .daemon
                .create_invite(name, None, Duration::from_secs(60))
                .await
                .is_err(),
            "{name:?} should not be usable as a name"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_survives_a_burst_of_messages() {
    let (left, right) = common::linked_pair("left", "right").await;

    // All at once rather than in turn, which is where ordering and framing break if they
    // are going to.
    let mut sends = Vec::new();
    for index in 0..25 {
        let daemon = left.daemon.clone();
        sends.push(tokio::spawn(async move {
            daemon.send(None, &format!("message {index}")).await
        }));
    }
    for send in sends {
        send.await.unwrap().expect("every message should land");
    }

    let mut seen = Vec::new();
    for _ in 0..25 {
        seen.push(
            right
                .daemon
                .inbox()
                .pop_wait(None, PATIENCE)
                .await
                .expect("all 25 arrive")
                .body,
        );
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 25, "none lost, none duplicated");
}

#[tokio::test]
async fn a_message_of_awkward_shapes_arrives_unaltered() {
    let (left, right) = common::linked_pair("left", "right").await;

    let awkward = [
        "",
        " ",
        "\n",
        "\n\n\n",
        "\t\ttabs",
        "trailing spaces   ",
        "\u{0000}nul",
        "grüße 🌍 こんにちは",
        "\"quoted\" 'and' `backticks`",
        "{\"looks\":\"like json\"}",
        ">>> pretending to be incoming",
        "--confirm --session deadbeef",
        &"long ".repeat(2000),
    ];

    for body in awkward {
        // An empty body is not a message anyone can send through the CLI, but the daemon
        // should still carry it rather than mangle it.
        left.daemon.send(None, body).await.unwrap();
        let got = right.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();
        assert_eq!(got.body, body, "altered in flight: {body:?}");
        assert_eq!(got.kind, Kind::Msg);
    }
}

#[tokio::test]
async fn joining_yourself_is_refused_rather_than_deadlocking() {
    let (inviter, _joiner) = inviter_and_joiner().await;
    let code = inviter
        .daemon
        .create_invite("kip", None, Duration::from_secs(60))
        .await
        .unwrap();

    let err = inviter
        .daemon
        .join(&code, "kip", None)
        .await
        .expect_err("dialling yourself is a mistake worth naming");
    assert!(
        err.to_string().contains("this machine"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_vanishes_mid_conversation_fails_the_next_send_not_the_process() {
    let left_endpoint = offline_endpoint(SecretKey::generate()).await;
    let right_endpoint = offline_endpoint(SecretKey::generate()).await;
    let left = start_node(
        left_endpoint,
        common::peers_knowing("right", &right_endpoint),
    );
    let right = start_node(
        right_endpoint,
        common::peers_knowing("left", left.daemon.endpoint()),
    );

    left.daemon.send(None, "still here?").await.unwrap();
    right.daemon.inbox().pop_wait(None, PATIENCE).await.unwrap();

    // The other side goes away without saying goodbye.
    right.daemon.endpoint().close().await;

    let err = left
        .daemon
        .send(None, "are you there?")
        .await
        .expect_err("a vanished peer cannot be delivered to");
    assert!(
        err.to_string().contains("right"),
        "the error should name the peer: {err:#}"
    );

    // And the sender is still usable afterwards.
    assert!(
        !left.daemon.has_departed("right").await,
        "vanishing is not a goodbye"
    );
}
