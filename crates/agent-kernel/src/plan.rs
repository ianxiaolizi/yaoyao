//! Minimal plan fold for the session's ledger.
//!
//! The kernel cannot depend on agent-projection, so the Scheduler reads the
//! plan straight from the event stream here, the way it reads the lifecycle.
//! The rule is the ledger's whole semantics: each `plan/updated` is a full
//! replacement, so the latest one is the current plan and the earlier ones are
//! history.

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::plan::{Plan, StepWorker, WorkerState};

/// The plan the session currently carries, if any.
///
/// A malformed payload is an error rather than "no plan": the event was
/// written by this build, so a payload that will not read back is a log
/// anomaly, and silently opening the next turn with no plan would hide it.
pub fn current(events: &[StoredEvent]) -> Result<Option<Plan>, serde_json::Error> {
    for stored in events.iter().rev() {
        if stored.event.event_type.as_str() != EventType::PLAN_UPDATED {
            continue;
        }
        let plan =
            serde_json::from_value(stored.event.payload.get("plan").cloned().unwrap_or_default())?;
        return Ok(Some(plan));
    }
    Ok(None)
}

/// The workers this session started for its plan's steps.
///
/// Two events make one worker: the spawn names the child and the step on its
/// request, and the first settlement naming that child ends it. Both are
/// matched by the child's session id — the thing `subagent.wait` takes — so a
/// session that delegates to the same backend twice keeps its workers apart.
///
/// A spawn that names no step is not a worker: a child the plan does not know
/// about is a delegation, not a step being carried, and putting it in the
/// table would make the task layer talk about work nobody ordered.
pub fn workers(events: &[StoredEvent]) -> Vec<StepWorker> {
    let mut workers: Vec<StepWorker> = Vec::new();
    for stored in events {
        let event_type = stored.event.event_type.as_str();
        if !event_type.starts_with("subagent/") {
            continue;
        }
        let payload = &stored.event.payload;
        let Some(child) = payload.get("child_session_id").and_then(|id| id.as_str()) else {
            continue;
        };
        if event_type == EventType::SUBAGENT_SPAWNED {
            let step = payload
                .get("request")
                .and_then(|request| request.get("step"))
                .and_then(|step| step.as_str());
            if let Some(step) = step {
                workers.push(StepWorker {
                    step: step.to_string(),
                    child: child.to_string(),
                    state: WorkerState::Running,
                });
            }
            continue;
        }
        let Some(worker) = workers
            .iter_mut()
            .rev()
            .find(|worker| worker.child == child && worker.state == WorkerState::Running)
        else {
            continue;
        };
        worker.state = match event_type {
            EventType::SUBAGENT_COMPLETED => WorkerState::Completed,
            EventType::SUBAGENT_FAILED => WorkerState::Failed,
            EventType::SUBAGENT_CANCELLED => WorkerState::Cancelled,
            // A settlement this fold does not know is not guessed at: the
            // worker stays running, which is the safe reading — a step whose
            // ending is unknown is not ready to start again.
            _ => continue,
        };
    }
    workers
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft, EventType};
    use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId};
    use chrono::Utc;

    fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: agent_protocol::ids::EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("s"),
                run_id: None,
                turn_id: None,
                step_id: None,
                actor: Actor::Agent,
                created_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    fn plan_event(seq: u64, goal: &str) -> StoredEvent {
        stored(
            seq,
            EventType::PLAN_UPDATED,
            serde_json::json!({ "provider_tool_call_id": "call-1", "plan": { "goal": goal } }),
        )
    }

    #[test]
    fn no_plan_event_means_no_plan() {
        let events = vec![stored(1, EventType::TURN_STARTED, serde_json::json!({}))];
        assert_eq!(current(&events).expect("readable"), None);
    }

    #[test]
    fn the_latest_plan_replaces_the_previous_one() {
        let events = vec![plan_event(1, "first"), plan_event(2, "second")];
        let plan = current(&events).expect("readable").expect("a plan");
        assert_eq!(plan.goal, "second");
        assert!(plan.items.is_empty(), "no items recorded");
    }

    #[test]
    fn a_plan_event_without_a_readable_plan_is_an_error() {
        let broken = stored(1, EventType::PLAN_UPDATED, serde_json::json!({}));
        assert!(current(&[broken]).is_err());
    }

    fn spawned(seq: u64, child: &str, step: Option<&str>) -> StoredEvent {
        let mut request = serde_json::json!({ "objective": "carry the step" });
        if let Some(step) = step {
            request["step"] = serde_json::json!(step);
        }
        stored(
            seq,
            EventType::SUBAGENT_SPAWNED,
            serde_json::json!({
                "child_session_id": child,
                "provider": "fake",
                "request": request,
            }),
        )
    }

    fn settled(seq: u64, child: &str, event_type: &str) -> StoredEvent {
        stored(
            seq,
            event_type,
            serde_json::json!({
                "child_session_id": child,
                "provider": "fake",
                "outcome": { "status": "completed" },
            }),
        )
    }

    #[test]
    fn a_spawn_naming_a_step_is_a_worker_until_its_settlement_arrives() {
        let events =
            vec![spawned(1, "child-1", Some("build")), spawned(2, "child-2", Some("test"))];
        let found = workers(&events);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].step, "build");
        assert_eq!(found[0].child, "child-1");
        assert_eq!(found[0].state, WorkerState::Running);
        assert_eq!(found[1].state, WorkerState::Running);

        let events = [events, vec![settled(3, "child-1", EventType::SUBAGENT_COMPLETED)]].concat();
        let found = workers(&events);
        assert_eq!(found[0].state, WorkerState::Completed, "the settlement follows the child");
        assert_eq!(found[1].state, WorkerState::Running, "the other child is untouched");
    }

    #[test]
    fn a_settlement_under_each_family_names_the_state_it_means() {
        for (event_type, state) in [
            (EventType::SUBAGENT_COMPLETED, WorkerState::Completed),
            (EventType::SUBAGENT_FAILED, WorkerState::Failed),
            (EventType::SUBAGENT_CANCELLED, WorkerState::Cancelled),
        ] {
            let events =
                vec![spawned(1, "child-1", Some("build")), settled(2, "child-1", event_type)];
            assert_eq!(workers(&events)[0].state, state, "{event_type}");
        }
    }

    #[test]
    fn a_delegation_that_named_no_step_is_not_a_worker() {
        // A child the plan does not know about is a delegation, not a step being
        // carried: putting it in the table would make the task layer talk about
        // work nobody ordered.
        let events =
            vec![spawned(1, "child-1", None), settled(2, "child-1", EventType::SUBAGENT_COMPLETED)];
        assert!(workers(&events).is_empty());
    }

    #[test]
    fn a_settlement_for_a_child_this_fold_never_saw_start_is_ignored() {
        // The parent's log can hold a settlement whose spawn was trimmed away or
        // written by a build that did not record the step. There is no worker to
        // end, and inventing one would put a step in the table that no plan
        // named.
        let events = vec![settled(1, "child-1", EventType::SUBAGENT_COMPLETED)];
        assert!(workers(&events).is_empty());
    }
}
