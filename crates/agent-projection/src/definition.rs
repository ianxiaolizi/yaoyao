//! The projection fold contract.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::EventSeq;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why a projection refused an event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("unknown event type: {0}")]
    UnknownEventType(EventType),
    /// A known event whose payload this build cannot read.
    ///
    /// Unlike an unknown type, this event *is* the fold's business: the fold
    /// knows what the record means and cannot read it. Skipping it would drop a
    /// fact the log holds and shorten the derived view silently — the view still
    /// looks well-formed, only wrong. So it is an error, and the engine does not
    /// consult `ignorable` for it: that flag answers "may I skip a type I do not
    /// know", not "may I skip one I know and cannot parse".
    #[error("malformed payload for {event_type} at seq {seq}: {reason}")]
    MalformedPayload { event_type: EventType, seq: EventSeq, reason: String },
}

impl ApplyError {
    /// The error for a known event whose payload this build cannot read.
    pub(crate) fn malformed(event: &StoredEvent, reason: impl std::fmt::Display) -> Self {
        Self::MalformedPayload {
            event_type: event.event.event_type.clone(),
            seq: event.seq,
            reason: reason.to_string(),
        }
    }
}

/// A pure fold from a durable event stream into some state.
///
/// `apply` returns the next state, or an `ApplyError` when it must refuse the
/// event. The engine splits the two refusals apart: an unknown type is judged by
/// the event's `ignorable` flag (unknown ignorable events are skipped, unknown
/// required events fail closed), while a known type with an unreadable payload
/// always fails closed.
pub trait ProjectionDefinition: Send + Sync {
    type State: Clone + PartialEq + Send + Sync + Serialize + DeserializeOwned;
    type View: Clone + Send + Sync;

    /// The projection's key, used to name its checkpoint file.
    fn key(&self) -> &str;
    /// Schema tag for the projection state (surfaced in checkpoints).
    fn state_schema(&self) -> &str;
    /// Bump when an `apply` change makes old checkpoints incompatible.
    fn state_version(&self) -> u64;
    /// The empty state folded from zero events.
    fn init(&self) -> Self::State;
    /// Fold one stored event into the next state.
    fn apply(&self, state: &Self::State, event: &StoredEvent) -> Result<Self::State, ApplyError>;
    /// Derive the client-facing value from the folded state.
    fn view(&self, state: &Self::State) -> Self::View;
}
