//! Inbox items: the durable pending-input queue entries.

use crate::ids::{InboxItemId, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of input an inbox item carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    /// A new turn request, processed after the current turn completes.
    NextTurn,
    /// Steering for the current turn, injected at the next safe boundary.
    NextStep,
    /// An interrupt request for the current run.
    Interrupt,
    /// A wakeup signal, e.g. an approval reply or subagent completion.
    Wakeup,
    /// A turn the user asked to start at a particular time.
    ///
    /// Not [`Wakeup`](Self::Wakeup): a wakeup says "the thing you were waiting
    /// on has an answer", and resume spends every wakeup without an approval id
    /// as already spent. A scheduled item is the opposite — it is waiting for a
    /// time, not for an event, and must survive a resume unchanged.
    ///
    /// The payload is `{ "due_at": <RFC3339>, "text": <string> }`. `due_at` is
    /// input the scheduler compares against a caller-supplied `now`; it is not
    /// a stamp, and nothing here reads a clock.
    Scheduled,
    /// A change to the running system, applied at the next safe boundary.
    ///
    /// Not [`NextStep`](Self::NextStep): steering is something said *to the
    /// turn*, while a control is something done *to the runtime* — the two are
    /// read by different sides and neither has a sentence to inject. The
    /// boundary is the same one, which is why they share a queue.
    Control,
    /// A kind this version has no name for.
    ///
    /// Kept rather than refused so that a newer writer's item does not make the
    /// whole item unreadable to an older reader: keep the event, let a
    /// projection skip what it cannot read. Nothing matches on it, so an
    /// unknown item waits for a version that understands it instead of being
    /// acted on by one that does not.
    ///
    /// The name is lost in the process: re-serializing an item read this way
    /// writes `"unknown"`. That is the price of the fallback, and it is the
    /// reason nothing in this crate re-writes an item it did not itself create.
    #[serde(other)]
    Unknown,
}

/// Lifecycle status of an inbox item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxItemStatus {
    Pending,
    Claimed,
    Discarded,
}

/// A durable pending-input entry, carried inside `inbox/appended` events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub item_id: InboxItemId,
    pub session_id: SessionId,
    pub kind: InboxKind,
    pub payload: serde_json::Value,
    /// Lower value = higher priority. Items process in priority-then-FIFO order.
    pub priority: u32,
    pub created_at: DateTime<Utc>,
    pub status: InboxItemStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbox_item_round_trips_through_serde() {
        let item = InboxItem {
            item_id: InboxItemId::new(),
            session_id: SessionId::new(),
            kind: InboxKind::NextTurn,
            payload: json!({"text": "hello"}),
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };
        let json = serde_json::to_string(&item).expect("serialize");
        let decoded: InboxItem = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(item.item_id, decoded.item_id);
        assert_eq!(item.kind, decoded.kind);
        assert_eq!(item.status, decoded.status);
    }

    #[test]
    fn inbox_kind_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&InboxKind::NextStep).unwrap(), "\"next_step\"");
        assert_eq!(serde_json::to_string(&InboxKind::NextTurn).unwrap(), "\"next_turn\"");
        assert_eq!(serde_json::to_string(&InboxKind::Control).unwrap(), "\"control\"");
        assert_eq!(serde_json::to_string(&InboxKind::Scheduled).unwrap(), "\"scheduled\"");
    }

    #[test]
    fn a_kind_this_version_does_not_know_is_data_rather_than_a_failure() {
        // What an older reader meets when a newer writer appends a kind it has
        // no name for. Refusing the whole item would make every field in it
        // unreadable — including the ones this version does understand — and
        // The rule asks for the opposite: keep the event, let a projection skip
        // what it cannot read. `EventType` is a transparent string and gets
        // this for free; this one has to say so.
        let item: InboxItem = serde_json::from_str(
            r#"{"item_id":"item-1","session_id":"s","kind":"some_future_kind",
                "payload":{},"priority":0,"created_at":"2026-10-10T00:00:00Z",
                "status":"pending"}"#,
        )
        .expect("an unknown kind is data, not an error");

        assert_eq!(item.kind, InboxKind::Unknown);
        assert_eq!(item.item_id.as_str(), "item-1", "the rest of the item is still readable");
    }

    #[test]
    fn a_control_item_carries_what_it_asks_for() {
        // The item is the request; nothing in it is an outcome. What a control
        // did to the session is recorded as what happened, not here.
        let item = InboxItem {
            item_id: InboxItemId::new(),
            session_id: SessionId::new(),
            kind: InboxKind::Control,
            payload: json!({"control": "plugin.disable", "plugin_id": "mcp.one"}),
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };

        let wire = serde_json::to_string(&item).expect("serialize");
        let decoded: InboxItem = serde_json::from_str(&wire).expect("deserialize");

        assert_eq!(decoded.kind, InboxKind::Control);
        assert_eq!(decoded.payload["control"], "plugin.disable");
        assert_eq!(decoded.payload["plugin_id"], "mcp.one");
    }

    #[test]
    fn inbox_status_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&InboxItemStatus::Pending).unwrap(), "\"pending\"");
    }
}
