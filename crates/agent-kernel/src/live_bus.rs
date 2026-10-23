//! An in-process `LiveEventBus` that records the most recent live events.
//!
//! Live events are UI-only: they never enter the durable log and never take
//! part in replay. This implementation keeps a bounded ring buffer so a
//! runtime without a UI can still be observed, and so tests can assert that a
//! delta stayed live, and hands out subscriptions for readers that want the
//! events as they happen rather than afterwards.
//!
//! Production reads events through `subscribe()`. The `events` /
//! `event_types` / `len` / `is_empty` accessors are the diagnostic surface:
//! they exist so a runtime or a test can look at the retained ring buffer
//! directly, and they are deliberately not part of the `LiveEventBus` port
//! (the trait stays the minimum a bus must provide).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_ports::live_event_bus::{LiveEventBus, LiveEventBusError, LiveEventSubscription};
use agent_protocol::events::LiveEvent;
use futures_channel::mpsc;
use futures_util::StreamExt;

/// Records live events in memory, keeping at most `capacity` of them, and fans
/// them out to subscribers.
pub struct InMemoryLiveEventBus {
    events: Mutex<Vec<LiveEvent>>,
    capacity: usize,
    subscribers: Mutex<Vec<Subscriber>>,
}

/// One subscriber's queue and its count of events it could not take.
struct Subscriber {
    tx: mpsc::Sender<LiveEvent>,
    dropped: Arc<AtomicU64>,
}

/// The receiving end of a subscription.
pub struct BusSubscription {
    rx: mpsc::Receiver<LiveEvent>,
    dropped: Arc<AtomicU64>,
}

impl BusSubscription {
    /// How many events this subscriber was too slow to take.
    ///
    /// Not on the port trait: what a user does about a gap is a rendering
    /// decision, and the trait stays the minimum a bus must provide.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl LiveEventSubscription for BusSubscription {
    async fn next(&mut self) -> Option<LiveEvent> {
        self.rx.next().await
    }
}

impl InMemoryLiveEventBus {
    /// A bus that retains the most recent `capacity` events.
    pub fn new(capacity: usize) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            capacity: capacity.max(1),
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// The retained events, oldest first.
    pub fn events(&self) -> Vec<LiveEvent> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }

    /// The retained event type names, oldest first.
    pub fn event_types(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.event_type.to_string()).collect()
    }

    /// How many events are retained right now.
    pub fn len(&self) -> usize {
        self.events.lock().map(|e| e.len()).unwrap_or(0)
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A subscription as its concrete type, which is the only way to ask how
    /// many events it had to refuse. The trait's `subscribe` boxes the same
    /// thing; this is that one construction with a narrower type.
    pub fn subscription(&self) -> Result<BusSubscription, LiveEventBusError> {
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return Err(LiveEventBusError::Closed);
        };
        let (tx, rx) = mpsc::channel(self.capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        subscribers.push(Subscriber { tx, dropped: dropped.clone() });
        Ok(BusSubscription { rx, dropped })
    }
}

#[async_trait::async_trait]
impl LiveEventBus for InMemoryLiveEventBus {
    async fn publish(&self, event: LiveEvent) -> Result<(), LiveEventBusError> {
        let Ok(mut events) = self.events.lock() else {
            return Err(LiveEventBusError::Closed);
        };
        events.push(event.clone());
        let overflow = events.len().saturating_sub(self.capacity);
        if overflow > 0 {
            events.drain(0..overflow);
        }
        drop(events);

        // A live event has no durable position, so a subscriber that cannot take
        // one has lost it for good. Each queue is bounded: a subscriber slow
        // enough to fill it is the one that pays, and its count is kept so what
        // it lost is visible rather than silently missing. What it loses is the
        // newest, not the oldest — a full queue keeps what it holds and refuses
        // what it cannot take.
        let Ok(mut subscribers) = self.subscribers.lock() else {
            return Err(LiveEventBusError::Closed);
        };
        subscribers.retain_mut(|subscriber| match subscriber.tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(e) if e.is_full() => {
                subscriber.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        });
        Ok(())
    }

    async fn subscribe(&self) -> Result<Box<dyn LiveEventSubscription>, LiveEventBusError> {
        Ok(Box::new(self.subscription()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::EventType;
    use agent_protocol::ids::SessionId;
    use chrono::Utc;

    fn live(seq: u64, event_type: &str) -> LiveEvent {
        LiveEvent {
            live_seq: seq,
            event_type: EventType::new(event_type),
            session_id: SessionId::from_string("bus-session"),
            created_at: Utc::now(),
            payload: serde_json::json!({}),
            metadata: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn retains_events_in_order() {
        let bus = InMemoryLiveEventBus::new(8);
        assert!(bus.is_empty());

        bus.publish(live(1, EventType::MODEL_STREAM_STARTED)).await.unwrap();
        bus.publish(live(2, EventType::MODEL_STREAM_DELTA)).await.unwrap();

        assert_eq!(bus.len(), 2);
        assert_eq!(
            bus.event_types(),
            vec!["model/stream.started".to_string(), "model/stream.delta".to_string()]
        );
    }

    #[tokio::test]
    async fn drops_the_oldest_event_past_capacity() {
        let bus = InMemoryLiveEventBus::new(2);
        for seq in 1..=4 {
            bus.publish(live(seq, EventType::MODEL_STREAM_DELTA)).await.unwrap();
        }
        let seqs: Vec<u64> = bus.events().iter().map(|e| e.live_seq).collect();
        assert_eq!(seqs, vec![3, 4]);
    }

    #[tokio::test]
    async fn a_subscriber_gets_events_published_after_it_subscribed() {
        let bus = InMemoryLiveEventBus::new(8);
        // Published before the subscription: a live event has no position to
        // replay from, so a late subscriber is not handed what it missed.
        bus.publish(live(1, EventType::MODEL_STREAM_STARTED)).await.unwrap();

        let mut sub = bus.subscribe().await.unwrap();
        bus.publish(live(2, EventType::MODEL_STREAM_DELTA)).await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("the published event arrives")
            .expect("a live event");
        assert_eq!(event.live_seq, 2);
        assert_eq!(event.event_type.as_str(), EventType::MODEL_STREAM_DELTA);
    }

    #[tokio::test]
    async fn a_subscriber_that_stops_reading_loses_the_newest_and_says_how_many() {
        // A subscriber that never reads while the bus keeps publishing. The
        // assertions are about the accounting rather than about a number, since
        // a channel's bound counts senders as well as slots and a test that
        // pinned that arithmetic would pin the wrong thing.
        let bus = InMemoryLiveEventBus::new(2);
        let mut sub = bus.subscription().unwrap();
        for seq in 1..=10 {
            bus.publish(live(seq, EventType::MODEL_STREAM_DELTA)).await.unwrap();
        }

        let mut delivered = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), sub.next()).await
        {
            delivered.push(event.live_seq);
        }

        assert!(sub.dropped() > 0, "a queue this small cannot have taken ten events");
        assert_eq!(
            delivered.len() as u64 + sub.dropped(),
            10,
            "every published event was delivered or counted as lost"
        );
        assert_eq!(
            delivered,
            (1..=delivered.len() as u64).collect::<Vec<_>>(),
            "what was kept is the oldest, in order: the queue refuses the newest"
        );
    }

    #[tokio::test]
    async fn a_subscription_ends_when_the_bus_is_dropped() {
        let bus = InMemoryLiveEventBus::new(4);
        let mut sub = bus.subscribe().await.unwrap();
        drop(bus);
        assert!(sub.next().await.is_none());
    }
}
