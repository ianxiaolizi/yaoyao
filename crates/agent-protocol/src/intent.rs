//! Intent ledger protocol: a durable identity for each direct user instruction.
//!
//! Before this module a user's instruction existed only as an `InboxItem` — a
//! queue entry with no identity of its own. Two queued instructions were
//! indistinguishable from one repeated instruction, so the scheduler could
//! answer the older one while the newer had no way to record that it had been
//! overtaken. That is exactly the 2026-11-01 incident: a new safety-fix
//! instruction never ran and the previous turn's report was returned instead.
//!
//! An [`Intent`] is written beside the inbox item it asks for, in the same
//! append batch. It records *what* was asked, *by whom*, and *whether it still
//! stands*. This slice only records and reads: routing, adjudication, and
//! superseding are later slices, and they read this ledger rather than
//! inventing a second one.
//!
//! The record rides the event envelope's schema version (CONTRACT §6 C6.1);
//! it deliberately has no version field of its own, because a second answer to
//! "which version is this record?" is the drift that boundary exists to
//! prevent.

use crate::ids::{InboxItemId, IntentId, SessionId, TurnId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How many characters a normalized [`Intent::summary`] keeps.
pub const SUMMARY_CHARS: usize = 200;

/// What relation a recorded instruction has to the session's work.
///
/// The write path can assert only [`Self::NewTask`] until an adjudicator
/// exists; the other names are here so a later slice records a decision in the
/// same vocabulary instead of re-typing the instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    /// A fresh request, with no assumption about what was running before.
    NewTask,
    /// "Keep going" on the work already in flight.
    Continuation,
    /// A correction to an answer or artifact already produced.
    Correction,
    /// A request that explicitly replaces an earlier one.
    Supersede,
    /// A future turn at a named time, not a replacement for the current one.
    Schedule,
    /// A request to stop what is running.
    Interrupt,
    /// A kind this version has no name for.
    ///
    /// Kept rather than refused for the same reason `InboxKind::Unknown` is:
    /// a newer writer's record must not become unreadable to an older reader.
    #[serde(other)]
    Unknown,
}

/// Who produced the instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentSource {
    /// A person typed it.
    User,
    /// A scheduled job started it.
    Scheduler,
    /// A child agent reported it.
    Subagent,
    /// The runtime recovered it.
    System,
    #[serde(other)]
    Unknown,
}

/// Where an intent stands now.
///
/// `Active` is what the write path records; the rest are applied by events
/// (`intent/superseded` today, a consuming event in a later slice). The fold
/// derives the latest value rather than trusting whichever write happens to be
/// first in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    /// Recorded and not yet overtaken or consumed.
    Active,
    /// Overtaken by a later instruction; its text and evidence stay readable.
    Superseded,
    /// The turn that answered it was claimed.
    Consumed,
    /// Refused before it could be queued.
    Rejected,
    #[serde(other)]
    Unknown,
}

/// One direct instruction, as it was received.
///
/// `text` is the user's words verbatim; `summary` is the whitespace-collapsed,
/// bounded form for status lines and UI. Keeping both means a short status view
/// never has to re-derive a normalization rule that a reader might spell
/// differently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub intent_id: IntentId,
    pub session_id: SessionId,
    pub kind: IntentKind,
    pub source: IntentSource,
    /// The instruction as received.
    pub text: String,
    /// The bounded, whitespace-collapsed form shown beside the id.
    pub summary: String,
    /// Earlier intents this one explicitly replaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<IntentId>,
    /// The inbox item this intent asked for, when it asked for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<InboxItemId>,
    /// The turn this intent was addressed to, when the caller knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_turn_id: Option<TurnId>,
    pub created_at: DateTime<Utc>,
    /// The status recorded with the write; the fold recomputes it from later
    /// events, so this field is never the last word on a superseded intent.
    pub status: IntentStatus,
    /// The intent that overtook this one, once one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<IntentId>,
}

impl Intent {
    /// The intent a direct user message records.
    ///
    /// `item_id` is minted by the caller so the item and the intent can name
    /// each other in the same append. The kind is [`IntentKind::NewTask`]
    /// because no relation has been decided yet — deciding it is the
    /// adjudicator's job, and this slice does not have one. Recording that as
    /// the fact it is beats guessing a relation the write path cannot know.
    pub fn user_direct(
        session_id: SessionId,
        text: impl Into<String>,
        item_id: InboxItemId,
        created_at: DateTime<Utc>,
    ) -> Self {
        let text = text.into();
        Self {
            intent_id: IntentId::new(),
            session_id,
            kind: IntentKind::NewTask,
            source: IntentSource::User,
            summary: Self::summarize(&text),
            text,
            supersedes: Vec::new(),
            item_id: Some(item_id),
            target_turn_id: None,
            created_at,
            status: IntentStatus::Active,
            superseded_by: None,
        }
    }

    /// Collapse whitespace and keep at most [`SUMMARY_CHARS`] characters.
    ///
    /// Characters rather than bytes: a summary is for people, and cutting a
    /// multi-byte character in half would make an unreadable one.
    pub fn summarize(text: &str) -> String {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut summary: String = flat.chars().take(SUMMARY_CHARS).collect();
        if flat.chars().count() > SUMMARY_CHARS {
            summary.push('…');
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> Intent {
        Intent::user_direct(
            SessionId::from_string("s"),
            "fix the junction escape",
            InboxItemId::from_string("item-1"),
            Utc::now(),
        )
    }

    #[test]
    fn an_intent_round_trips_through_serde() {
        let intent = intent();
        let wire = serde_json::to_string(&intent).expect("serialize");
        let decoded: Intent = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(intent, decoded);
    }

    #[test]
    fn a_kind_or_status_from_a_newer_build_is_data_not_failure() {
        // Same rule `InboxKind::Unknown` follows: an older reader must still be
        // able to read the rest of the record.
        let value = serde_json::json!({
            "intent_id": "i-1",
            "session_id": "s",
            "kind": "some_future_kind",
            "source": "user",
            "text": "hi",
            "summary": "hi",
            "created_at": "2026-11-01T00:00:00Z",
            "status": "some_future_status",
        });
        let decoded: Intent = serde_json::from_value(value).expect("data, not an error");
        assert_eq!(decoded.kind, IntentKind::Unknown);
        assert_eq!(decoded.status, IntentStatus::Unknown);
        assert_eq!(decoded.intent_id.as_str(), "i-1");
    }

    #[test]
    fn a_summary_collapses_whitespace_and_keeps_the_text() {
        let intent = Intent::user_direct(
            SessionId::from_string("s"),
            "  one\n\n two\tthree ",
            InboxItemId::from_string("item-1"),
            Utc::now(),
        );
        assert_eq!(intent.summary, "one two three");
        assert_eq!(intent.text, "  one\n\n two\tthree ");
    }

    #[test]
    fn a_long_summary_is_bounded_on_a_character_boundary() {
        let text = "爻".repeat(SUMMARY_CHARS + 10);
        let summary = Intent::summarize(&text);
        assert_eq!(summary.chars().count(), SUMMARY_CHARS + 1, "the ellipsis is one more");
        assert!(summary.ends_with('…'));
    }
}
