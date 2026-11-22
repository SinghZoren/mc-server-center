//! Topic-based event hub with per-topic sequence numbers and bounded replay history.
//!
//! Clients subscribe with an optional cursor (`from_seq`). Buffered events after the cursor are
//! replayed; anything older that was evicted is reported as a gap so the UI can say "earlier
//! output unavailable" instead of silently losing lines (REM-05).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;

const HISTORY: usize = 2000;
const CHANNEL: usize = 1024;

#[derive(Debug)]
struct Topic {
    seq: u64,
    history: VecDeque<(u64, Value)>,
    tx: broadcast::Sender<(u64, Value)>,
}

impl Topic {
    fn new() -> Self {
        Self {
            seq: 0,
            history: VecDeque::new(),
            tx: broadcast::channel(CHANNEL).0,
        }
    }
}

/// What a subscriber receives first.
#[derive(Debug)]
pub struct Subscription {
    /// Events missing between the cursor and the oldest buffered event, if any.
    pub gap: Option<(u64, u64)>,
    /// Buffered events after the cursor.
    pub backlog: Vec<(u64, Value)>,
    /// Live events.
    pub rx: broadcast::Receiver<(u64, Value)>,
}

/// The hub.
#[derive(Debug, Default)]
pub struct EventHub {
    topics: Mutex<HashMap<String, Topic>>,
}

impl EventHub {
    /// Publishes an event, returning its sequence number.
    pub fn publish(&self, topic: &str, data: Value) -> u64 {
        let Ok(mut topics) = self.topics.lock() else {
            return 0;
        };
        let t = topics.entry(topic.to_owned()).or_insert_with(Topic::new);
        t.seq += 1;
        let seq = t.seq;
        if t.history.len() == HISTORY {
            t.history.pop_front();
        }
        t.history.push_back((seq, data.clone()));
        let _ = t.tx.send((seq, data));
        seq
    }

    /// Subscribes, replaying events with `seq > from_seq` (`None` = only new events).
    pub fn subscribe(&self, topic: &str, from_seq: Option<u64>) -> Option<Subscription> {
        let mut topics = self.topics.lock().ok()?;
        let t = topics.entry(topic.to_owned()).or_insert_with(Topic::new);
        let rx = t.tx.subscribe();
        let Some(cursor) = from_seq else {
            return Some(Subscription {
                gap: None,
                backlog: Vec::new(),
                rx,
            });
        };
        let backlog: Vec<_> = t
            .history
            .iter()
            .filter(|(s, _)| *s > cursor)
            .cloned()
            .collect();
        let oldest = t.history.front().map_or(t.seq + 1, |(s, _)| *s);
        let gap = (cursor + 1 < oldest).then(|| (cursor + 1, oldest - 1));
        Some(Subscription { gap, backlog, rx })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn replay_and_gap() {
        let hub = EventHub::default();
        for i in 0..(HISTORY as u64 + 5) {
            hub.publish("t", json!(i));
        }
        let sub = hub.subscribe("t", Some(0)).unwrap();
        assert_eq!(sub.gap, Some((1, 5)));
        assert_eq!(sub.backlog.len(), HISTORY);
        let sub = hub.subscribe("t", Some(HISTORY as u64 + 3)).unwrap();
        assert_eq!(sub.gap, None);
        assert_eq!(sub.backlog.len(), 2);
        let sub = hub.subscribe("t", None).unwrap();
        assert!(sub.backlog.is_empty());
    }
}
