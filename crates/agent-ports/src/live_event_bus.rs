//! LiveEventBus trait — UI-only events that never enter the durable log.

use agent_protocol::events::LiveEvent;

/// A subscription to live events published from now on.
///
/// `None` from `next` means the bus is gone, the same as for stored events. What
/// a slow subscriber sees is the implementation's to state: a live event has no
/// durable position to be re-read from, so an implementation that refuses to
/// grow without bound has to drop something — and it has to say which end.
#[async_trait::async_trait]
pub trait LiveEventSubscription: Send {
    async fn next(&mut self) -> Option<LiveEvent>;
}

/// Port for publishing UI-only live events.
#[async_trait::async_trait]
pub trait LiveEventBus: Send + Sync {
    async fn publish(&self, event: LiveEvent) -> Result<(), LiveEventBusError>;

    /// Watch from this moment on.
    ///
    /// Nothing published before the call is replayed. A live event carries no
    /// durable position, so there is no "since" a late subscriber could be
    /// handed: replaying the recent past would be inventing history for a stream
    /// that deliberately has none.
    async fn subscribe(&self) -> Result<Box<dyn LiveEventSubscription>, LiveEventBusError>;
}

#[derive(Debug, thiserror::Error)]
pub enum LiveEventBusError {
    #[error("live event bus closed")]
    Closed,
}
