use super::*;

/// A turn that keeps working until the scheduler lets it reach a final answer.
struct CountingLoop {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentLoop for CountingLoop {
    async fn begin_turn(&self, _ctx: &TurnContext) -> Result<TurnStart, AgentLoopError> {
        Ok(TurnStart { state: TurnState::new(()), events: vec![] })
    }

    async fn step(
        &self,
        _turn: &mut TurnState,
        _steering: &[SteeringInput],
    ) -> Result<StepOutcome, AgentLoopError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(StepOutcome {
            step_id: StepId::new(),
            events: vec![],
            stop: (call >= 321).then_some(StopReason::FinalAnswer),
            final_content: (call >= 321).then(|| String::from("done")),
            spent: Spent::default(),
        })
    }
}

async fn run_with_ceiling(
    max_steps: Option<u32>,
) -> (Scheduler, Arc<FakeEventStore>, SessionId, Arc<AtomicUsize>) {
    let store = Arc::new(FakeEventStore::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let loop_ = Arc::new(CountingLoop { calls: calls.clone() });
    let scheduler = Scheduler::new(store.clone(), loop_, max_steps);
    let sid = SessionId::from_string("step-budget-session");
    scheduler
        .inbox
        .append(
            &sid,
            InboxKind::NextTurn,
            serde_json::json!({"text": "keep going"}),
            0,
            Actor::User,
        )
        .await
        .unwrap();
    (scheduler, store, sid, calls)
}

#[tokio::test]
async fn an_unbounded_run_crosses_the_old_step_ceilings() {
    let (scheduler, store, sid, calls) = run_with_ceiling(None).await;
    let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
    assert_eq!(report.stop_reason, StopReason::FinalAnswer);
    assert_eq!(calls.load(Ordering::SeqCst), 321);
    assert!(!event_types(&store, &sid).await.iter().any(|kind| kind == "budget/parked"));
}

#[tokio::test]
async fn an_explicit_step_ceiling_still_stops_at_the_boundary() {
    let (scheduler, store, sid, calls) = run_with_ceiling(Some(3)).await;
    let report = scheduler.run_pending(&sid, Utc::now()).await.unwrap().expect("a run");
    assert_eq!(report.stop_reason, StopReason::MaxSteps);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    let parked: Vec<_> =
        events.iter().filter(|event| event.event.event_type.as_str() == "budget/parked").collect();
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].event.payload["parked"]["reason"], serde_json::json!("max_steps"));
}

#[test]
fn a_profile_snapshot_is_the_authority_for_the_step_ceiling() {
    let (scheduler, _store, _sid) = super::scheduler();
    let scheduler = scheduler.with_profile_snapshot(snapshot_with_budget(RunBudget::default()));
    assert_eq!(scheduler.max_steps, None);
    assert_eq!(scheduler.budget.max_steps, None);
}
