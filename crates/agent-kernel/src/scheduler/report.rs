//! What one driven turn produced, and the terminal payload that says it.

use std::time::Instant;

use agent_ports::agent_loop::StopReason;
use agent_protocol::ids::{RunId, TurnId};
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
        }
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
