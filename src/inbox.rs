//! The daemon's in-memory queue of received messages.
//!
//! Deliberately not persisted. Delivery is online-only: if the receiving daemon is not
//! running, the sender's `send` fails loudly rather than queueing somewhere. That keeps
//! the failure mode obvious instead of silently swallowing messages, and it keeps the
//! daemon free of a durable store.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::wire::Kind;
use tokio::time::Instant;

/// A message as it sits in the inbox, tagged with the local name of the sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Local name of the peer that sent it, from `peers.toml`.
    pub peer: String,
    /// Sender-assigned message id.
    pub id: String,
    /// Sender-assigned Unix timestamp, in seconds.
    pub ts: i64,
    /// Whether this is conversation, an arrival, or a departure.
    #[serde(default)]
    pub kind: Kind,
    /// The message text. Untrusted — see [`crate::render`].
    pub body: String,
}

/// Default number of messages kept before the oldest are dropped.
pub const DEFAULT_CAPACITY: usize = 1000;

/// A bounded FIFO queue with async waiting.
#[derive(Debug)]
pub struct Inbox {
    queue: Mutex<VecDeque<Message>>,
    notify: Notify,
    capacity: usize,
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl Inbox {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "inbox capacity must be at least 1");
        Self {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            capacity,
        }
    }

    /// Append a message, evicting the oldest if we are at capacity.
    ///
    /// Returns the evicted message, if any, so the caller can log the loss.
    pub fn push(&self, message: Message) -> Option<Message> {
        let evicted = {
            let mut queue = self.lock();
            let evicted = if queue.len() >= self.capacity {
                queue.pop_front()
            } else {
                None
            };
            queue.push_back(message);
            evicted
        };
        self.notify.notify_waiters();
        evicted
    }

    /// Take the oldest message, optionally restricted to one peer. Never blocks.
    pub fn try_pop(&self, peer: Option<&str>) -> Option<Message> {
        let mut queue = self.lock();
        let index = match peer {
            None => 0,
            Some(name) => queue.iter().position(|m| m.peer == name)?,
        };
        queue.remove(index)
    }

    /// Take the oldest message, waiting up to `timeout` for one to arrive.
    ///
    /// A zero timeout degrades to [`Self::try_pop`].
    pub async fn pop_wait(&self, peer: Option<&str>, timeout: Duration) -> Option<Message> {
        let deadline = Instant::now() + timeout;
        loop {
            // Register interest *before* checking, otherwise a push landing between the
            // check and the registration would not wake us.
            let notified = self.notify.notified();

            if let Some(message) = self.try_pop(peer) {
                return Some(message);
            }
            if Instant::now() >= deadline {
                return None;
            }

            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => {
                    // One last look: a message may have arrived as the timer fired.
                    return self.try_pop(peer);
                }
            }
        }
    }

    /// Total queued messages.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Queued message count per peer, for `agent2agent status`.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        let queue = self.lock();
        let mut counts = BTreeMap::new();
        for message in queue.iter() {
            *counts.entry(message.peer.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Recover from a poisoned lock rather than cascading the panic: a corrupted count
    /// is not worth taking the daemon down for.
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Message>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn msg(peer: &str, body: &str) -> Message {
        Message {
            peer: peer.to_string(),
            id: format!("{peer}-{body}"),
            ts: 0,
            kind: Kind::Msg,
            body: body.to_string(),
        }
    }

    #[test]
    fn pops_in_fifo_order() {
        let inbox = Inbox::new(10);
        inbox.push(msg("codex", "one"));
        inbox.push(msg("codex", "two"));

        assert_eq!(inbox.try_pop(None).unwrap().body, "one");
        assert_eq!(inbox.try_pop(None).unwrap().body, "two");
        assert!(inbox.try_pop(None).is_none());
        assert!(inbox.is_empty());
    }

    #[test]
    fn filters_by_peer_without_disturbing_others() {
        let inbox = Inbox::new(10);
        inbox.push(msg("codex", "c1"));
        inbox.push(msg("gemini", "g1"));
        inbox.push(msg("codex", "c2"));

        assert_eq!(inbox.try_pop(Some("gemini")).unwrap().body, "g1");
        assert!(inbox.try_pop(Some("gemini")).is_none());

        // The codex messages are untouched and still in order.
        assert_eq!(inbox.try_pop(Some("codex")).unwrap().body, "c1");
        assert_eq!(inbox.try_pop(None).unwrap().body, "c2");
    }

    #[test]
    fn evicts_the_oldest_at_capacity() {
        let inbox = Inbox::new(2);
        assert!(inbox.push(msg("codex", "one")).is_none());
        assert!(inbox.push(msg("codex", "two")).is_none());

        let evicted = inbox.push(msg("codex", "three")).expect("should evict");
        assert_eq!(evicted.body, "one");
        assert_eq!(inbox.len(), 2);
        assert_eq!(inbox.try_pop(None).unwrap().body, "two");
        assert_eq!(inbox.try_pop(None).unwrap().body, "three");
    }

    #[test]
    fn counts_per_peer() {
        let inbox = Inbox::new(10);
        inbox.push(msg("codex", "a"));
        inbox.push(msg("codex", "b"));
        inbox.push(msg("gemini", "c"));

        let counts = inbox.counts();
        assert_eq!(counts.get("codex"), Some(&2));
        assert_eq!(counts.get("gemini"), Some(&1));
        assert_eq!(counts.get("nobody"), None);
    }

    #[test]
    #[should_panic(expected = "capacity")]
    fn zero_capacity_is_rejected() {
        Inbox::new(0);
    }

    #[tokio::test]
    async fn pop_wait_returns_immediately_when_a_message_is_ready() {
        let inbox = Inbox::new(10);
        inbox.push(msg("codex", "ready"));

        let got = inbox.pop_wait(None, Duration::from_secs(30)).await;
        assert_eq!(got.unwrap().body, "ready");
    }

    #[tokio::test(start_paused = true)]
    async fn pop_wait_times_out_on_an_empty_inbox() {
        let inbox = Inbox::new(10);
        let started = Instant::now();

        assert!(inbox.pop_wait(None, Duration::from_secs(5)).await.is_none());
        assert!(started.elapsed() >= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_timeout_does_not_block() {
        let inbox = Inbox::new(10);
        assert!(inbox.pop_wait(None, Duration::ZERO).await.is_none());

        inbox.push(msg("codex", "here"));
        assert_eq!(
            inbox.pop_wait(None, Duration::ZERO).await.unwrap().body,
            "here"
        );
    }

    #[tokio::test]
    async fn pop_wait_is_woken_by_a_later_push() {
        let inbox = Arc::new(Inbox::new(10));
        let waiter = {
            let inbox = inbox.clone();
            tokio::spawn(async move { inbox.pop_wait(None, Duration::from_secs(30)).await })
        };

        // Give the waiter a chance to park, then deliver.
        tokio::task::yield_now().await;
        inbox.push(msg("codex", "late"));

        let got = waiter.await.unwrap();
        assert_eq!(got.unwrap().body, "late");
    }

    #[tokio::test]
    async fn a_waiter_filtered_to_one_peer_ignores_traffic_from_another() {
        let inbox = Arc::new(Inbox::new(10));
        let waiter = {
            let inbox = inbox.clone();
            tokio::spawn(
                async move { inbox.pop_wait(Some("codex"), Duration::from_secs(30)).await },
            )
        };

        tokio::task::yield_now().await;
        inbox.push(msg("gemini", "not for you"));
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "must not wake for another peer");

        inbox.push(msg("codex", "for you"));
        let got = waiter.await.unwrap();
        assert_eq!(got.unwrap().body, "for you");

        // The gemini message is still queued.
        assert_eq!(inbox.try_pop(None).unwrap().body, "not for you");
    }

    #[tokio::test]
    async fn concurrent_waiters_each_get_a_distinct_message() {
        let inbox = Arc::new(Inbox::new(10));
        let mut waiters = Vec::new();
        for _ in 0..3 {
            let inbox = inbox.clone();
            waiters.push(tokio::spawn(async move {
                inbox.pop_wait(None, Duration::from_secs(30)).await
            }));
        }
        tokio::task::yield_now().await;

        for body in ["a", "b", "c"] {
            inbox.push(msg("codex", body));
        }

        let mut bodies = Vec::new();
        for waiter in waiters {
            bodies.push(waiter.await.unwrap().expect("each waiter gets one").body);
        }
        bodies.sort();
        assert_eq!(bodies, vec!["a", "b", "c"], "no message delivered twice");
        assert!(inbox.is_empty());
    }
}
