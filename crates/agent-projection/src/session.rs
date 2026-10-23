//! The Session projection: Session / Turn / Run / Step lifecycle.
//!
//! Tracks which turn, run, and step are currently open, plus lifecycle
//! counters. Terminal events close their target by ID (compare-and-set
//! semantics): a stale terminal event for an already-closed entity is a no-op,
//! so the first terminal fact is never overwritten.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{RunId, SessionId, StepId, TurnId};
use serde::{Deserialize, Serialize};

use crate::definition::{ApplyError, ProjectionDefinition};

/// Folded session lifecycle state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub started: bool,
    /// The currently open turn, run, and step, if any.
    pub open_turn: Option<TurnId>,
    pub open_run: Option<RunId>,
    pub open_step: Option<StepId>,
    /// The last turn that yielded and has not been carried forward since. A
    /// yielded turn is closed (it is not running) yet not finished (it waits on
    /// a decision), and only this field tells the two apart.
    #[serde(default)]
    pub yielded_turn: Option<TurnId>,
    pub turns_started: u64,
    pub turns_terminal: u64,
    pub runs_started: u64,
    pub runs_terminal: u64,
    pub steps_started: u64,
    pub steps_terminal: u64,
    /// The session this one was forked from, and how much of its log was
    /// inherited. Set once, from the child's own header.
    ///
    /// `inherited_events` is a count rather than a set of ids: a reader wants to
    /// know where the child's own history starts, and the header sits at seq 0,
    /// so the inherited events are exactly `seq 1..=inherited_events`. The ids
    /// are in the log for anyone who needs them.
    #[serde(default)]
    pub parent_session_id: Option<SessionId>,
    #[serde(default)]
    pub inherited_events: u64,
}

/// Client-facing view derived from `SessionState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub open: bool,
    pub open_turns: u64,
    pub has_open_turn: bool,
    pub has_open_run: bool,
    pub has_open_step: bool,
    /// Set when this session was forked from another.
    pub inherited_from: Option<SessionId>,
}

/// The Session projection definition.
pub struct SessionProjection;

impl SessionProjection {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SessionProjection {
    fn default() -> Self {
        Self
    }
}

impl ProjectionDefinition for SessionProjection {
    type State = SessionState;
    type View = SessionView;

    fn key(&self) -> &str {
        "session"
    }

    fn state_schema(&self) -> &str {
        "session/0.1.0"
    }

    fn state_version(&self) -> u64 {
        // 3: lineage from `session/forked`.
        3
    }

    fn init(&self) -> Self::State {
        SessionState::default()
    }

    fn apply(&self, state: &Self::State, event: &StoredEvent) -> Result<Self::State, ApplyError> {
        let mut next = state.clone();
        match event.event.event_type.as_str() {
            EventType::SESSION_STARTED => next.started = true,
            // A forked session has begun by definition: whatever the prefix
            // contains, it is history this session now holds. The lineage is
            // read from the payload rather than from the parent's log, because
            // the child has to be readable on its own.
            EventType::SESSION_FORKED => {
                next.started = true;
                // An absent parent leaves this session reading as a root, which
                // is a state the fold can hold honestly (`Option`); the header
                // is written whole by `agent-kernel/src/fork.rs:306-327`, so
                // both reads are anomalies when they come up empty. The count is
                // the one that cannot default: `0` here is not "unknown", it is
                // the *claim* that nothing was inherited — a wrong fact, and the
                // reader that consumes it (`agent-cli`'s "N events inherited")
                // cannot tell it from a real answer. Fail closed (YA-04).
                next.parent_session_id = event
                    .event
                    .payload
                    .get("parent_session_id")
                    .and_then(|v| v.as_str())
                    .map(SessionId::from_string);
                next.inherited_events = event
                    .event
                    .payload
                    .get("inherited_event_count")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| ApplyError::malformed(event, "no inherited_event_count"))?;
            }

            EventType::TURN_STARTED => {
                next.turns_started = next.turns_started.saturating_add(1);
                next.open_turn = event.event.turn_id.clone();
                // A newer turn supersedes an older wait.
                next.yielded_turn = None;
            }
            EventType::TURN_RESUMED => {
                if next.yielded_turn == event.event.turn_id {
                    next.yielded_turn = None;
                }
            }
            EventType::TURN_COMPLETED
            | EventType::TURN_FAILED
            | EventType::TURN_CANCELLED
            | EventType::TURN_INTERRUPTED => {
                // Counted only when the event actually ends the turn it names —
                // the same compare-and-set the module doc promises. A terminal
                // event for a turn that is not open and not waiting is stale,
                // and a stale terminal no more ends a turn than it closes one.
                // Counting events instead of entities would let a late terminal
                // drive `open_turns` (started − terminal) to zero while a turn
                // is still open.
                let closes_open = next.open_turn.is_some() && next.open_turn == event.event.turn_id;
                let closes_waiting =
                    next.yielded_turn.is_some() && next.yielded_turn == event.event.turn_id;
                if closes_open || closes_waiting {
                    next.turns_terminal = next.turns_terminal.saturating_add(1);
                }
                if closes_open {
                    next.open_turn = None;
                }
                if closes_waiting {
                    next.yielded_turn = None;
                }
            }
            EventType::TURN_YIELDED => {
                // A yield ends the turn that was open, because a yielded turn is
                // never carried to a terminal of its own: the continuation is a
                // *new* turn with a new id (`agent-kernel/src/scheduler.rs:153`,
                // which mints a fresh `TurnId` per `run_pending`). Counting the
                // yield is what makes `open_turns` reach zero for a session all
                // of whose turns have stopped — the waiting turn is no longer
                // running, and nothing else will ever end it (C1.5).
                if next.open_turn.is_some() && next.open_turn == event.event.turn_id {
                    next.turns_terminal = next.turns_terminal.saturating_add(1);
                    next.open_turn = None;
                }
                next.yielded_turn = event.event.turn_id.clone();
            }

            EventType::RUN_STARTED => {
                next.runs_started = next.runs_started.saturating_add(1);
                next.open_run = event.event.run_id.clone();
            }
            EventType::RUN_COMPLETED
            | EventType::RUN_FAILED
            | EventType::RUN_CANCELLED
            | EventType::RUN_INTERRUPTED
            | EventType::RUN_YIELDED => {
                if next.open_run.is_some() && next.open_run == event.event.run_id {
                    next.runs_terminal = next.runs_terminal.saturating_add(1);
                    next.open_run = None;
                }
            }

            EventType::STEP_STARTED => {
                next.steps_started = next.steps_started.saturating_add(1);
                next.open_step = event.event.step_id.clone();
            }
            EventType::STEP_COMPLETED | EventType::STEP_FAILED | EventType::STEP_INTERRUPTED => {
                if next.open_step.is_some() && next.open_step == event.event.step_id {
                    next.steps_terminal = next.steps_terminal.saturating_add(1);
                    next.open_step = None;
                }
            }

            // Core types with nothing to say about which entities are open:
            // inbox bookkeeping, model traffic, tool outcomes (the model-context
            // projection's business), approvals, policy, live stream deltas.
            // Skipped as ordinary traffic rather than treated as unknown, the
            // same way the model-context fold skips them — a fold that failed
            // closed on every core type it does not fold could not read any real
            // session. Types outside the core table still fail closed.
            _ if event.event.event_type.is_core() => {}

            _ => return Err(ApplyError::UnknownEventType(event.event.event_type.clone())),
        }
        Ok(next)
    }

    fn view(&self, state: &Self::State) -> Self::View {
        SessionView {
            // A session has no close: it begins and then holds history. Runs and
            // turns end, but the log stays readable, so "open" is exactly
            // "begun". There was once a `closed` flag and a `session/closed`
            // event read into it, but nothing in this build ever wrote that
            // event, so the flag could only ever be false (YA-73).
            open: state.started,
            open_turns: state.turns_started.saturating_sub(state.turns_terminal),
            has_open_turn: state.open_turn.is_some(),
            has_open_run: state.open_run.is_some(),
            has_open_step: state.open_step.is_some(),
            inherited_from: state.parent_session_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId};
    use chrono::Utc;

    fn event(
        event_type: &str,
        turn: Option<&TurnId>,
        run: Option<&RunId>,
        step: Option<&StepId>,
    ) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(0),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::new(),
                run_id: run.cloned(),
                turn_id: turn.cloned(),
                step_id: step.cloned(),
                actor: Actor::Scheduler,
                created_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                payload: serde_json::json!({}),
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    #[test]
    fn turn_lifecycle_opens_and_closes_by_id() {
        let projection = SessionProjection::new();
        let turn = TurnId::new();
        let mut state = SessionState::default();

        state = projection
            .apply(&state, &event(EventType::TURN_STARTED, Some(&turn), None, None))
            .unwrap();
        assert_eq!(state.open_turn, Some(turn.clone()));

        state = projection
            .apply(&state, &event(EventType::TURN_COMPLETED, Some(&turn), None, None))
            .unwrap();
        assert_eq!(state.open_turn, None);
        assert_eq!(state.turns_started, 1);
        assert_eq!(state.turns_terminal, 1);
    }

    #[test]
    fn stale_terminal_does_not_close_a_newer_turn() {
        let projection = SessionProjection::new();
        let old = TurnId::new();
        let new = TurnId::new();
        let mut state = SessionState::default();

        state = projection
            .apply(&state, &event(EventType::TURN_STARTED, Some(&old), None, None))
            .unwrap();
        state = projection
            .apply(&state, &event(EventType::TURN_COMPLETED, Some(&old), None, None))
            .unwrap();
        state = projection
            .apply(&state, &event(EventType::TURN_STARTED, Some(&new), None, None))
            .unwrap();

        // A late terminal event for the old turn must not close the new turn.
        state = projection
            .apply(&state, &event(EventType::TURN_COMPLETED, Some(&old), None, None))
            .unwrap();
        assert_eq!(state.open_turn, Some(new));
    }

    /// The shape the kernel actually writes, taken from a real log rather than
    /// from the ledger's description of it: the yielded turn is **never** carried
    /// to a terminal of its own. It yields, it is resumed, and the continuation
    /// is a *new* turn with a new id and its own `turn/started`
    /// (`agent-kernel/src/scheduler.rs:153` mints a fresh `TurnId` per
    /// `run_pending`). Two turns, two endings, nothing left open.
    ///
    /// This is a boundary test, not a regression probe: the older code counted
    /// the same two endings here (a yield plus a completion), so it passes there
    /// too. What it pins is the semantics, so the next reader cannot re-derive
    /// the ledger's mistaken "one turn counted twice".
    #[test]
    fn a_yielded_turn_ends_at_its_yield_and_the_continuation_is_a_new_turn() {
        let projection = SessionProjection::new();
        let yielded = TurnId::new();
        let continuation = TurnId::new();
        let mut state = SessionState::default();

        for step in [
            event(EventType::TURN_STARTED, Some(&yielded), None, None),
            event(EventType::TURN_YIELDED, Some(&yielded), None, None),
            event(EventType::TURN_RESUMED, Some(&yielded), None, None),
            event(EventType::TURN_STARTED, Some(&continuation), None, None),
            event(EventType::TURN_COMPLETED, Some(&continuation), None, None),
        ] {
            state = projection.apply(&state, &step).unwrap();
        }

        assert_eq!(state.turns_started, 2);
        assert_eq!(state.turns_terminal, 2, "each turn ends exactly once");
        assert_eq!(projection.view(&state).open_turns, 0, "and none is left open");
    }

    /// A terminal ends the turn its id names, and only that turn — the
    /// compare-and-set the module doc promises. Without the id check the count
    /// moved for *any* terminal event, and `open_turns` (started − terminal)
    /// could fall to zero while a turn was still open.
    ///
    /// Not reachable from today's emitters (nothing writes a late terminal), so
    /// this is a robustness probe; it is red on the older code, where the stale
    /// terminal made `turns_terminal` 2.
    #[test]
    fn a_terminal_for_a_turn_that_is_not_open_does_not_move_the_count() {
        let projection = SessionProjection::new();
        let closed = TurnId::new();
        let open = TurnId::new();
        let mut state = SessionState::default();

        for step in [
            event(EventType::TURN_STARTED, Some(&closed), None, None),
            event(EventType::TURN_COMPLETED, Some(&closed), None, None),
            event(EventType::TURN_STARTED, Some(&open), None, None),
        ] {
            state = projection.apply(&state, &step).unwrap();
        }
        // A late terminal for the turn that already closed.
        state = projection
            .apply(&state, &event(EventType::TURN_COMPLETED, Some(&closed), None, None))
            .unwrap();

        assert_eq!(state.turns_terminal, 1, "the stale terminal ended nothing");
        assert_eq!(state.open_turn, Some(open));
        assert_eq!(projection.view(&state).open_turns, 1, "the open turn is still open");
    }

    #[test]
    fn run_and_step_track_open_state() {
        let projection = SessionProjection::new();
        let run = RunId::new();
        let step = StepId::new();
        let mut state = SessionState::default();

        state = projection
            .apply(&state, &event(EventType::RUN_STARTED, None, Some(&run), None))
            .unwrap();
        state = projection
            .apply(&state, &event(EventType::STEP_STARTED, None, Some(&run), Some(&step)))
            .unwrap();
        assert_eq!(state.open_run, Some(run.clone()));
        assert_eq!(state.open_step, Some(step.clone()));

        state = projection
            .apply(&state, &event(EventType::STEP_COMPLETED, None, Some(&run), Some(&step)))
            .unwrap();
        assert_eq!(state.open_step, None);

        state = projection
            .apply(&state, &event(EventType::RUN_COMPLETED, None, Some(&run), None))
            .unwrap();
        assert_eq!(state.open_run, None);
    }

    #[test]
    fn a_forked_session_records_where_it_came_from() {
        let projection = SessionProjection::new();
        let parent = SessionId::from_string("parent");
        let child = SessionId::from_string("child");

        // The header a fork writes, as the child's own first event.
        let mut header = event(EventType::SESSION_FORKED, None, None, None).event;
        header.session_id = child;
        header.payload = serde_json::json!({
            "parent_session_id": "parent",
            "fork_at_event_id": "evt-9",
            "fork_at_turn_id": "turn-2",
            "fork_reason": "try another way",
            "inherited_event_count": 7,
        });
        let stored = StoredEvent { seq: EventSeq::new(0), recorded_at: Utc::now(), event: header };

        let state = projection.apply(&SessionState::default(), &stored).unwrap();
        assert_eq!(state.parent_session_id, Some(parent.clone()));
        assert_eq!(state.inherited_events, 7);
        assert!(state.started, "a session holding history has begun");

        let view = projection.view(&state);
        assert_eq!(view.inherited_from, Some(parent));
        assert!(view.open);
    }

    #[test]
    fn a_fork_header_without_its_count_is_refused() {
        let projection = SessionProjection::new();
        let mut header = event(EventType::SESSION_FORKED, None, None, None);
        header.event.payload = serde_json::json!({ "parent_session_id": "parent" });

        let folded = projection.apply(&SessionState::default(), &header);

        // The count cannot be defaulted: `0` is not "unknown", it is the claim
        // that nothing was inherited, and the reader that prints it ("N events
        // inherited") cannot tell that claim from a real answer.
        let Err(ApplyError::MalformedPayload { event_type, seq, reason }) = folded else {
            panic!("a fork header the fold cannot read must be refused, got {folded:?}");
        };
        assert_eq!(event_type.as_str(), EventType::SESSION_FORKED);
        assert_eq!(seq, EventSeq::new(0));
        assert!(
            reason.contains("inherited_event_count"),
            "the reason must name the field: {reason}"
        );
    }
}
