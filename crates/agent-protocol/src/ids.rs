//! Strongly-typed ID newtypes for the agent framework.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A fresh random identifier, as a bare string.
///
/// This is the framework's one draw of randomness. Every typed id below is
/// minted through it, and the handful of places that hold a bare `String` id —
/// `QuestionPack::pack_id`, `DecisionTrace::trace_id`, `ToolManifest::
/// manifest_id` — call it too rather than reaching for `Uuid::new_v4`
/// themselves, so "where do ids come from?" has one answer in the source
/// instead of five (CONTRACT.md §7 裁决 #19).
///
/// It is random because an id minted here must be *unique per call*: the event
/// store keys its dedup table on `EventId`, so a generator that could repeat an
/// id would silently drop the event carrying it. Ids that must instead be
/// *stable across a rebuild* are not drawn here at all — they are written out
/// of durable ids (`MessageUid::from_string(format!("msg/user/{turn_id}"))` and
/// its neighbours in `model`).
pub fn fresh_id() -> String {
    Uuid::new_v4().to_string()
}

// `Ord` is derived for every id so that id-keyed maps (`BTreeMap<PluginId, _>`,
// for one) iterate in a stable order. A `HashMap` would make a tool surface
// assembled from a registry come out in a different order each run, and an
// order that changes for no reason is a difference a reader has to rule out.
macro_rules! id_newtype {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(String);

        #[allow(clippy::new_without_default)]
        impl $name {
            /// Generate a new random ID.
            pub fn new() -> Self {
                Self(fresh_id())
            }

            /// Create from an existing string (for deserialization / testing).
            pub fn from_string(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            /// Borrow the inner string.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_newtype!(SessionId, "Unique identifier for a Session.");
id_newtype!(RunId, "Unique identifier for a Run.");
id_newtype!(StepId, "Unique identifier for a Step.");
id_newtype!(EventId, "Unique identifier for an Event.");
id_newtype!(TurnId, "Unique identifier for a Turn.");
id_newtype!(InboxItemId, "Unique identifier for an Inbox item.");
id_newtype!(ApprovalId, "Unique identifier for an approval request.");
id_newtype!(MessageUid, "Stable identifier for one model-visible message.");
id_newtype!(AttemptId, "Unique identifier for one model-invocation attempt.");
id_newtype!(SchemaVersion, "Version tag for event schema evolution.");
id_newtype!(PluginId, "Stable identifier of a Plugin, from its manifest.");

impl SchemaVersion {
    /// The schema version this build stamps into every event envelope and
    /// reports from `doctor` (CONTRACT.md §6 C6.1).
    ///
    /// **One home.** A draft that hard-codes `"0.1.0"` again is a second answer
    /// to "which schema is this log?", and two answers can drift: a bump that
    /// touches most of them leaves one log carrying two versions. Banner the
    /// bump here and nowhere else. The value is deliberately *not* tied to
    /// `CARGO_PKG_VERSION` — compatibility tracks the wire shape, not the crate
    /// number (C6.2), so a code change that leaves the schema alone must not
    /// move this.
    ///
    /// Guarded by `crates/agent-testkit/tests/schema_version_boundaries.rs`,
    /// which fails if production source names a bare version literal again.
    pub const CURRENT: &'static str = "0.1.0";

    /// [`Self::CURRENT`] as a value, for the many draft builders that stamp the
    /// envelope.
    pub fn current() -> Self {
        Self::from_string(Self::CURRENT)
    }
}

/// Per-session monotonic event sequence number, assigned by the EventStore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSeq(pub u64);

impl EventSeq {
    /// Sequence number of the first event in a session.
    pub const ZERO: EventSeq = EventSeq(0);

    /// Create a sequence number from a raw value.
    pub fn new(seq: u64) -> Self {
        Self(seq)
    }

    /// The raw sequence value.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// The sequence number that follows this one.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for EventSeq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_schema_version_is_the_one_literal_that_exists() {
        // `current()` and `CURRENT` are the same value, and the value is what
        // the guard in `agent-testkit/tests/schema_version_boundaries.rs`
        // watches for: a bump that leaves the literal somewhere else would make
        // one log carry two versions (CONTRACT.md §6 C6.1).
        assert_eq!(SchemaVersion::current().as_str(), SchemaVersion::CURRENT);
        assert_eq!(SchemaVersion::current(), SchemaVersion::from_string("0.1.0"));
    }
}
