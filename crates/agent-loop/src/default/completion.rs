//! The operator's completion criterion, run at a step boundary.
//!
//! A run can be finished while the model is still exploring: submissions are
//! in, the grader says yes, and every further step is spend without progress.
//! Waiting for the model to announce that is waiting for a self-assessment, so
//! a profile may state the criterion itself — a command whose exit status is
//! the answer, or an endpoint whose 2xx is — and the loop asks it every
//! `interval_steps` steps.
//!
//! The check is not a tool the model can see or name. It goes through the same
//! gateway a model-made call does, so the profile's policy, the tool's own
//! timeout, and the secret redaction in front of every result all apply; a
//! criterion the gateway refuses is recorded as a check that could not run,
//! never as a completion. Whether it may exist at all is settled where the
//! configuration is resolved (`agent-config`'s completion validation), so a
//! check that runs at all is a check the profile said yes to.
//!
//! Only the status is read: exit 0 for a command, 2xx for an HTTP GET. A
//! criterion that needs a particular body is a criterion the operator can
//! express in the command itself.

use agent_ports::tool_gateway::ToolGateway;
use agent_protocol::ids::SessionId;
use agent_protocol::profile::{CompletionKind, CompletionSpec};
use agent_protocol::redactor::Redactor;
use agent_protocol::tool::{ToolInvocation, ToolStatus};

use super::*;

/// The command tool a command criterion runs through.
///
/// Spelled here rather than imported because the loop sits above the tool
/// crates (they are assembled by the app, not linked by the loop). A gateway
/// that offers no such tool answers `unknown tool`, which the check records
/// as a check that could not run.
const SHELL_TOOL: &str = "shell.run";

/// The HTTP tool an endpoint criterion fetches through. Same reasoning.
const WEB_TOOL: &str = "web.request";

/// The most of a check's output kept in the event.
///
/// The interesting part of a failing check is usually its first line; the
/// whole of it can be a test suite. The tool already bounds what reaches the
/// model — this bounds what reaches the log.
const MAX_DETAIL_CHARS: usize = 500;

/// What one run of the criterion found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Finding {
    /// The criterion passed: the world says the task is done.
    Complete,
    /// The criterion ran and did not pass. The turn continues.
    Incomplete { detail: String },
    /// The check itself could not run — a refused call, a missing tool, a
    /// process that would not start. Not completion, and not a fact about the
    /// task either: the turn continues and the log says the check failed.
    Failed { detail: String },
}

impl Finding {
    /// The word the event uses.
    pub(super) fn status(&self) -> &'static str {
        match self {
            Self::Complete => "completed",
            Self::Incomplete { .. } => "incomplete",
            Self::Failed { .. } => "error",
        }
    }

    /// What the event says happened, bounded and with secrets removed.
    ///
    /// The redactor is the loop's own — the same one the tool boundary uses —
    /// so a token the gateway would have removed does not come back into the
    /// log through the check's error text.
    pub(super) fn detail(&self, redactor: &Redactor) -> Option<String> {
        let detail = match self {
            Self::Complete => return None,
            Self::Incomplete { detail } | Self::Failed { detail } => detail,
        };
        let (redacted, _) = redactor.redact(detail);
        let trimmed = redacted.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut text: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
        if text.chars().count() < trimmed.chars().count() {
            text.push('…');
        }
        Some(text)
    }
}

impl LoopTurn {
    /// The completion criterion this turn runs, if the profile named one.
    pub(super) fn completion_spec(&self) -> Option<&CompletionSpec> {
        self.completion.as_ref()
    }

    /// Whether this step boundary is one the criterion is asked at.
    ///
    /// `step_index` is this step's ordinal, so with an interval of three the
    /// first check runs before the fourth request: after three steps of work,
    /// at the boundary. An interval of one checks before every request.
    pub(super) fn completion_due(&self) -> bool {
        let Some(spec) = self.completion_spec() else {
            return false;
        };
        spec.interval_steps > 0 && self.step_index.is_multiple_of(spec.interval_steps)
    }
}

/// Run the criterion once.
///
/// `call_id` names the synthesized invocation in the gateway's own records; it
/// is derived from the turn and step so two checks in one log never share an
/// id and the same check replayed keeps it.
pub(super) async fn run(
    gateway: &dyn ToolGateway,
    session_id: &SessionId,
    spec: &CompletionSpec,
    call_id: String,
) -> Finding {
    let (tool, arguments) = match spec.kind {
        CompletionKind::Command => (
            SHELL_TOOL,
            serde_json::json!({
                "command": spec.command.clone().unwrap_or_default(),
                "timeout_ms": spec.timeout_ms,
            }),
        ),
        CompletionKind::Http => (
            WEB_TOOL,
            serde_json::json!({
                "method": "GET",
                "url": spec.url.clone().unwrap_or_default(),
                "timeout_ms": spec.timeout_ms,
            }),
        ),
    };
    let invocation = ToolInvocation::from_tool_call(call_id, tool, arguments);
    match gateway.execute(session_id, &invocation).await {
        Ok(result) if result.status == ToolStatus::Completed => Finding::Complete,
        Ok(result) => Finding::Incomplete { detail: result.content },
        Err(error) => Finding::Failed { detail: error.to_string() },
    }
}

/// Ask the criterion at this step boundary, when one is due.
///
/// Returns the outcome that ends the turn when the check passed — the request
/// this boundary would have sent is never sent — and `None` otherwise, leaving
/// `completion/checked` in `events` whether the check failed or could not run.
pub(super) async fn check_at_boundary(
    gateway: &dyn ToolGateway,
    redactor: &Redactor,
    ctx: &TurnContext,
    turn: &mut LoopTurn,
    step_id: &StepId,
    events: &mut Vec<EventDraft>,
) -> Result<Option<StepOutcome>, AgentLoopError> {
    if !turn.completion_due() {
        return Ok(None);
    }
    let spec = turn.completion_spec().cloned().expect("due means present");
    let step_index = turn.step_index;
    let spent = turn.spent;
    // The step so far becomes durable before a process can run: the same rule
    // the tool dispatch follows, for the same reason.
    let journal = turn.journal.clone();
    journal.commit(std::mem::take(events)).await?;
    let call_id = format!("completion/{}/{step_index}", ctx.turn_id);
    let finding = run(gateway, &ctx.session_id, &spec, call_id).await;
    let mut payload = serde_json::json!({
        "kind": spec.kind.as_str(),
        "target": target(&spec),
        "status": finding.status(),
        "steps": step_index,
    });
    if let Some(detail) = finding.detail(redactor) {
        payload["detail"] = serde_json::json!(detail);
    }
    events.push(draft(ctx, step_id, EventType::COMPLETION_CHECKED, payload, Actor::Agent));
    if finding != Finding::Complete {
        return Ok(None);
    }
    Ok(Some(StepOutcome {
        step_id: step_id.clone(),
        events: std::mem::take(events),
        stop: Some(StopReason::Completed),
        final_content: None,
        spent,
    }))
}

/// What the criterion checked, for the event payload: the command or the URL.
pub(super) fn target(spec: &CompletionSpec) -> String {
    match spec.kind {
        CompletionKind::Command => spec.command.clone().unwrap_or_default(),
        CompletionKind::Http => spec.url.clone().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_spec() -> CompletionSpec {
        CompletionSpec {
            kind: CompletionKind::Command,
            command: Some("python check.py".to_string()),
            url: None,
            interval_steps: 1,
            timeout_ms: 1_000,
        }
    }

    #[test]
    fn a_finding_says_which_word_the_event_uses() {
        assert_eq!(Finding::Complete.status(), "completed");
        assert_eq!(Finding::Incomplete { detail: String::new() }.status(), "incomplete");
        assert_eq!(Finding::Failed { detail: String::new() }.status(), "error");
    }

    #[test]
    fn a_long_check_output_is_bounded_and_redacted() {
        let finding =
            Finding::Incomplete { detail: format!("exit code 1\nsk-{}", "a".repeat(4_000)) };
        let detail =
            finding.detail(&Redactor::new()).expect("a failing check has something to say");

        assert!(detail.chars().count() <= MAX_DETAIL_CHARS + 1, "{} chars", detail.len());
        assert!(!detail.contains("sk-"), "a key in the output is not written down: {detail}");
    }

    #[test]
    fn a_check_with_nothing_to_say_records_no_detail() {
        let redactor = Redactor::new();
        assert_eq!(Finding::Complete.detail(&redactor), None);
        assert_eq!(Finding::Incomplete { detail: "  \n ".to_string() }.detail(&redactor), None);
    }

    #[test]
    fn the_target_is_the_criterion_itself() {
        assert_eq!(target(&command_spec()), "python check.py");
    }
}
