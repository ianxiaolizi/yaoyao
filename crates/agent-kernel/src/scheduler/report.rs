//! What one driven turn produced, and the terminal payload that says it.

use std::time::Instant;

use agent_ports::agent_loop::StopReason;
use agent_protocol::ids::{IntentId, RunId, TurnId};
use agent_protocol::inbox::InboxItem;
use agent_protocol::profile::Spent;

/// What one driven turn produced.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub stop_reason: StopReason,
    pub final_content: Option<String>,
    /// What the run spent, as its last completed step reported it.
    pub spent: Spent,
    /// Steps the turn actually took, counted from the log's own markers.
    pub steps: u32,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
    /// The instruction this run answered, when the item it claimed named one.
    ///
    /// `None` is a real answer: the item was queued before the intent ledger,
    /// or by something that is not a person's message (a wakeup, a tool). A
    /// caller that shows this answer as the session's current one has to look
    /// at this field first — that is the whole reason it is here.
    pub intent_id: Option<IntentId>,
}

/// The spend and the clock reading a turn ended with.
///
/// It exists so that the terminal payload and the report are written from one
/// reading of the clock and one set of counters: a kernel that read the clock
/// twice could log an elapsed time that its own report contradicts.
pub(crate) struct Finished {
    spent: Spent,
    steps: usize,
    tool_calls: usize,
    elapsed_ms: u64,
    weight_percent: u32,
}

impl Finished {
    /// Take the reading. `weight_percent` is the profile's cache-hit discount,
    /// used only for the `billable_tokens` view beside the raw totals.
    pub(crate) fn now(
        spent: Spent,
        steps: usize,
        tool_calls: usize,
        started: Instant,
        weight_percent: u32,
    ) -> Self {
        Self {
            spent,
            steps,
            tool_calls,
            elapsed_ms: started.elapsed().as_millis() as u64,
            weight_percent,
        }
    }

    /// Write the end of the run into its terminal payload.
    ///
    /// Every ending carries `spent`: a final answer that cost 2M tokens and one
    /// that cost 2k are different facts, and the log is the only place that can
    /// say which happened. A limit ending additionally carries the legacy
    /// `budget` object, because readers that predate `spent` read that shape and
    /// nothing else. `stop_reason` is not repeated here — the terminal payload
    /// beside this already says it.
    ///
    /// The steps and tool calls are counted from the log's own markers rather
    /// than a loop counter: a step that refused to open — a spent wall clock at
    /// the boundary — returned without one, and it must not be counted as a
    /// step the turn took. The journal counts what it actually wrote, which is
    /// the same number whether the batch was written once or a boundary at a
    /// time.
    pub(crate) fn decorate(&self, payload: &mut serde_json::Value, limit: bool) {
        payload["spent"] = spent_payload(
            self.spent,
            self.steps,
            self.tool_calls,
            self.elapsed_ms,
            self.weight_percent,
        );
        if limit {
            payload["budget"] = serde_json::json!({
                "steps": self.steps,
                "tool_calls": self.tool_calls,
                "elapsed_ms": self.elapsed_ms,
            });
        }
    }

    /// The report the scheduler hands back to its caller.
    pub(crate) fn report(
        self,
        run_id: RunId,
        turn_id: TurnId,
        stop_reason: StopReason,
        final_content: Option<String>,
        intent_id: Option<IntentId>,
    ) -> RunReport {
        RunReport {
            run_id,
            turn_id,
            stop_reason,
            final_content,
            spent: self.spent,
            steps: u32::try_from(self.steps).unwrap_or(u32::MAX),
            tool_calls: u32::try_from(self.tool_calls).unwrap_or(u32::MAX),
            elapsed_ms: self.elapsed_ms,
            intent_id,
        }
    }
}

/// The payload a `turn/started` carries: what it consumed, and — when the item
/// asked for something a person said — the instruction it is an answer *to*.
///
/// The binding lives on `turn/started` rather than on the terminal payload
/// because the terminal events are the run's ending and already carry several
/// shapes for `stop_reason`; a turn that dies before its ending would lose a
/// binding recorded only there, and the log has to be able to say which request
/// was being worked on when it died. An item from before the ledger keeps its
/// exact old payload: a null binding would be a claim the log cannot make.
pub(super) fn turn_started_payload(
    item: &InboxItem,
    intent_id: Option<&IntentId>,
) -> serde_json::Value {
    match intent_id {
        Some(intent_id) => serde_json::json!({
            "inbox_item": item.item_id,
            "intent_id": intent_id,
        }),
        None => serde_json::json!({ "inbox_item": item.item_id }),
    }
}

/// The `spent` object every terminal payload carries.
///
/// The token counts are the loop's, as the provider reported or the estimator
/// supplied them. Steps, tool calls and elapsed time are the journal's and the
/// clock's — facts only the kernel has, which is why the loop's `Spent` cannot
/// carry them itself. `billable_tokens` is the discounted view beside the raw
/// total; it is never what the limit was compared against.
fn spent_payload(
    spent: Spent,
    steps: usize,
    tool_calls: usize,
    elapsed_ms: u64,
    weight_percent: u32,
) -> serde_json::Value {
    let mut value = serde_json::to_value(spent).unwrap_or_else(|_| serde_json::json!({}));
    value["steps"] = serde_json::json!(steps);
    value["tool_calls"] = serde_json::json!(tool_calls);
    value["wall_clock_ms"] = serde_json::json!(elapsed_ms);
    value["cache_hit_weight_percent"] = serde_json::json!(weight_percent);
    value["billable_tokens"] = serde_json::json!(spent.billable_tokens(weight_percent));
    value
}
