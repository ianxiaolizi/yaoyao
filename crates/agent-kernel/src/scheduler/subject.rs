//! The authorization subject one turn acts for.

use agent_protocol::events::StoredEvent;
use agent_protocol::ids::{IntentId, SessionId};
use agent_protocol::inbox::InboxItem;
use agent_protocol::intent::IntentStatus;
use agent_protocol::permission::AuthorizationSubject;

use crate::intent::{self, IntentError};

/// The intent and revision every tool call in this turn carries.
///
/// A direct request names its intent in the inbox item. A resumed continuation
/// carries no new intent, so it continues the active one. The intent's recorded
/// sequence is a stable revision across that resume and changes with a new
/// request. A turn with no intent, or an old log, uses the manual subject.
pub(super) fn for_turn(
    session_id: &SessionId,
    item: &InboxItem,
    resumed: bool,
    history: &[StoredEvent],
) -> Result<AuthorizationSubject, IntentError> {
    let direct = crate::inbox::item_intent_id(item);
    let continuation = if resumed && direct.is_none() {
        intent::fold_intents(history)?
            .into_iter()
            .rev()
            .find(|intent| intent.status == IntentStatus::Active)
            .map(|intent| intent.intent_id)
    } else {
        None
    };
    Ok(subject_for_intent(session_id, direct.or(continuation), history))
}

fn subject_for_intent(
    session_id: &SessionId,
    intent_id: Option<IntentId>,
    history: &[StoredEvent],
) -> AuthorizationSubject {
    match intent_id {
        Some(intent_id) => AuthorizationSubject::new(
            intent_id.as_str(),
            intent::recorded_revision(history, &intent_id).unwrap_or(0),
        ),
        None => AuthorizationSubject::manual(session_id.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft, EventType};
    use agent_protocol::ids::InboxItemId;
    use agent_protocol::ids::{EventId, EventSeq, SchemaVersion};
    use agent_protocol::inbox::{InboxItemStatus, InboxKind};
    use chrono::Utc;

    fn stored(seq: u64, payload: serde_json::Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(EventType::INTENT_RECORDED),
                schema_version: SchemaVersion::current(),
                session_id: SessionId::from_string("s"),
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
            },
        }
    }

    fn item_with_intent(intent_id: &str) -> InboxItem {
        InboxItem {
            item_id: InboxItemId::new(),
            session_id: SessionId::from_string("s"),
            kind: InboxKind::NextTurn,
            payload: serde_json::json!({ "text": "do work", "intent_id": intent_id }),
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        }
    }

    #[test]
    fn a_bound_turn_uses_the_intent_and_its_record_sequence() {
        let intent_id = IntentId::from_string("intent-1");
        let history = vec![stored(
            4,
            serde_json::json!({
                "intent_id": intent_id,
                "session_id": "s",
                "item_id": null,
                "kind": "task",
                "text": "do work",
                "summary": "do work",
                "status": "active",
                "created_at": Utc::now(),
                "superseded_by": null,
            }),
        )];
        let subject =
            for_turn(&SessionId::from_string("s"), &item_with_intent("intent-1"), false, &history)
                .unwrap();
        assert_eq!(subject.task_id, "intent-1");
        assert_eq!(subject.task_revision, 5);
    }

    #[test]
    fn a_turn_with_no_intent_uses_the_manual_subject() {
        let item = InboxItem {
            item_id: InboxItemId::new(),
            session_id: SessionId::from_string("s"),
            kind: InboxKind::NextTurn,
            payload: serde_json::json!({ "text": "do work" }),
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };
        let subject = for_turn(&SessionId::from_string("s"), &item, false, &[]).unwrap();
        assert_eq!(subject, AuthorizationSubject::manual("s"));
    }
}
