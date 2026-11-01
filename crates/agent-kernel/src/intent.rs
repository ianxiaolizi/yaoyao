//! The intent ledger, read back from the event log.
//!
//! The write path lives with the queue it feeds ([`crate::inbox::Inbox`]): an
//! intent and the `inbox/appended` item it asks for are written in one append,
//! so neither can exist without the other. This module is the read side — the
//! fold that answers "what did the user ask for, and does it still stand?" from
//! the log alone, with no process memory and no second store.
//!
//! Two rules the fold keeps:
//!
//! * **Later events decide status.** The `status` field on the recorded intent
//!   is the value at write time; a fold that returned it unchanged would report
//!   a superseded intent as active for the rest of the session.
//! * **A log without intents is not an error.** Sessions written before this
//!   slice have no `intent/*` events, so every read answers "none" rather than
//!   failing. That is what keeps old logs readable (the compatibility rule in
//!   CONTRACT §6).

use std::collections::HashMap;
use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, IntentId, SessionId};
use agent_protocol::intent::{Intent, IntentStatus};
use serde::Deserialize;

/// Errors from reading the intent ledger.
///
/// A known intent event whose payload cannot be read is an error, not a
/// skipped record: skipping would hand back a ledger shorter than the log while
/// still looking well-formed (CONTRACT §2 C2.5).
#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("intent payload error: {0}")]
    Payload(#[from] serde_json::Error),
}

/// The payload `intent/superseded` carries.
#[derive(Debug, Deserialize)]
struct SupersedePayload {
    intent_id: IntentId,
    #[serde(default)]
    superseded_by: Option<IntentId>,
}

/// Reads intents out of one session's event log.
#[derive(Clone)]
pub struct IntentLedger {
    store: Arc<dyn EventStore>,
}

impl IntentLedger {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self { store }
    }

    /// Every intent the session recorded, in append order.
    pub async fn all(&self, session_id: &SessionId) -> Result<Vec<Intent>, IntentError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        fold_intents(&events)
    }

    /// The most recently recorded intent, whatever its status.
    pub async fn latest(&self, session_id: &SessionId) -> Result<Option<Intent>, IntentError> {
        Ok(self.all(session_id).await?.pop())
    }

    /// The most recently recorded intent that still stands.
    pub async fn active(&self, session_id: &SessionId) -> Result<Option<Intent>, IntentError> {
        Ok(self.all(session_id).await?.into_iter().rev().find(|i| i.status == IntentStatus::Active))
    }
}

/// Fold `intent/*` events into their latest intent states, in append order.
fn fold_intents(events: &[StoredEvent]) -> Result<Vec<Intent>, IntentError> {
    let mut by_id: HashMap<String, Intent> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for event in events {
        match event.event.event_type.as_str() {
            EventType::INTENT_RECORDED => {
                let intent: Intent = serde_json::from_value(event.event.payload.clone())?;
                if !by_id.contains_key(&intent.intent_id.to_string()) {
                    order.push(intent.intent_id.to_string());
                }
                by_id.insert(intent.intent_id.to_string(), intent);
            }
            EventType::INTENT_SUPERSEDED => {
                let payload: SupersedePayload =
                    serde_json::from_value(event.event.payload.clone())?;
                // An intent this session never recorded cannot be marked: the
                // log says something overtook it, but there is nothing here to
                // label. Leaving it out of the fold is not the same as denying
                // it happened — the event is still there.
                if let Some(intent) = by_id.get_mut(&payload.intent_id.to_string()) {
                    intent.status = IntentStatus::Superseded;
                    intent.superseded_by = payload.superseded_by;
                }
            }
            _ => {}
        }
    }

    Ok(order.into_iter().filter_map(|key| by_id.remove(&key)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventId, InboxItemId, SchemaVersion};
    use agent_testkit::FakeEventStore;
    use chrono::{TimeZone, Utc};

    fn draft(session_id: &SessionId, event_type: &str, payload: serde_json::Value) -> EventDraft {
        EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::current(),
            session_id: session_id.clone(),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::User,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: serde_json::json!({}),
            ignorable: false,
        }
    }

    fn an_intent(session_id: &SessionId, text: &str) -> Intent {
        Intent::user_direct(
            session_id.clone(),
            text,
            InboxItemId::new(),
            Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(),
        )
    }

    #[tokio::test]
    async fn recorded_intents_come_back_in_append_order() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("s");
        let first = an_intent(&sid, "first");
        let second = an_intent(&sid, "second");
        store
            .append(
                &sid,
                EventSeq::ZERO,
                vec![
                    draft(&sid, EventType::INTENT_RECORDED, serde_json::to_value(&first).unwrap()),
                    draft(&sid, EventType::INTENT_RECORDED, serde_json::to_value(&second).unwrap()),
                ],
            )
            .await
            .unwrap();

        let ledger = IntentLedger::new(store);
        let all = ledger.all(&sid).await.unwrap();
        assert_eq!(all, vec![first.clone(), second.clone()]);
        assert_eq!(ledger.latest(&sid).await.unwrap(), Some(second));
    }

    #[tokio::test]
    async fn a_supersede_event_changes_the_latest_answer_not_the_record() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("s");
        let old = an_intent(&sid, "old");
        let new = an_intent(&sid, "new");
        store
            .append(
                &sid,
                EventSeq::ZERO,
                vec![
                    draft(&sid, EventType::INTENT_RECORDED, serde_json::to_value(&old).unwrap()),
                    draft(&sid, EventType::INTENT_RECORDED, serde_json::to_value(&new).unwrap()),
                    draft(
                        &sid,
                        EventType::INTENT_SUPERSEDED,
                        serde_json::json!({
                            "intent_id": old.intent_id,
                            "superseded_by": new.intent_id,
                        }),
                    ),
                ],
            )
            .await
            .unwrap();

        let ledger = IntentLedger::new(store);
        let all = ledger.all(&sid).await.unwrap();
        assert_eq!(all[0].status, IntentStatus::Superseded);
        assert_eq!(all[0].superseded_by.as_ref(), Some(&new.intent_id));
        assert_eq!(all[0].text, "old", "the words stay readable");
        assert_eq!(all[1].status, IntentStatus::Active);
        assert_eq!(ledger.active(&sid).await.unwrap().map(|i| i.intent_id), Some(new.intent_id));
    }

    #[tokio::test]
    async fn a_log_without_intents_answers_none() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("s");
        store
            .append(
                &sid,
                EventSeq::ZERO,
                vec![draft(&sid, EventType::INBOX_APPENDED, serde_json::json!({}))],
            )
            .await
            .unwrap();

        let ledger = IntentLedger::new(store);
        assert!(ledger.all(&sid).await.unwrap().is_empty());
        assert_eq!(ledger.latest(&sid).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_supersede_without_an_intent_id_is_an_error_not_a_skip() {
        let store = Arc::new(FakeEventStore::new());
        let sid = SessionId::from_string("s");
        store
            .append(
                &sid,
                EventSeq::ZERO,
                vec![draft(&sid, EventType::INTENT_SUPERSEDED, serde_json::json!({}))],
            )
            .await
            .unwrap();

        let ledger = IntentLedger::new(store);
        assert!(matches!(ledger.all(&sid).await, Err(IntentError::Payload(_))));
    }
}
