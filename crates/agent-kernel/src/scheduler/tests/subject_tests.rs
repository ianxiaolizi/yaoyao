//! Scheduler-level proof that the turn context carries an intent subject.

use std::sync::{Arc, Mutex};

use agent_ports::agent_loop::{
    AgentLoop, AgentLoopError, SteeringInput, TurnContext, TurnStart, TurnState,
};
use agent_protocol::events::EventType;
use agent_protocol::ids::{EventSeq, SessionId, StepId};
use agent_protocol::profile::Spent;
use agent_testkit::FakeEventStore;
use chrono::Utc;

use super::super::*;

struct RecordingLoop {
    seen: Mutex<Vec<TurnContext>>,
}

#[async_trait::async_trait]
impl AgentLoop for RecordingLoop {
    async fn begin_turn(&self, ctx: &TurnContext) -> Result<TurnStart, AgentLoopError> {
        self.seen.lock().unwrap().push(ctx.clone());
        Ok(TurnStart { state: TurnState::new(()), events: vec![] })
    }

    async fn step(
        &self,
        _turn: &mut TurnState,
        _steering: &[SteeringInput],
    ) -> Result<agent_ports::agent_loop::StepOutcome, AgentLoopError> {
        Ok(agent_ports::agent_loop::StepOutcome {
            step_id: StepId::new(),
            events: vec![],
            stop: Some(agent_ports::agent_loop::StopReason::FinalAnswer),
            final_content: Some("done".to_string()),
            spent: Spent::default(),
        })
    }
}

#[tokio::test]
async fn a_turn_carries_the_intent_and_its_record_sequence_as_its_subject() {
    let store = Arc::new(FakeEventStore::new());
    let loop_ = Arc::new(RecordingLoop { seen: Mutex::new(Vec::new()) });
    let scheduler = Scheduler::new(store.clone(), loop_.clone(), 8);
    let sid = SessionId::from_string("intent-subject-session");
    let now = Utc::now();
    let submitted =
        scheduler.inbox.submit_user_message(&sid, "change the parser", now).await.unwrap();

    scheduler.run_pending(&sid, now).await.unwrap().expect("a run");
    let ctx = loop_.seen.lock().unwrap().remove(0);

    assert_eq!(ctx.authorization_subject.task_id, submitted.intent.intent_id.as_str());
    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let recorded = events
        .iter()
        .find(|stored| {
            stored.event.event_type.as_str() == EventType::INTENT_RECORDED
                && stored.event.payload.get("intent_id").and_then(|value| value.as_str())
                    == Some(submitted.intent.intent_id.as_str())
        })
        .expect("the intent is in the log");
    assert_eq!(ctx.authorization_subject.task_revision, recorded.seq.as_u64() + 1);
    assert_ne!(ctx.authorization_subject.task_revision, 0);
}
