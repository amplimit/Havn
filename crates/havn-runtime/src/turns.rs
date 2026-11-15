//! One turn at a time per conversation (issue #125). A message for a
//! conversation whose turn is still running waits in a queue and runs
//! after it; the user can stop the running turn, and the queue goes on.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use havn_core::InboundMessage;
use tokio_util::sync::CancellationToken;

/// Messages queued in one conversation, at most.
pub const MAX_QUEUED: usize = 20;

/// A stop that arrives this soon after a queued turn started was most
/// likely meant for the turn before it, whose reply crossed the stop on
/// the way; it is ignored.
const STOP_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct Turns {
    running: Mutex<HashMap<String, Running>>,
}

#[derive(Debug)]
struct Running {
    stop: CancellationToken,
    queue: VecDeque<InboundMessage>,
    /// When the running turn came off the queue, if it did.
    dequeued_at: Option<Instant>,
}

#[derive(Debug)]
pub enum Admitted {
    /// Run this turn now; cancelling the token stops it.
    Start(InboundMessage, CancellationToken),
    /// Queued behind the running turn, with `ahead` turns before it
    /// (the running one included).
    Queued {
        channel_id: String,
        channel: String,
        ahead: usize,
    },
    /// The queue is full; the message was dropped.
    Full { channel_id: String },
}

impl Turns {
    pub fn admit(&self, msg: InboundMessage) -> Admitted {
        let mut running = self.lock();
        match running.get_mut(&msg.channel_id) {
            Some(turn) if turn.queue.len() >= MAX_QUEUED => Admitted::Full {
                channel_id: msg.channel_id,
            },
            Some(turn) => {
                let (channel_id, channel) = (msg.channel_id.clone(), msg.channel.clone());
                turn.queue.push_back(msg);
                Admitted::Queued {
                    channel_id,
                    channel,
                    ahead: turn.queue.len(),
                }
            }
            None => {
                let stop = CancellationToken::new();
                running.insert(
                    msg.channel_id.clone(),
                    Running {
                        stop: stop.clone(),
                        queue: VecDeque::new(),
                        dequeued_at: None,
                    },
                );
                Admitted::Start(msg, stop)
            }
        }
    }

    /// The next queued message of `channel_id` and its stop token, or
    /// `None` when none is left and the conversation is idle again.
    pub fn next(&self, channel_id: &str) -> Option<(InboundMessage, CancellationToken)> {
        let mut running = self.lock();
        let turn = running.get_mut(channel_id)?;
        match turn.queue.pop_front() {
            Some(msg) => {
                turn.stop = CancellationToken::new();
                turn.dequeued_at = Some(Instant::now());
                Some((msg, turn.stop.clone()))
            }
            None => {
                running.remove(channel_id);
                None
            }
        }
    }

    /// Stop the running turn of `channel_id`; false when none runs or the
    /// stop came within [`STOP_GRACE`] of a queued turn starting.
    pub fn stop(&self, channel_id: &str) -> bool {
        self.stop_at(channel_id, Instant::now())
    }

    fn stop_at(&self, channel_id: &str, now: Instant) -> bool {
        let running = self.lock();
        match running.get(channel_id) {
            Some(turn)
                if turn
                    .dequeued_at
                    .is_some_and(|at| now.saturating_duration_since(at) < STOP_GRACE) =>
            {
                false
            }
            Some(turn) => {
                turn.stop.cancel();
                true
            }
            None => false,
        }
    }

    /// Take every queued message out, leaving running turns to finish; for
    /// a runtime that is about to stop.
    pub fn take_queued(&self) -> Vec<InboundMessage> {
        let mut running = self.lock();
        running
            .values_mut()
            .flat_map(|turn| turn.queue.drain(..))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Running>> {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use havn_core::MessageContent;

    fn msg(channel_id: &str, text: &str) -> InboundMessage {
        InboundMessage {
            channel_id: channel_id.into(),
            sender_id: "u".into(),
            channel: "telegram".into(),
            content: MessageContent::Text { text: text.into() },
            timestamp: chrono::Utc::now(),
            raw: serde_json::Value::Null,
        }
    }

    fn text(m: &InboundMessage) -> &str {
        match &m.content {
            MessageContent::Text { text } => text,
            _ => "",
        }
    }

    #[test]
    fn messages_queue_per_conversation_and_run_in_order() {
        let turns = Turns::default();
        let Admitted::Start(first, stop) = turns.admit(msg("a", "1")) else {
            panic!("first message starts");
        };
        assert_eq!(text(&first), "1");
        assert!(
            matches!(turns.admit(msg("b", "x")), Admitted::Start(..)),
            "other conversation"
        );
        assert!(matches!(
            turns.admit(msg("a", "2")),
            Admitted::Queued { ahead: 1, .. }
        ));
        assert!(matches!(
            turns.admit(msg("a", "3")),
            Admitted::Queued { ahead: 2, .. }
        ));

        assert!(turns.stop("a"));
        assert!(stop.is_cancelled());
        let (second, stop2) = turns.next("a").expect("queued message");
        assert_eq!(text(&second), "2");
        assert!(!stop2.is_cancelled(), "a new turn gets a fresh token");
        assert!(
            !turns.stop("a"),
            "a stop right after a queued turn starts is ignored"
        );
        assert!(!stop2.is_cancelled());
        assert!(turns.stop_at("a", Instant::now() + STOP_GRACE));
        assert!(stop2.is_cancelled());
        let (third, _) = turns.next("a").expect("queued message");
        assert_eq!(text(&third), "3");
        assert!(turns.next("a").is_none());
        assert!(!turns.stop("a"), "idle again");
        assert!(matches!(turns.admit(msg("a", "4")), Admitted::Start(..)));
    }

    #[test]
    fn the_queue_is_bounded() {
        let turns = Turns::default();
        let _ = turns.admit(msg("a", "running"));
        for i in 0..MAX_QUEUED {
            assert!(matches!(
                turns.admit(msg("a", &i.to_string())),
                Admitted::Queued { .. }
            ));
        }
        assert!(matches!(
            turns.admit(msg("a", "late")),
            Admitted::Full { .. }
        ));
        assert_eq!(turns.take_queued().len(), MAX_QUEUED);
        assert!(
            turns.next("a").is_none(),
            "the running turn ends with nothing queued"
        );
    }
}
