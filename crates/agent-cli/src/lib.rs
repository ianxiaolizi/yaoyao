//! agent-cli: CLI presentation adapter and command runner.

#![deny(unsafe_code)]

pub mod args;
pub mod commands;
pub mod context;
pub mod memory;
pub mod reason;
pub mod schedule;

use agent_kernel::approval::ApprovalService;
use agent_kernel::doctor::KernelDoctor;
use agent_kernel::fork::{Fork, ForkOutcome};
use agent_kernel::inbox::Inbox;
use agent_kernel::recovery::RecoveryReport;
use agent_kernel::resume::{Disposition, Resume, ResumeOutcome};
use agent_kernel::scheduler::Scheduler;
use agent_ports::artifact_store::{ArtifactStore, ArtifactStoreError};
use agent_ports::event_store::EventStore;
use agent_ports::tool_gateway::ToolGateway;
use agent_ports::tool_provider::ToolInvocation;
use agent_protocol::diagnostics::HealthStatus;
use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId, TurnId};
use agent_protocol::permissions::{ApprovalScope, ArtifactRef};
use agent_protocol::redactor::Redactor;
use agent_protocol::tool::ToolStatus;
use chrono::{DateTime, Utc};

/// The one shape a rendered `--json` answer has.
///
/// The module is private and so is the field, which together make
/// [`json_output`] the only way to build a [`JsonDoc`] — not just for other
/// crates, but for every other file in this one. That is the point: a command
/// that would rather emit a different shape cannot express it, so the shape is
/// not a convention to be remembered but a fact the compiler checks. A test
/// could have said the same thing more weakly; this says it where the mistake
/// would be made.
mod shape {
    /// A rendered single-object `--json` answer.
    ///
    /// Pretty because these documents are read by people at least as often as by
    /// parsers, and a parser does not care about whitespace. The line-delimited
    /// `--events` chain is a *different* shape on purpose ([`crate::render_events`]:
    /// one compact object per line, so it can be `tail -f`'d), and it is not a
    /// `CliOutput` at all, so it does not come through here.
    #[derive(Debug)]
    pub struct JsonDoc(String);

    impl JsonDoc {
        /// The document as bytes, ready to print.
        pub fn as_str(&self) -> &str {
            &self.0
        }
    }

    impl std::fmt::Display for JsonDoc {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.0)
        }
    }

    /// Render one single-object `--json` answer.
    ///
    /// The only constructor of [`JsonDoc`], and therefore the one place the shape
    /// of every `--json` answer is decided.
    pub fn json_output<T: serde::Serialize>(value: &T) -> Result<JsonDoc, String> {
        serde_json::to_string_pretty(value).map(JsonDoc).map_err(|e| e.to_string())
    }
}

pub use shape::{JsonDoc, json_output};

/// Run a CLI command and return output.
#[derive(Debug)]
pub enum CliOutput {
    Text(String),
    Json(JsonDoc),
}

/// What became of a command, in the same three terms its exit status speaks.
///
/// Kept apart from `Result::Err`, which means the command could not be carried
/// out at all. A tool that ran and failed *was* carried out: its result is the
/// answer, it belongs on stdout, and what the exit status adds is that nobody
/// has to parse the answer to learn which kind of answer it was. Collapsing the
/// two would put the result on stderr for a caller who asked for it on stdout.
///
/// `DidNotRun` is the third term, and the one that used to have none: the
/// command ran, and the thing it was asked to do did not happen. A run that
/// could not start because one was already active, a resume with nothing to
/// resume. That is neither an answer nor a failure to answer — it is the
/// absence of an answer, and a caller reading only `$?` has to be able to tell
/// it from both (§3 C3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Verdict {
    /// The command ran and its answer stands, whatever the answer was.
    Answered,
    /// The command ran, and what it came back with was a failure.
    AnsweredNo,
    /// The command ran, and nothing happened: no answer, only a reason.
    DidNotRun,
}

/// What a command produced, and the verdict its exit status carries.
///
/// `must_use`, because a verdict that is dropped is the whole defect this
/// carries: every command that is handed one has to end on it.
#[derive(Debug)]
#[must_use]
pub struct CliOutcome {
    pub output: CliOutput,
    pub verdict: Verdict,
}

impl CliOutcome {
    /// The ordinary case: the command did what it was asked to.
    pub fn ok(output: CliOutput) -> Self {
        Self { output, verdict: Verdict::Answered }
    }

    /// The command ran, and what it ran to was a failure.
    pub fn failed(output: CliOutput) -> Self {
        Self { output, verdict: Verdict::AnsweredNo }
    }

    /// The command ran, and the thing it was asked to do did not happen.
    ///
    /// The output still belongs on stdout: what did not happen needs saying,
    /// and what it is said with is a reason, not a failure report.
    pub fn did_not_run(output: CliOutput) -> Self {
        Self { output, verdict: Verdict::DidNotRun }
    }
}

impl From<CliOutput> for CliOutcome {
    fn from(output: CliOutput) -> Self {
        Self::ok(output)
    }
}

/// One event per line, as JSON.
///
/// Line-delimited rather than one array because this is a chain: it can be
/// appended to, `tail -f`'d, and read one event at a time without holding the
/// whole session in memory. Every field a reader might key on is lifted out of
/// the payload, so spotting "the turn that failed" does not require knowing the
/// payload shape of each event type.
pub fn render_events(events: &[StoredEvent]) -> String {
    events.iter().map(render_event).collect::<Vec<_>>().join("\n")
}

fn render_event(stored: &StoredEvent) -> String {
    let event = &stored.event;
    serde_json::json!({
        "seq": stored.seq.as_u64(),
        "recorded_at": stored.recorded_at.to_rfc3339(),
        "event_id": event.event_id.as_str(),
        "type": event.event_type.as_str(),
        "session_id": event.session_id.as_str(),
        "actor": event.actor,
        "run_id": event.run_id.as_ref().map(|id| id.as_str()),
        "turn_id": event.turn_id.as_ref().map(|id| id.as_str()),
        "step_id": event.step_id.as_ref().map(|id| id.as_str()),
        "payload": event.payload,
    })
    .to_string()
}

/// A piece of live model output, labelled by what it is.
#[derive(Debug, PartialEq)]
pub struct LiveChunk {
    /// The marker that opens a run of this kind of text, e.g. `[model] `.
    pub label: &'static str,
    pub text: String,
}

/// What one live event looks like to a person watching, if anything at all.
///
/// Structural live events render to nothing. `stream.started` is a boundary and
/// a tool-call delta is half an argument; neither is text a person reads, and
/// both are already named in the durable chain on stdout. Rendering them again
/// as prose would be inventing a summary.
pub fn render_live(event: &agent_protocol::events::LiveEvent) -> Option<LiveChunk> {
    use agent_protocol::model::ModelStreamEvent;

    let chunk = match serde_json::from_value::<ModelStreamEvent>(event.payload.clone()) {
        Ok(ModelStreamEvent::TextDelta { text }) => LiveChunk { label: "[model] ", text },
        Ok(ModelStreamEvent::ReasoningDelta { text }) => LiveChunk { label: "[think] ", text },
        _ => return None,
    };
    Some(chunk)
}

/// Assembles live chunks into the lines a terminal should show.
///
/// A run of deltas is one line: the label opens it and the text follows without
/// interruption, so a model's typing reads as typing. The line is closed — by a
/// newline — as soon as something else needs the terminal, and closing twice
/// prints nothing the second time.
#[derive(Default)]
pub struct LiveLines {
    open: Option<&'static str>,
}

impl LiveLines {
    /// The text to write for this chunk, which may open a line first.
    pub fn push(&mut self, chunk: LiveChunk) -> String {
        match self.open {
            Some(label) if label == chunk.label => chunk.text,
            Some(_) => {
                self.open = Some(chunk.label);
                format!("\n{}{}", chunk.label, chunk.text)
            }
            None => {
                self.open = Some(chunk.label);
                format!("{}{}", chunk.label, chunk.text)
            }
        }
    }

    /// The text that closes an open line, before anything else is written to
    /// the same stream.
    pub fn finish(&mut self) -> &'static str {
        match self.open.take() {
            Some(_) => "\n",
            None => "",
        }
    }
}

/// Execute the `doctor` command.
///
/// An unhealthy report is an answer, and it exits `1` (§3 C3.4, 裁决 #5): the
/// doctor ran to the end and what it found was bad. The output still belongs on
/// stdout in whichever shape was asked for — what the status adds is that
/// nobody has to parse the output to learn that the answer was bad.
///
/// A report that cannot be rendered is the one thing here that is not an
/// answer: it is `Err`, because the command could not be carried out at all —
/// the distinction [`CliOutcome`] draws. The old fallback printed an error
/// *object* on stdout and called it a verdict, which claimed a check had been
/// read when what happened is that it could not be written down.
pub async fn run_doctor(
    doctor: &KernelDoctor,
    schema_version: SchemaVersion,
    json: bool,
) -> Result<CliOutcome, String> {
    let report = doctor.report(schema_version).await;
    // Only a failed check makes a report unhealthy: `HealthReport::aggregate` is
    // `Failed` if any check failed and `Passed` otherwise, so a check that could
    // not judge does not by itself make the build unhealthy. That is a rule
    // about what "unhealthy" means, and it belongs to the aggregate rather than
    // here — this only says that whatever the aggregate calls unhealthy exits
    // non-zero.
    let verdict = match report.aggregate_status {
        HealthStatus::Failed => Verdict::AnsweredNo,
        HealthStatus::Passed | HealthStatus::Skipped => Verdict::Answered,
    };
    let output = if json {
        CliOutput::Json(json_output(&report)?)
    } else {
        let mut lines = Vec::new();
        for check in &report.checks {
            let status_icon = match check.status {
                HealthStatus::Passed => "✓",
                HealthStatus::Failed => "✗",
                HealthStatus::Skipped => "–",
            };
            lines.push(format!("{} [{}] {}", status_icon, check.check_id.0, check.message));
        }
        let overall = match report.aggregate_status {
            HealthStatus::Passed => "healthy",
            HealthStatus::Failed => "unhealthy",
            HealthStatus::Skipped => "skipped",
        };
        lines.push(format!("\nOverall: {}", overall));
        CliOutput::Text(lines.join("\n"))
    };
    Ok(CliOutcome { output, verdict })
}

/// Execute the `ask`/`run` command: submit a `next_turn` input and drive one run.
///
/// `ask` and `run` are the same command under two names. They were meant to
/// differ — the plan had `run` carrying recovery, the coding toolset, and
/// steering-aware semantics — but `ask` already has all three: `assemble()`
/// builds one tool surface for every command and both call sites run recovery
/// first. Two names for one behavior is worth saying out loud rather than
/// inventing a difference to justify the second name; the split belongs with
/// the profile system, where "ask is the read-only profile" is a real rule.
///
/// The scheduler declining to run is not the same kind of answer as a turn that
/// ran and ended: the input this command appended is still sitting in the inbox
/// untouched, so the command's verdict is `DidNotRun` and it exits `2` (§3 C3.5,
/// 裁决 #6). Handing the caller a reason as an ordinary answer would tell a
/// script that the text had been run.
pub async fn run_ask(
    scheduler: &Scheduler,
    session_id: &SessionId,
    text: &str,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    let started =
        commands::start_turn(scheduler, session_id, text, now).await.map_err(|e| e.to_string())?;

    match started {
        commands::TurnStart::Ran(r) => {
            if json {
                let value = serde_json::json!({
                    "run_id": r.run_id.as_str(),
                    "turn_id": r.turn_id.as_str(),
                    "stop_reason": r.stop_reason,
                    "final_content": r.final_content,
                });
                Ok(CliOutcome::ok(CliOutput::Json(json_output(&value)?)))
            } else {
                match r.final_content {
                    Some(content) => Ok(CliOutcome::ok(CliOutput::Text(content))),
                    None => Ok(CliOutcome::ok(CliOutput::Text(format!(
                        "[turn ended: {:?}]",
                        r.stop_reason
                    )))),
                }
            }
        }
        commands::TurnStart::NotStarted => Ok(CliOutcome::did_not_run(if json {
            let value = serde_json::json!({ "run": null, "reason": "busy_or_no_input" });
            CliOutput::Json(json_output(&value)?)
        } else {
            CliOutput::Text(
                "no run: a run is already active or there is no pending input".to_string(),
            )
        })),
    }
}

/// Execute the `resume` command: settle the call a paused turn is waiting on,
/// then drive the turn that carries on from it.
///
/// Two of its outcomes are answers and one is not. `Waiting` says the turn is
/// still waiting on a decision: that is `reply` — a pause for a person is an
/// answer in this repository, the same one `agent run` gives when it stops at an
/// approval (§3 C3.3) — and it exits `0`. `Settled` did the work it was asked
/// for. `NothingToResume` is the odd one: there was no wait to settle, so
/// nothing ran, and it exits `2` (§3 C3.5, 裁决 #7). A caller that reads only
/// `$?` must not read "there was nothing to do" as "the thing is done".
pub async fn run_resume(
    resume: &Resume,
    scheduler: &Scheduler,
    session_id: &SessionId,
    text: &str,
    now: DateTime<Utc>,
    json: bool,
) -> Result<CliOutcome, String> {
    // An empty text is not "no message": the kernel composes the continuation
    // from what settled, because it is the only place that holds both the call
    // and its outcome. See `Resume::resume`.
    let outcome = resume.resume(session_id, text).await.map_err(|e| e.to_string())?;

    match outcome {
        ResumeOutcome::NothingToResume => Ok(CliOutcome::did_not_run(if json {
            CliOutput::Json(json_output(&serde_json::json!({ "outcome": "nothing_to_resume" }))?)
        } else {
            CliOutput::Text("nothing to resume: no turn is waiting on a decision".to_string())
        })),
        ResumeOutcome::Waiting { approval_id, tool_name } => Ok(CliOutcome::ok(if json {
            CliOutput::Json(json_output(&serde_json::json!({
                "outcome": "waiting",
                "approval_id": approval_id.as_str(),
                "tool_name": tool_name,
            }))?)
        } else {
            CliOutput::Text(format!(
                "still waiting: {tool_name} needs {approval_id} to be approved or denied first"
            ))
        })),
        // A wait this build cannot read is not a command that failed so much as
        // a session that needs a person. Non-zero would suggest retrying.
        ResumeOutcome::Unresumable { turn_id, reason } => {
            Err(format!("cannot resume turn {turn_id}: {reason}"))
        }
        ResumeOutcome::Settled { turn_id, tool_name, disposition } => {
            let report = scheduler.run_pending(session_id, now).await.map_err(|e| e.to_string())?;
            let settled = disposition_text(&disposition);
            match report {
                Some(r) => {
                    if json {
                        Ok(CliOutcome::ok(CliOutput::Json(json_output(&serde_json::json!({
                            "outcome": "settled",
                            "turn_id": turn_id.as_str(),
                            "tool_name": tool_name,
                            "disposition": settled,
                            "run_id": r.run_id.as_str(),
                            "stop_reason": r.stop_reason,
                            "final_content": r.final_content,
                        }))?)))
                    } else {
                        let mut lines =
                            vec![format!("resumed {turn_id}: {tool_name} was {settled}")];
                        lines.push(match r.final_content {
                            Some(content) => content,
                            None => format!("[turn ended: {:?}]", r.stop_reason),
                        });
                        Ok(CliOutcome::ok(CliOutput::Text(lines.join("\n"))))
                    }
                }
                None => Ok(CliOutcome::ok(CliOutput::Text(format!(
                    "resumed {turn_id}: {tool_name} was {settled}; no continuation ran"
                )))),
            }
        }
    }
}

/// What to tell the user about how the held call ended.
fn disposition_text(disposition: &Disposition) -> String {
    match disposition {
        Disposition::Ran { status } => format!("run ({status:?})"),
        Disposition::Denied { status } => format!("refused ({status:?})"),
        Disposition::Rejected { reason } => format!("refused by the tool gateway: {reason}"),
        Disposition::AlreadySettled => "already settled by an earlier resume".to_string(),
    }
}

/// Execute the `fork` command: derive a new session from a closed turn boundary
/// of this one, leaving this one alone.
///
/// `at` names the boundary turn as its id. A fork that could not be cut reports
/// why as ordinary output rather than as a failure: the reason is always a state
/// a person can act on ("that turn is waiting on a decision"), not a command
/// that went wrong.
///
/// Returns the output and, when a child was made, its id — because the events
/// this command produced are in the *child's* log, and a caller printing the
/// event chain has to know where to look. The source gains nothing.
pub async fn run_fork(
    fork: &Fork,
    source: &SessionId,
    at: Option<&str>,
    reason: &str,
    json: bool,
) -> Result<(CliOutput, Option<SessionId>), String> {
    let at = at.map(TurnId::from_string);
    let outcome = fork.fork(source, at.as_ref(), reason).await.map_err(|e| e.to_string())?;

    match outcome {
        ForkOutcome::NoBoundary { reason } => Ok((
            if json {
                CliOutput::Json(json_output(&serde_json::json!({
                    "outcome": "no_boundary",
                    "reason": reason,
                }))?)
            } else {
                CliOutput::Text(format!("no fork: {reason}"))
            },
            None,
        )),
        ForkOutcome::Forked {
            session_id,
            parent,
            fork_at_event_id,
            fork_at_turn_id,
            inherited_events,
            repaired,
        } => {
            let child = session_id.clone();
            let output = if json {
                let value = serde_json::json!({
                    "outcome": "forked",
                    "session_id": session_id.as_str(),
                    "parent_session_id": parent.as_str(),
                    "fork_at_event_id": fork_at_event_id.as_str(),
                    "fork_at_turn_id": fork_at_turn_id.as_str(),
                    "inherited_event_count": inherited_events,
                    "repair": {
                        "tools_outcome_unknown": repaired.tools_outcome_unknown,
                        "closed_step": repaired.closed_step,
                        "closed_turn": repaired.closed_turn,
                        "closed_run": repaired.closed_run,
                    },
                });
                CliOutput::Json(json_output(&value)?)
            } else {
                let mut line = format!(
                    "forked {session_id} from {parent} at turn {fork_at_turn_id}, \
                     {inherited_events} events inherited"
                );
                if let Some(repaired) = repair_summary(&repaired) {
                    line.push_str(&format!("; closed an interrupted tail: {repaired}"));
                }
                CliOutput::Text(line)
            };
            Ok((output, Some(child)))
        }
    }
}

/// What the child's inherited tail needed closing, in words, or `None` when it
/// was already ended.
fn repair_summary(repaired: &RecoveryReport) -> Option<String> {
    let mut parts = Vec::new();
    if repaired.tools_outcome_unknown > 0 {
        parts.push(format!(
            "{} dispatched call(s) marked outcome-unknown",
            repaired.tools_outcome_unknown
        ));
    }
    if repaired.closed_step {
        parts.push("step".to_string());
    }
    if repaired.closed_turn {
        parts.push("turn".to_string());
    }
    if repaired.closed_run {
        parts.push("run".to_string());
    }
    if parts.is_empty() { None } else { Some(parts.join(", ")) }
}

/// Execute the `status` command: report the session's open state and pending inputs.
pub async fn run_status(
    scheduler: &Scheduler,
    session_id: &SessionId,
    json: bool,
) -> Result<CliOutput, String> {
    let report = commands::status(scheduler, session_id).await.map_err(|e| e.to_string())?;

    let open_run = report.open_run.as_ref().map(|id| id.as_str());
    let open_turn = report.open_turn.as_ref().map(|id| id.as_str());
    let open_step = report.open_step.as_ref().map(|id| id.as_str());

    if json {
        let value = serde_json::json!({
            "session": session_id.as_str(),
            "open_run": open_run,
            "open_turn": open_turn,
            "open_step": open_step,
            "pending": report.pending,
        });
        Ok(CliOutput::Json(json_output(&value)?))
    } else {
        let mut lines = vec![format!("session: {}", session_id.as_str())];
        lines.push(format!("open run:  {}", open_run.unwrap_or("none")));
        lines.push(format!("open turn: {}", open_turn.unwrap_or("none")));
        lines.push(format!("open step: {}", open_step.unwrap_or("none")));
        lines.push(format!("pending:   {}", report.pending));
        Ok(CliOutput::Text(lines.join("\n")))
    }
}

/// Execute the `approve` command: record a grant decision for a pending approval
/// and enqueue a wakeup so the yielded run can be resumed with the decision.
///
/// The `always` scope is the wide form: it grants for the whole session *and*
/// settles every other pending request of the same kind, which is what a person
/// means when they answer a question with "always". A wakeup is enqueued for
/// each approval that was settled, not just the one that was named — each is a
/// decision the paused turn may be waiting on.
pub async fn run_approve(
    approvals: &ApprovalService,
    inbox: &Inbox,
    session_id: &SessionId,
    approval_id: &str,
    scope: &str,
    json: bool,
) -> Result<CliOutput, String> {
    let grant = commands::Grant::from_name(scope).map_err(|e| e.to_string())?;
    let receipt =
        commands::grant_approval(approvals, inbox, session_id, approval_id, grant, "user")
            .await
            .map_err(|e| e.to_string())?;
    let replies = receipt.replies;

    let settled = replies.iter().map(|r| r.approval_id.as_str()).collect::<Vec<_>>();
    let Some(first) = replies.first() else {
        return Err("no approval was settled".to_string());
    };
    if json {
        let value = serde_json::json!({
            "approval_id": first.approval_id.as_str(),
            "decision": first.decision,
            "scope": first.scope,
            "settled": settled,
        });
        Ok(CliOutput::Json(json_output(&value)?))
    } else {
        let mut line = format!("approved {} ({})", first.approval_id, scope_label(first.scope));
        if settled.len() > 1 {
            line.push_str(&format!(
                ", and settled {} other request(s) of the same kind",
                settled.len() - 1
            ));
        }
        Ok(CliOutput::Text(line))
    }
}

/// Execute the `deny` command: record a deny decision for a pending approval and
/// enqueue a wakeup so the yielded run can be resumed with the decision.
///
/// Unless `only` is set, the denial cascades to every other pending approval in
/// the session: a person who says no has stopped trusting what the session is
/// attempting, and the remaining questions would otherwise keep asking them.
pub async fn run_deny(
    approvals: &ApprovalService,
    inbox: &Inbox,
    session_id: &SessionId,
    approval_id: &str,
    only: bool,
    json: bool,
) -> Result<CliOutput, String> {
    let receipt = commands::deny_approval(approvals, inbox, session_id, approval_id, only, "user")
        .await
        .map_err(|e| e.to_string())?;
    let replies = receipt.replies;

    let settled = replies.iter().map(|r| r.approval_id.as_str()).collect::<Vec<_>>();
    let Some(first) = replies.first() else {
        return Err("no approval was settled".to_string());
    };
    if json {
        let value = serde_json::json!({
            "approval_id": first.approval_id.as_str(),
            "decision": first.decision,
            "settled": settled,
        });
        Ok(CliOutput::Json(json_output(&value)?))
    } else {
        let mut line = format!("denied {}", first.approval_id);
        if settled.len() > 1 {
            line.push_str(&format!(", and {} other pending request(s)", settled.len() - 1));
        }
        Ok(CliOutput::Text(line))
    }
}

/// Execute the `steer` command: queue text for the turn already in flight.
///
/// The turn is read from the durable lifecycle rather than from any in-process
/// state, so this works from a different process than the one driving the run.
/// The turn id travels with the text: between reading it here and the scheduler
/// injecting the text, that turn may have ended, and `expected_turn_id` is what
/// keeps a note written for one turn out of its successor. When no turn is open
/// the note is addressed to no one in particular and lands on the next turn's
/// first step.
pub async fn run_steer(
    scheduler: &Scheduler,
    session_id: &SessionId,
    text: &str,
    json: bool,
) -> Result<CliOutput, String> {
    let receipt = commands::steer(scheduler, session_id, text).await.map_err(|e| e.to_string())?;
    let turn = receipt.expected_turn.as_ref().map(|id| id.as_str().to_string());
    if json {
        let value = serde_json::json!({
            "session": session_id.as_str(),
            "item_id": receipt.item_id.as_str(),
            "text": receipt.text,
            "expected_turn_id": turn,
        });
        Ok(CliOutput::Json(json_output(&value)?))
    } else {
        Ok(CliOutput::Text(format!(
            "steered {} (turn: {})",
            session_id.as_str(),
            turn.as_deref().unwrap_or("next")
        )))
    }
}

/// Execute the `interrupt` command: stop the run that is in flight.
///
/// The state check comes first, and it is the whole reason this is not a
/// one-line append. An interrupt item is read by the scheduler at the *top* of
/// the boundary loop — the same place a fresh run reads it for the first time —
/// so an interrupt appended while nothing is running does not sit inert. It
/// kills the next turn before its first step. A command that ran and had no
/// effect is `DidNotRun`, exit `2` (§3 C3.5), and the honest way to tell "no
/// effect" from "stopped the run" is to read the durable lifecycle first.
///
/// Both fields are read because they answer different questions. `open_run` is
/// a run in flight — in this process or in another one, which is what makes
/// this workable from a second terminal. `yielded_turn` is a run that stopped
/// for a person, and it is exactly the thing somebody means to interrupt; it
/// has **no** `open_run` beside it, because `TURN_YIELDED` clears that field.
/// Checking only `open_run` would refuse the pause, which is the one state a
/// person most often wants out of.
///
/// The reason travels as a literal because `Inbox::interrupt` takes one. It is
/// not read back: `Ending::Interrupted` settles on the item id and writes
/// `cause: "user_interrupt"` itself, so nothing on the reading side ever looks
/// at the payload (YA-41 F1). A `--reason` flag would therefore add a field
/// whose only observable effect is to be different, and there is none.
pub async fn run_interrupt(
    scheduler: &Scheduler,
    session_id: &SessionId,
    json: bool,
) -> Result<CliOutcome, String> {
    let outcome = commands::interrupt(scheduler, session_id).await.map_err(|e| e.to_string())?;
    match outcome {
        commands::InterruptOutcome::NothingToInterrupt => Ok(CliOutcome::did_not_run(if json {
            CliOutput::Json(json_output(&serde_json::json!({
                "queued": null,
                "reason": "no_run",
            }))?)
        } else {
            CliOutput::Text(
                "no run to interrupt: no run is in flight and no turn is waiting".to_string(),
            )
        })),
        commands::InterruptOutcome::Queued(item_id) => {
            if json {
                let value = serde_json::json!({
                    "queued": {
                        "item_id": item_id.as_str(),
                        "kind": "interrupt",
                        "applies_at": "the next step boundary",
                    },
                });
                Ok(CliOutcome::ok(CliOutput::Json(json_output(&value)?)))
            } else {
                Ok(CliOutcome::ok(CliOutput::Text(format!(
                    "queued: the run will stop at the next step boundary (item {})\n\
                     this command only asks; the process driving the session applies it",
                    item_id.as_str()
                ))))
            }
        }
    }
}

/// Execute the `tool` command: run a single tool invocation through the guarded
/// gateway (policy + approval + resource boundary + redaction).
pub async fn run_tool(
    gateway: &dyn ToolGateway,
    session_id: &SessionId,
    tool_name: &str,
    arguments: serde_json::Value,
    json: bool,
) -> Result<CliOutcome, String> {
    let invocation = ToolInvocation::proposed(tool_name, arguments);
    let result = gateway.execute(session_id, &invocation).await.map_err(|e| e.to_string())?;
    let output =
        if json { CliOutput::Json(json_output(&result)?) } else { CliOutput::Text(result.content) };
    // A tool that ran and came back bad is not a command that could not run.
    // The result is still the answer and still printed; the verdict is what
    // travels beside it, so the caller can exit on it without reading its own
    // output back. Anything short of a completed run counts: a cancelled call
    // is no more a success to a script than a failed one.
    if result.status == ToolStatus::Completed {
        Ok(CliOutcome::ok(output))
    } else {
        Ok(CliOutcome::failed(output))
    }
}

/// The bundle shape this build writes. A reader can tell two exports apart
/// without guessing from the fields alone.
///
/// This is the *export document's* version, not the event-log schema
/// (`agent_protocol::ids::SchemaVersion::CURRENT`): a bundle reader's contract
/// grows when the bundle's fields change, which is a different occasion from a
/// change to what an event envelope can carry. The two coincide today and are
/// free to move apart.
///
/// `redacted` was added to the bundle without moving this number (YA-41). The
/// version answers "can a 0.1.0 reader read this document?", and the answer is
/// still yes: the new field carries an explicit value rather than changing one,
/// so a reader that ignores it reads the same session it would have read
/// before. A version bump is for a change that would make such a reader wrong.
const EXPORT_SCHEMA_VERSION: &str = "0.1.0";

/// Execute `artifact get`: the reader half of the artifact store.
///
/// A locator is what the log and the model see when output was too large to
/// carry or could not be carried as text. Until a reader can follow one, those
/// bytes exist only inside the store. This is that reader.
///
/// The reference may be the locator exactly as written
/// (`artifact://sha256/<hash>`) or a bare hash; a locator under any other
/// scheme, or one with no hash, is refused before the store is asked. Only the
/// hash is a lookup key — `size` and `mime_type` are written-side metadata a
/// reader holding a locator does not have.
///
/// Without `out` the bytes go to stdout as text. A blob that is not UTF-8 is
/// not dumped into a terminal: it is an answer that says where to put it
/// instead (`--out`), and the verdict is `DidNotRun` because no bytes were
/// delivered. With `--json` and no `out`, the command is refused for the same
/// reason: its answer would be bytes, and bytes are not a JSON document.
///
/// Exit semantics follow C3.1. An artifact that is not there, or whose bytes
/// no longer hash to its name, was *reached*: the answer is a failure
/// (`AnsweredNo`). A reference this build cannot name, an unwritable
/// destination, or a refused output shape never reached one (`DidNotRun`).
pub fn run_artifact_get(
    store: &dyn ArtifactStore,
    reference: &str,
    out: Option<&str>,
    json: bool,
) -> Result<CliOutcome, String> {
    let hash = match ArtifactRef::hash_from_locator(reference) {
        Some(hash) => hash,
        // A locator-shaped string under another scheme is a mistake worth
        // naming as one: passing it through would make the store report it as
        // "not a blob name", which hides the actual problem.
        None if reference.starts_with("artifact://") => {
            return Err(format!("not an artifact locator: {reference}"));
        }
        None => reference,
    };
    let artifact = ArtifactRef { hash: hash.to_string(), mime_type: String::new(), size: 0 };

    let bytes = match store.get(&artifact) {
        Ok(bytes) => bytes,
        Err(ArtifactStoreError::NotFound(_)) => {
            let reason = format!("artifact not found: {}", artifact.locator());
            return Ok(CliOutcome::failed(artifact_failure(json, &artifact, false, &reason)?));
        }
        Err(ArtifactStoreError::ContentMismatch { expected, actual }) => {
            let reason =
                format!("artifact {expected} does not match its content (it hashes to {actual})");
            return Ok(CliOutcome::failed(artifact_failure(json, &artifact, true, &reason)?));
        }
        // The reference never named a blob, or the store could not be read:
        // neither is an answer about an artifact.
        Err(error) => return Err(error.to_string()),
    };

    if let Some(path) = out {
        std::fs::write(path, &bytes).map_err(|e| format!("cannot write {path}: {e}"))?;
        let output = if json {
            CliOutput::Json(json_output(&serde_json::json!({
                "locator": artifact.locator(),
                "hash": artifact.hash,
                "bytes": bytes.len(),
                "out": path,
            }))?)
        } else {
            CliOutput::Text(format!(
                "wrote {} byte(s) to {path} ({})",
                bytes.len(),
                artifact.locator()
            ))
        };
        return Ok(CliOutcome::ok(output));
    }

    if json {
        return Err("--json needs --out: the artifact's bytes are not a JSON document".to_string());
    }

    match String::from_utf8(bytes) {
        Ok(text) => Ok(CliOutcome::ok(CliOutput::Text(text))),
        Err(error) => {
            let bytes = error.into_bytes();
            Ok(CliOutcome::did_not_run(CliOutput::Text(format!(
                "artifact {} is not UTF-8 text ({} bytes); give --out <path> to write it",
                artifact.locator(),
                bytes.len()
            ))))
        }
    }
}

/// The one-object answer a failed fetch gets under `--json`.
///
/// `found` distinguishes "no blob under this name" from "bytes under this name
/// that are not what the name says"; both are answers, and both are failures.
fn artifact_failure(
    json: bool,
    artifact: &ArtifactRef,
    found: bool,
    reason: &str,
) -> Result<CliOutput, String> {
    if !json {
        return Ok(CliOutput::Text(reason.to_string()));
    }
    let value = serde_json::json!({
        "locator": artifact.locator(),
        "hash": artifact.hash,
        "found": found,
        "reason": reason,
    });
    Ok(CliOutput::Json(json_output(&value)?))
}

/// Execute the `export` command: turn a session's log into a document a person
/// reads and a bundle a program reads.
///
/// Artifacts are exported as *references*, never as bytes. [`run_artifact_get`]
/// is the command that fetches one; inlining content here would duplicate a
/// retrieval path and make the bundle depend on blobs the log may outlive.
///
/// With `out_dir` the two halves are written as `session.md` and `bundle.json`;
/// without it they go to stdout (markdown, or the bundle under `--json`). The
/// bundle is a pure function of the log — it carries no export timestamp, so
/// two exports of an unchanged session are byte-identical and a diff means the
/// log changed. The `redacted` flag does not break that: it is an input to the
/// function, and the same input on the same log exports the same bytes.
///
/// `redacted` scrubs the events **once**, before either half is rendered, so
/// the document and the bundle cannot disagree about what the session said.
/// Redacting each half inside its own renderer is the version of this that
/// grows a leak the day somebody adds a third half.
pub async fn run_export(
    store: &dyn EventStore,
    session_id: &SessionId,
    out_dir: Option<&str>,
    redacted: bool,
    json: bool,
) -> Result<CliOutput, String> {
    let events = store
        .read_stream(session_id, EventSeq::ZERO, usize::MAX)
        .await
        .map_err(|e| e.to_string())?;
    let events = if redacted { redact_events(&events) } else { events };
    let artifacts = collect_artifacts(&events);
    let markdown = render_export_markdown(session_id, &events, &artifacts, redacted);
    let bundle = export_bundle(session_id, &events, &artifacts, redacted);

    let Some(dir) = out_dir else {
        return Ok(if json {
            CliOutput::Json(json_output(&bundle)?)
        } else {
            CliOutput::Text(markdown)
        });
    };

    let dir = std::path::Path::new(dir);
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let markdown_path = dir.join("session.md");
    let bundle_path = dir.join("bundle.json");
    std::fs::write(&markdown_path, &markdown)
        .map_err(|e| format!("cannot write {}: {e}", markdown_path.display()))?;
    std::fs::write(&bundle_path, json_output(&bundle)?.as_str())
        .map_err(|e| format!("cannot write {}: {e}", bundle_path.display()))?;

    if json {
        let value = serde_json::json!({
            "session": session_id.as_str(),
            "redacted": redacted,
            "event_count": events.len(),
            "artifact_count": artifacts.len(),
            "markdown": markdown_path.display().to_string(),
            "bundle": bundle_path.display().to_string(),
        });
        Ok(CliOutput::Json(json_output(&value)?))
    } else {
        Ok(CliOutput::Text(format!(
            "exported {} event(s) and {} artifact reference(s) to {} (session.md, bundle.json){}",
            events.len(),
            artifacts.len(),
            dir.display(),
            if redacted { ", secret-shaped tokens replaced with ***" } else { "" }
        )))
    }
}

/// Every artifact the session's log refers to, in first-seen order.
///
/// Deduplicated by locator: the same blob offloaded twice is one artifact, and
/// listing it twice would overstate how much the session produced.
fn collect_artifacts(events: &[StoredEvent]) -> Vec<serde_json::Value> {
    let mut seen = std::collections::HashSet::new();
    let mut artifacts = Vec::new();
    for stored in events {
        let Some(refs) = stored.event.payload.get("artifact_refs").and_then(|v| v.as_array())
        else {
            continue;
        };
        for reference in refs {
            let Some(locator) = reference.get("locator").and_then(|v| v.as_str()) else {
                continue;
            };
            if seen.insert(locator.to_string()) {
                artifacts.push(reference.clone());
            }
        }
    }
    artifacts
}

/// The same events, with every string leaf of every payload and metadata block
/// passed through the redactor.
///
/// Both halves of the draft are scrubbed, not just the payload. Metadata is
/// written by whichever side recorded the event and is free to carry the same
/// text as the payload; a scrub that leaves one of two adjacent copies of a
/// secret in the document is not a scrub. Shape is preserved
/// ([`Redactor::redact_value`]), so what a program reads keeps the same fields
/// and the same types — only the bytes that were secret change.
///
/// The envelope is deliberately left alone. Ids, timestamps, the actor, and the
/// event type are the record's structure rather than its content, and rewriting
/// them would produce a different session's log rather than a quieter copy of
/// this one.
///
/// This is the same backstop the kernel applies on the way in
/// (`agent-kernel/src/guarded.rs`), and it has the same limits: it matches
/// known token prefixes, so a secret that does not look like one passes
/// through. It is worth running anyway, because the durable log is *not* the
/// only place user text lands — an inbox item's payload is written raw, which
/// is what makes a session where somebody pasted a token into a prompt a
/// session whose log holds that token.
fn redact_events(events: &[StoredEvent]) -> Vec<StoredEvent> {
    let redactor = Redactor::new();
    events
        .iter()
        .map(|stored| {
            let mut scrubbed = stored.clone();
            let (payload, _) = redactor.redact_value(&stored.event.payload);
            let (metadata, _) = redactor.redact_value(&stored.event.metadata);
            scrubbed.event.payload = payload;
            scrubbed.event.metadata = metadata;
            scrubbed
        })
        .collect()
}

/// The machine-readable half: the events as recorded, plus the artifact index.
///
/// `redacted` is always recorded, in both directions. A document that says
/// nothing about whether it was scrubbed is a document a reader has to guess
/// about, and the guess that matters — "the token is absent, so it was
/// redacted" — is unavailable to them precisely when the log never held a
/// token at all.
///
/// The document, not its rendering: both readers (the file `bundle.json` and a
/// `--json` answer) go through [`json_output`], so they cannot disagree about
/// the shape of what they write.
fn export_bundle(
    session_id: &SessionId,
    events: &[StoredEvent],
    artifacts: &[serde_json::Value],
    redacted: bool,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": EXPORT_SCHEMA_VERSION,
        "redacted": redacted,
        "session": session_id.as_str(),
        "event_count": events.len(),
        "artifacts": artifacts,
        "events": events,
    })
}

/// The human-readable half: the events grouped by turn, one line each.
///
/// The redaction notice appears only when the document was actually scrubbed.
/// Printing it unconditionally would put a claim on every export, including the
/// ones nothing was removed from, and a reader who learns to skip a line that
/// is always there learns to skip it on the export where it meant something.
/// The notice says what the filter is *not*, because a redacted export invites
/// exactly one false conclusion — that what is left is safe to paste.
fn render_export_markdown(
    session_id: &SessionId,
    events: &[StoredEvent],
    artifacts: &[serde_json::Value],
    redacted: bool,
) -> String {
    let mut out = format!(
        "# Session {}\n\n{} event(s) from the session log.\n",
        session_id.as_str(),
        events.len()
    );
    if redacted {
        out.push_str(
            "\nRedacted: secret-shaped tokens in event payloads and metadata were replaced with \
             `***`.\nThat is a prefix-matching backstop, not a guarantee — text that does not look \
             like a known token was not touched.\n",
        );
    }

    let mut current: Option<Option<String>> = None;
    for stored in events {
        let turn = stored.event.turn_id.as_ref().map(|t| t.as_str().to_string());
        if current.as_ref() != Some(&turn) {
            out.push('\n');
            match &turn {
                Some(id) => out.push_str(&format!("## Turn {id}\n\n")),
                None => out.push_str("## Session-level\n\n"),
            }
            current = Some(turn);
        }
        out.push_str(&format!(
            "- `{}` **{}** {}\n",
            stored.seq.as_u64(),
            stored.event.event_type.as_str(),
            event_summary(stored)
        ));
    }

    out.push_str("\n## Artifacts\n\n");
    if artifacts.is_empty() {
        out.push_str("None.\n");
    } else {
        for artifact in artifacts {
            out.push_str(&format!(
                "- {} ({} bytes, {})\n",
                artifact.get("locator").and_then(|v| v.as_str()).unwrap_or("?"),
                artifact.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
                artifact.get("mime").and_then(|v| v.as_str()).unwrap_or("?")
            ));
        }
    }
    out
}

/// A short line for one event.
///
/// Types this renderer knows get a summary built from their own fields. Anything
/// else falls back to the payload itself, bounded: the log is the record, and
/// inventing a summary for an event this code does not understand would be the
/// document lying about what happened. An empty payload renders as nothing
/// rather than as `{}`.
fn event_summary(stored: &StoredEvent) -> String {
    let payload = &stored.event.payload;
    let text = |key: &str| payload.get(key).and_then(|v| v.as_str()).map(str::to_string);

    match stored.event.event_type.as_str() {
        EventType::TOOL_STARTED => match text("tool_name") {
            Some(name) => format!(
                "{name} {}",
                compact(payload.get("arguments").unwrap_or(&serde_json::Value::Null), 120)
            ),
            None => compact(payload, 160),
        },
        EventType::TOOL_COMPLETED | EventType::TOOL_FAILED | EventType::TOOL_CANCELLED => {
            match (text("tool_name"), text("status")) {
                (Some(name), Some(status)) => format!("{name} ({status})"),
                (Some(name), None) => name,
                _ => compact(payload, 160),
            }
        }
        EventType::TOOL_RESULT_ARTIFACT_OFFLOADED => match text("tool_name") {
            Some(name) => format!("{name} -> {}", locators(payload)),
            None => locators(payload),
        },
        EventType::APPROVAL_REQUESTED => match text("tool_name") {
            Some(name) => format!("{name} is asking for a decision"),
            None => compact(payload, 160),
        },
        EventType::APPROVAL_GRANTED | EventType::APPROVAL_DENIED => {
            match (text("decision"), text("scope")) {
                (Some(decision), Some(scope)) => format!("{decision} ({scope})"),
                (Some(decision), None) => decision,
                _ => compact(payload, 160),
            }
        }
        EventType::INBOX_APPENDED => {
            let kind = text("kind").unwrap_or_default();
            match payload.get("payload").and_then(|p| p.get("text")).and_then(|t| t.as_str()) {
                Some(body) => format!("{kind}: {body}"),
                None => kind,
            }
        }
        _ => compact(payload, 160),
    }
}

/// The artifact locators named by an event's payload, in order.
fn locators(payload: &serde_json::Value) -> String {
    payload
        .get("artifact_refs")
        .and_then(|v| v.as_array())
        .map(|refs| {
            refs.iter()
                .filter_map(|r| r.get("locator").and_then(|l| l.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// A payload on one bounded line. Long content is cut, not paraphrased — a
/// marker saying what was cut is honest; a plausible summary is not.
fn compact(value: &serde_json::Value, limit: usize) -> String {
    let text = value.to_string();
    if text == "{}" || text == "null" {
        return String::new();
    }
    if text.chars().count() <= limit {
        return text;
    }
    let end = text.char_indices().nth(limit).map(|(i, _)| i).unwrap_or(text.len());
    format!("{}… ({} bytes total)", &text[..end], text.len())
}

fn scope_label(scope: ApprovalScope) -> &'static str {
    match scope {
        ApprovalScope::AllowOnce => "once",
        ApprovalScope::AllowForTurn => "turn",
        ApprovalScope::AllowForSession => "session",
    }
}
