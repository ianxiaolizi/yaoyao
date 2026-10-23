//! Minimal lifecycle fold for open turn/run/step.
//!
//! The kernel cannot depend on agent-projection (that would invert the
//! dependency direction), so the Scheduler and recovery keep their own fold of
//! the session lifecycle here. It mirrors the session projection's open-entity
//! compare-and-set: a terminal event only closes the entity whose ID it names.

use agent_ports::agent_loop::TurnEnding;
use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{RunId, StepId, TurnId};

/// The currently open run, turn, and step for a session.
#[derive(Debug, Default, PartialEq)]
pub struct Lifecycle {
    pub open_run: Option<RunId>,
    pub open_turn: Option<TurnId>,
    pub open_step: Option<StepId>,
    /// The last turn that ended by yielding — waiting on an approval or on the
    /// user — and that nothing has carried forward since.
    ///
    /// `turn/yielded` clears `open_turn`, which is right (the turn is no longer
    /// running) and not enough: "the session is waiting on a decision" and "the
    /// last turn finished" are different states that a cleared `open_turn`
    /// cannot tell apart, and `resume` has to act on exactly that difference.
    pub yielded_turn: Option<TurnId>,
}

/// Fold a session's event stream into its current open-entity state.
pub fn fold(events: &[StoredEvent]) -> Lifecycle {
    let mut lc = Lifecycle::default();
    for stored in events {
        let e = &stored.event;
        match e.event_type.as_str() {
            EventType::RUN_STARTED => lc.open_run = e.run_id.clone(),
            EventType::RUN_COMPLETED
            | EventType::RUN_FAILED
            | EventType::RUN_CANCELLED
            | EventType::RUN_INTERRUPTED
            | EventType::RUN_YIELDED
                if lc.open_run.is_some() && lc.open_run == e.run_id =>
            {
                lc.open_run = None;
            }
            EventType::TURN_STARTED => {
                lc.open_turn = e.turn_id.clone();
                // A newer turn supersedes an older wait: whatever the session
                // was waiting on stopped being the thing in front of it.
                lc.yielded_turn = None;
            }
            EventType::TURN_RESUMED => {
                if lc.yielded_turn.is_some() && lc.yielded_turn == e.turn_id {
                    lc.yielded_turn = None;
                }
            }
            EventType::TURN_COMPLETED
            | EventType::TURN_FAILED
            | EventType::TURN_CANCELLED
            | EventType::TURN_INTERRUPTED
                if (lc.open_turn.is_some() && lc.open_turn == e.turn_id)
                    || (lc.yielded_turn.is_some() && lc.yielded_turn == e.turn_id) =>
            {
                if lc.open_turn == e.turn_id {
                    lc.open_turn = None;
                }
                if lc.yielded_turn == e.turn_id {
                    lc.yielded_turn = None;
                }
            }
            EventType::TURN_YIELDED => {
                if lc.open_turn.is_some() && lc.open_turn == e.turn_id {
                    lc.open_turn = None;
                }
                lc.yielded_turn = e.turn_id.clone();
            }
            EventType::STEP_STARTED => lc.open_step = e.step_id.clone(),
            EventType::STEP_COMPLETED | EventType::STEP_FAILED | EventType::STEP_INTERRUPTED
                if lc.open_step.is_some() && lc.open_step == e.step_id =>
            {
                lc.open_step = None;
            }
            _ => {}
        }
    }
    lc
}

/// The most recent turn that ended, and how.
///
/// Read from the terminal events alone. A turn that started but never ended —
/// a process that died mid-turn — is not reported as the previous turn: until
/// recovery closes it, "the last turn did not end" is the honest answer, and a
/// model told it ended would act on a fact that is not yet true.
pub fn last_turn_ending(events: &[StoredEvent]) -> Option<(TurnId, TurnEnding)> {
    let mut last = None;
    for stored in events {
        let e = &stored.event;
        let ending = match e.event_type.as_str() {
            EventType::TURN_COMPLETED => TurnEnding::Completed,
            EventType::TURN_FAILED => TurnEnding::Failed,
            EventType::TURN_CANCELLED => TurnEnding::Cancelled,
            EventType::TURN_INTERRUPTED => TurnEnding::Interrupted,
            EventType::TURN_YIELDED => TurnEnding::Yielded,
            _ => continue,
        };
        if let Some(turn_id) = &e.turn_id {
            last = Some((turn_id.clone(), ending));
        }
    }
    last
}
