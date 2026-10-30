//! The journal a run writes through: sequenced, flushed, and counted.
//!
//! Every durable write a Scheduler turn makes goes through this type, so the
//! lease, the sequence number and the flush live in one place rather than at
//! each call site.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use agent_ports::agent_loop::EventSink;
use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventSeq, RunId, SessionId, TurnId};

use super::draft;

/// The Scheduler's [`EventSink`]: this session's log, appended under the write
/// lease and flushed before returning.
///
/// Sequence and write are one step here for the same reason they are at the
/// terminal batch (YA-36): the number is read from the log and consumed by the
/// append, and a number read outside the lease could be moved by another
/// process before the append lands.
pub(crate) struct SessionJournal {
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) session_id: SessionId,
    /// Steps that actually opened, and calls that actually started — counted
    /// as their markers pass through, so a turn's budget report does not
    /// depend on how many boundaries the turn happened to be written in.
    steps: AtomicU32,
    calls: AtomicU32,
}

impl SessionJournal {
    pub(crate) fn new(store: Arc<dyn EventStore>, session_id: SessionId) -> Self {
        Self { store, session_id, steps: AtomicU32::new(0), calls: AtomicU32::new(0) }
    }

    pub(crate) fn steps(&self) -> usize {
        self.steps.load(Ordering::Relaxed) as usize
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed) as usize
    }
}

#[async_trait::async_trait]
impl EventSink for SessionJournal {
    async fn commit(&self, drafts: Vec<EventDraft>) -> Result<(), EventStoreError> {
        if drafts.is_empty() {
            return Ok(());
        }
        for event in &drafts {
            match event.event_type.as_str() {
                EventType::STEP_STARTED => self.steps.fetch_add(1, Ordering::Relaxed),
                EventType::TOOL_STARTED => self.calls.fetch_add(1, Ordering::Relaxed),
                _ => continue,
            };
        }
        let _lease = self.store.lease(&self.session_id).await?;
        let events = self.store.read_stream(&self.session_id, EventSeq::ZERO, usize::MAX).await?;
        let next = events.last().map(|event| event.seq.next()).unwrap_or(EventSeq::ZERO);
        self.store.append(&self.session_id, next, drafts).await?;
        // Accepting the append is not the promise this method makes: it says
        // the fact survives losing the process, so it waits for the disk.
        self.store.flush(&self.session_id).await?;
        Ok(())
    }
}

/// Close a turn the loop failed inside, so the log says a run failed rather
/// than leaving it open for recovery to call it an interruption.
///
/// Best effort on purpose: the caller is already returning an error, and a
/// store that cannot take these two events is a store whose next caller runs
/// recovery, which closes an open run exactly as well.
pub(crate) async fn close_failed_turn(
    journal: &SessionJournal,
    run_id: &RunId,
    turn_id: &TurnId,
    mut pending: Vec<EventDraft>,
) {
    let session_id = &journal.session_id;
    pending.push(draft(
        session_id,
        EventType::TURN_FAILED,
        serde_json::json!({}),
        Actor::Scheduler,
        Some(run_id),
        Some(turn_id),
        None,
    ));
    pending.push(draft(
        session_id,
        EventType::RUN_FAILED,
        serde_json::json!({}),
        Actor::Scheduler,
        Some(run_id),
        None,
        None,
    ));
    let _ = journal.commit(pending).await;
}
