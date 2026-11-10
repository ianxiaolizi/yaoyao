//! Small constructors and read-only accessors for Scheduler.

use super::*;
use crate::inbox::Inbox;

impl Scheduler {
    pub fn new(
        store: Arc<dyn EventStore>,
        loop_: Arc<dyn AgentLoop>,
        max_steps: impl Into<Option<u32>>,
    ) -> Self {
        let max_steps = max_steps.into();
        let inbox = Inbox::new(store.clone());
        let budget = RunBudget { max_steps, ..RunBudget::default() };
        Self {
            store,
            inbox,
            loop_,
            max_steps,
            budget,
            completion: None,
            profile: None,
            controls: Vec::new(),
            instructions: None,
            capability: None,
            adjudicator: None,
            security: None,
        }
    }

    /// The scheduler's Inbox, for submitting inputs to this session.
    pub fn inbox(&self) -> &Inbox {
        &self.inbox
    }

    /// A read-only view of the session's intent ledger.
    ///
    /// Cheap to construct: it holds the store handle and every read folds the
    /// log, so there is no second copy of the truth to keep in step.
    pub fn intents(&self) -> crate::intent::IntentLedger {
        crate::intent::IntentLedger::new(self.store.clone())
    }
}
