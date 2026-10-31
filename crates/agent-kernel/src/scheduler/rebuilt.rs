//! The scheduler, rebuilt for a second session.
//!
//! A delegated child runs on the same terms as its parent and under a budget the
//! parent's profile did not state (CONTRACT §7 #133). That is not a second kind
//! of run — it is the same run configuration with one input replaced — so it
//! lives beside the scheduler rather than inside its turn logic.

use std::sync::Arc;

use agent_ports::agent_loop::AgentLoop;
use agent_protocol::profile::RunBudget;

use super::{Inbox, Scheduler};

impl Scheduler {
    /// The same run configuration under a different budget.
    ///
    /// For the caller that drives a **second session** on this scheduler's
    /// terms but under a spend limit the profile did not state: a delegated
    /// child, whose limit is its parent's request. Everything that makes a run a
    /// run is carried over — the store, the loop, the frozen profile, the
    /// instructions and the capability facts — and only `budget`, with the step
    /// limit it carries, changes.
    ///
    /// The profile snapshot is carried unchanged, so a child's log records the
    /// configuration its parent ran under. What the child was *actually* held to
    /// is stated where delegation states it: the request in `subagent/spawned`,
    /// and the `budget` facts on the child's own terminal event.
    pub fn rebudgeted(&self, budget: RunBudget) -> Self {
        self.rebuilt(self.loop_.clone(), budget)
    }

    /// The same run configuration with a different loop.
    ///
    /// For the child whose turn runs on **this session's terms and a different
    /// surface**: an in-process child's turn is the same run under the same
    /// frozen profile, held to a read-only tool surface — and the surface is the
    /// loop, not a setting on it (CONTRACT §7 #133). Everything else, the budget
    /// included, is carried over; a request that names a budget narrows it
    /// afterwards with [`rebudgeted`](Self::rebudgeted), so each replacement is
    /// stated once.
    pub fn relooped(&self, loop_: Arc<dyn AgentLoop>) -> Self {
        self.rebuilt(loop_, self.budget.clone())
    }

    /// Every field of `self`, with `loop_` and `budget` replacing the two that
    /// differ. The step limit travels with the budget because the two are one
    /// number stated twice.
    fn rebuilt(&self, loop_: Arc<dyn AgentLoop>, budget: RunBudget) -> Self {
        Self {
            store: self.store.clone(),
            inbox: Inbox::new(self.store.clone()),
            loop_,
            max_steps: budget.max_steps,
            budget,
            completion: self.completion.clone(),
            profile: self.profile.clone(),
            controls: self.controls.clone(),
            instructions: self.instructions.clone(),
            capability: self.capability.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent_ports::agent_loop::AgentLoop;
    use agent_ports::model_provider::ModelResponse;
    use agent_protocol::ids::SessionId;
    use agent_protocol::profile::RunBudget;
    use agent_testkit::{
        FakeEventStore, FakeModelProvider, FakeToolGateway, FakeToolProvider, NullContextEngine,
        one_model_router,
    };

    use crate::scheduler::Scheduler;
    use agent_loop::DefaultAgentLoop;

    fn scheduler() -> Scheduler {
        let store = Arc::new(FakeEventStore::new());
        let loop_ = Arc::new(DefaultAgentLoop::new(
            Arc::new(FakeModelProvider::new(Vec::new())),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ));
        Scheduler::new(store, loop_, 8)
    }

    /// The step limit `new` was given and the budget a run executes under are
    /// the same number when the budget is the one that names it: a rebuilt
    /// scheduler that kept one and took the other from somewhere else would
    /// enforce two limits where the caller stated one.
    #[test]
    fn a_rebuilt_scheduler_takes_its_step_limit_from_the_budget_it_was_given() {
        let budget = RunBudget { max_steps: 3, ..RunBudget::default() };
        let rebuilt = scheduler().rebudgeted(budget);

        assert_eq!(rebuilt.budget, budget);
        assert_eq!(rebuilt.max_steps, 3);
    }

    /// Rebuilding is a narrowing of one input, not a change to the scheduler it
    /// came from: the caller still holds the original for its own session.
    #[test]
    fn rebudgeting_leaves_the_scheduler_it_was_taken_from_alone() {
        let original = scheduler();
        let before = original.budget;
        let _ = original.rebudgeted(RunBudget { max_steps: 1, ..RunBudget::default() });

        assert_eq!(original.budget, before);
        assert_eq!(original.max_steps, 8);
    }

    /// Replacing the loop replaces the only thing that runs a turn, and nothing
    /// else: a child held to a different surface is still held to the budget its
    /// parent was given.
    #[tokio::test]
    async fn a_relooped_scheduler_runs_its_own_loop_under_the_budget_it_inherited() {
        let store = Arc::new(FakeEventStore::new());
        let parent = Scheduler::new(store, answering("the parent's loop"), 8);
        let child = parent.relooped(answering("the child's loop"));
        let session = SessionId::from_string("child");

        assert_eq!(child.budget, parent.budget);
        assert_eq!(child.max_steps, 8);

        child
            .inbox()
            .append(
                &session,
                agent_protocol::inbox::InboxKind::NextTurn,
                serde_json::json!({ "text": "hi" }),
                0,
                agent_protocol::events::Actor::User,
            )
            .await
            .unwrap();
        let report = child.run_pending(&session, chrono::Utc::now()).await.unwrap().expect("a run");

        assert_eq!(report.final_content.as_deref(), Some("the child's loop"));
    }

    /// A loop that answers every turn with `answer`.
    fn answering(answer: &str) -> Arc<dyn AgentLoop> {
        Arc::new(DefaultAgentLoop::new(
            Arc::new(FakeModelProvider::new(vec![ModelResponse::text(answer)])),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            Arc::new(NullContextEngine::new()),
            one_model_router(),
        ))
    }

    /// The store is the same one, so a child's log is in the same place as its
    /// parent's and a driver lease on one session does not block the other.
    #[tokio::test]
    async fn a_rebuilt_scheduler_drives_a_different_session_over_the_same_store() {
        let original = scheduler();
        let rebuilt = original.rebudgeted(RunBudget::default());
        let parent = SessionId::from_string("parent");
        let child = SessionId::from_string("child");

        assert!(original.lifecycle(&parent).await.is_ok());
        assert!(rebuilt.lifecycle(&child).await.is_ok());
        assert!(
            rebuilt.lifecycle(&parent).await.is_ok(),
            "the same store answers for the parent too"
        );
    }
}
