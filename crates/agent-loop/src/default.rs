//! DefaultAgentLoop: model -> tools -> model ... -> final answer.
//!
//! Each iteration is one Step: a model request, its attempt lifecycle, any tool
//! invocations, then step completion. A step ends for four reasons and only
//! four: `FinalAnswer` when the model returns no tool calls, `NeedsUserInput`
//! when a call is waiting for a person, and `FatalError` or `UserCancelled` when
//! the attempt cannot go on.
//!
//! It never ends on `MaxSteps`. The step budget belongs to whoever advances the
//! turn, not to the loop that runs one step of it: a step that has spent the
//! budget is still a completed step reporting `stop: None`, and saying why the
//! turn stopped is the Scheduler's to do. An interrupt at a step boundary
//! arrives from there too, which is why one `UserCancelled` covers both paths.
//! The end of `step` carries the rest of the reason — a loop that stopped itself
//! could not be steered past the step it just finished.
//!
//! Model attempts are settled explicitly: `request.prepared` and
//! `attempt.started` precede the call, and exactly one of `response.completed`,
//! `attempt.failed`, or `attempt.cancelled` follows it. A failed attempt returns
//! a turn outcome carrying those events — it never drops them on the floor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_ports::agent_loop::{
    AgentLoop, AgentLoopError, SteeringInput, StepOutcome, StopReason, TurnCapabilityFacts,
    TurnContext, TurnEnding, TurnInputKind, TurnStart, TurnState, TurnStateFacts,
};
use agent_ports::context_engine::{
    CompactionCandidate, CompactionScope, ContextEngine, ContextUsage,
};
use agent_ports::decision::DecisionAdvisor;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::live_event_bus::LiveEventBus;
use agent_ports::model_provider::{
    ContentBlock, MessageRole, ModelBudget, ModelCapability, ModelMessage, ModelProvider,
    ModelRequest, ModelResponse, ModelStreamError, ModelStreamEvent, ModelStreamSink, ToolManifest,
};
use agent_ports::model_router::{ModelRoute, ModelRouter, RouteRequest, TaskClass};
use agent_ports::tool_gateway::{ToolGateway, ToolGatewayError};
use agent_ports::tool_provider::{ToolInvocation, ToolResult, ToolStatus};
use agent_protocol::digest;
use agent_protocol::events::{Actor, EventDraft, EventType, LiveEvent};
use agent_protocol::failures::Failures;
use agent_protocol::ids::{AttemptId, EventId, EventSeq, SchemaVersion, SessionId, StepId};
use agent_protocol::judgment::{self, Judgment, JudgmentStage};
use agent_protocol::ledger::Ledger;
use agent_protocol::model::{ToolCall, estimate_message, estimate_text};
use agent_protocol::permissions::ArtifactRef;
use agent_protocol::plan::{self, Plan};
use agent_protocol::redactor::Redactor;
use agent_protocol::tool::{
    APPROVAL_PENDING_OUTCOME, GATE_REFUSED_OUTCOME, GATEWAY_REFUSED_OUTCOME, ToolSideEffect,
};
use agent_protocol::usertext::UserText;
use chrono::{SecondsFormat, Utc};

/// Longest provider error text persisted in a durable attempt event.
const MAX_ERROR_CHARS: usize = 500;

/// The message layout version recorded on every prepared request.
///
/// Version 1 placed the per-turn layers (state, task, notice) before history.
/// Version 2 places them after settled history and before this turn's input,
/// so the cached prefix is `instructions + capability + settled history` and
/// rewriting a layer mid-turn cannot invalidate it. The number is in the event
/// so a replay can tell which layout a request was assembled under.
const LAYOUT_VERSION: u32 = 2;

/// How much of this turn's input the task layer quotes.
///
/// The layer is a header, not a copy of the conversation: the user message
/// that follows already carries the whole text, and a layer that repeated an
/// unbounded input would double its cost on every step.
const MAX_TASK_CHARS: usize = 2000;

/// How much of a tool description the capability index repeats.
///
/// One line per tool: the index says what exists, and the full description in
/// the tool definition says how to use it.
const MAX_CAPABILITY_GIST_CHARS: usize = 100;

/// The longest handover summary kept from one compaction attempt.
///
/// A summary is already lossy; a runaway answer must not silently become the
/// new largest message. The event records the text as truncated, so the cap is
/// visible rather than a claim about what the model wrote.
const MAX_SUMMARY_CHARS: usize = 4000;

/// What the compaction request asks for. Fixed text, versioned in the event so
/// a reader knows which instruction produced a summary.
///
/// The user's own words are treated differently from routine chatter: a
/// requirement, constraint, or decision is quoted; an acknowledgement or a
/// repeated restatement is condensed. The goal is a summary in which every
/// line is still information the next turn needs.
const COMPACTION_PROMPT: &str = "You maintain the running handover summary of a conversation so it can continue after older messages are removed from the model's context. A deterministic ledger of the tool calls and their results is kept separately and does not need repeating: do not restate or paraphrase individual tool calls. A separate record also keeps the user's own messages verbatim: do not re-quote them, and do not claim what an earlier user message said unless the conversation below shows it. Write one compact summary of the conversation below. Keep: the goal and current state of the work, decisions made and why, facts that matter (names, paths, values), what was tried and failed in a sentence, and what remains. Preserve the user's requirements, constraints, and decisions word for word; condense routine chatter, acknowledgements, and repeated restatements. Be factual and specific; do not invent anything and do not give advice. Do not compute or infer values: keep a number only where the conversation shows it, and treat the latest user message as the current request rather than an earlier one. Reply with the summary text only.";

/// The user turn that closes a compaction request.
///
/// A system prompt alone is not enough. Candidate history can end on an
/// assistant tool call, and a chat model then continues that turn instead of
/// writing a summary. Real use on 2026-10-19 produced a raw tool-call dump as
/// the "summary": the run continued because the files were still on disk, but
/// the handover it was supposed to preserve was lost. A final user turn is what
/// makes the request an instruction rather than a continuation.
const COMPACTION_INSTRUCTION: &str = "Write the handover summary of the conversation above now. The tool calls and their results are already recorded in the ledger, and the user's own messages are kept verbatim in a separate record, so do not repeat either. Keep the goal and current state, decisions, the user's requirements and constraints verbatim, what failed in a sentence, and what remains. Do not continue the conversation and do not call tools. Reply with the summary text only.";

/// How far this turn's own traffic may grow before the loop compacts it once.
///
/// The engine only ever sees settled history, so a turn that outgrows the
/// window through its own tool results has no protection from it. Twice the
/// usable budget is the point where the next request is already larger than
/// the configured window and the trimmer (which also only sees the settled
/// derivation) cannot help.
const IN_FLIGHT_CEILING_MULTIPLIER: usize = 2;

/// How many mid-turn compactions one turn may attempt.
///
/// One attempt is enough while the newest group fits the retention target: it
/// clears every older group. It is not enough when the newest group alone is
/// larger than the target, because that group is always kept — only a later
/// step, after the model has acted on it, makes it eligible. The second
/// attempt is gated on new traffic (the newest group must have changed), so
/// this is a cost ceiling, not a licence to summarize every step.
const MAX_IN_FLIGHT_COMPACTIONS: u32 = 2;

/// A ledger this turn's compaction wrote, recorded on later prepared requests.
///
/// The ledger itself is a message in the request and stays readable in the
/// `context/compacted` event. This record points at it from the request side:
/// which message carried it, under which rendering version, and a hash that
/// proves the text the request saw is the text the event recorded.
struct LedgerRef {
    message_uid: String,
    view_version: String,
    hash: String,
    lines: usize,
    elided: usize,
}

impl LedgerRef {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "message_uid": self.message_uid,
            "view_version": self.view_version,
            "hash": self.hash,
            "lines": self.lines,
            "elided": self.elided,
        })
    }
}

/// The verbatim record of the user's words this turn's compaction wrote.
///
/// Like [`LedgerRef`] it points at the message in the request rather than
/// repeating it, and records a hash of exactly the text the request sent.
struct UserTextRef {
    message_uid: String,
    view_version: String,
    hash: String,
    messages: usize,
    elided: usize,
}

impl UserTextRef {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "message_uid": self.message_uid,
            "view_version": self.view_version,
            "hash": self.hash,
            "messages": self.messages,
            "elided": self.elided,
        })
    }
}

/// The failures record this turn's compaction wrote.
///
/// Same terms as [`LedgerRef`] and [`UserTextRef`]: the message that carries
/// it and a hash of exactly the text the request sent.
struct FailuresRef {
    message_uid: String,
    view_version: String,
    hash: String,
    entries: usize,
    elided: usize,
}

impl FailuresRef {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "message_uid": self.message_uid,
            "view_version": self.view_version,
            "hash": self.hash,
            "entries": self.entries,
            "elided": self.elided,
        })
    }
}

/// This turn's own step traffic, proposed for one of its mid-turn compactions.
///
/// The range starts after this turn's input and covers every assistant/tool
/// message since; user text (the input, any steering note) is never proposed
/// and stays verbatim. `None` means the turn is under the ceiling, there is
/// nothing older than the newest group to summarize, or no window is
/// configured — in which case the turn keeps the behavior it had before
/// in-flight compaction existed.
fn in_flight_candidate(turn: &LoopTurn) -> Option<CompactionCandidate> {
    let max_input = turn.context_usage.max_input_tokens?;
    let budget =
        max_input.saturating_sub(turn.context_usage.reserved_output_tokens.unwrap_or(0)) as usize;
    if budget == 0 {
        return None;
    }
    let input_uid = format!("msg/user/{}", turn.ctx.turn_id);
    let input =
        turn.messages.iter().position(|message| message.message_uid.as_str() == input_uid)?;
    let traffic = &turn.messages[input + 1..];
    let traffic_tokens: usize = traffic.iter().map(estimate_message).sum();
    if traffic_tokens < budget.saturating_mul(IN_FLIGHT_CEILING_MULTIPLIER) {
        return None;
    }

    let groups = agent_protocol::compaction::groups(traffic);
    let keep_target =
        budget.saturating_mul(agent_protocol::compaction::COMPACTION_KEEP_PERCENT as usize) / 100;
    let (keep_from, retained) =
        agent_protocol::compaction::keep_from(traffic, &groups, 0, keep_target)?;

    let candidate: Vec<ModelMessage> = groups[..keep_from]
        .iter()
        .filter(|group| {
            group.keep
                && !traffic[group.start..=group.end]
                    .iter()
                    .any(|message| message.role == MessageRole::User)
        })
        .flat_map(|group| traffic[group.start..=group.end].iter().cloned())
        .collect();
    if candidate.is_empty() {
        return None;
    }
    Some(CompactionCandidate {
        estimated_tokens: candidate.iter().map(estimate_message).sum(),
        messages: candidate,
        retained_tokens: retained,
        budget_tokens: budget,
        observed_input_tokens: None,
        scope: CompactionScope::InFlight,
    })
}

/// The default turn-advancement strategy.
pub struct DefaultAgentLoop {
    model: Arc<dyn ModelProvider>,
    gateway: Arc<dyn ToolGateway>,
    /// Where the model's history comes from. The loop never keeps a transcript
    /// of its own: what a turn sends is the derived history plus the messages
    /// this turn produced, and the turn's own messages enter the history when
    /// the Scheduler appends them.
    context: Arc<dyn ContextEngine>,
    /// Where the model choice comes from. Required, like the context
    /// engine: a loop that silently picked a model itself would be a routing
    /// policy nothing declared.
    router: Arc<dyn ModelRouter>,
    /// Optional shadow decisions. Absent means no decision call is made at all,
    /// which is the honest way to say "this session runs no fast-decision
    /// backend" — rather than a broker that is present and asked to do nothing.
    decisions: Option<Arc<dyn DecisionAdvisor>>,
    /// Optional live-event sink. Without a UI attached the loop still runs and
    /// simply publishes deltas nowhere.
    live_bus: Option<Arc<dyn LiveEventBus>>,
    /// Monotonic across turns so a subscriber can order live events.
    live_seq: Arc<AtomicU64>,
    /// Whether the judgment ledger and its failure gate are part of this loop.
    ///
    /// On by default. The switch exists for the evaluation that has to show
    /// the gate changes behavior: the same fixtures are run with it off to get
    /// a baseline on the same build, rather than compared against a different
    /// one.
    judgment_enabled: bool,
}

impl DefaultAgentLoop {
    pub fn new(
        model: Arc<dyn ModelProvider>,
        gateway: Arc<dyn ToolGateway>,
        context: Arc<dyn ContextEngine>,
        router: Arc<dyn ModelRouter>,
    ) -> Self {
        Self {
            model,
            gateway,
            context,
            router,
            decisions: None,
            live_bus: None,
            live_seq: Arc::new(AtomicU64::new(0)),
            judgment_enabled: true,
        }
    }

    /// Turn the judgment ledger and the failure gate off.
    ///
    /// This is the baseline arm of the evaluation, not a user preference: with
    /// the ledger off the tool is not offered, no record is written, and the
    /// gate cannot fire. Everything else about the turn is unchanged.
    pub fn without_judgment(mut self) -> Self {
        self.judgment_enabled = false;
        self
    }

    /// Publish model stream deltas to a live event bus.
    pub fn with_live_bus(mut self, bus: Arc<dyn LiveEventBus>) -> Self {
        self.live_bus = Some(bus);
        self
    }

    /// Run fast decisions in shadow alongside each turn.
    ///
    /// The advisor's answer is recorded and never used: the route set on the
    /// requests is the router's, and nothing else in the turn reads the trace.
    pub fn with_decisions(mut self, advisor: Arc<dyn DecisionAdvisor>) -> Self {
        self.decisions = Some(advisor);
        self
    }

    /// Whether the model can be asked to call tools at all.
    ///
    /// A provider that does not declare `tool_call` is never offered a tool.
    /// That is the documented degrade path: drop a capability the
    /// provider does not have rather than fail closed on every turn. The
    /// request event records what was actually required, so the difference is
    /// visible in the log rather than silent.
    fn offers_tools(&self) -> bool {
        self.model.capabilities().iter().any(|c| c.as_str() == ModelCapability::TOOL_CALL)
    }

    /// A sink for this turn's deltas: the live bus when one is attached, a
    /// no-op otherwise.
    fn sink(&self, ctx: &TurnContext) -> Box<dyn ModelStreamSink> {
        match &self.live_bus {
            Some(bus) => Box::new(LiveBusSink {
                bus: bus.clone(),
                session_id: ctx.session_id.clone(),
                live_seq: self.live_seq.clone(),
            }),
            None => Box::new(NullSink),
        }
    }

    /// Ask the model for a handover summary and rewrite this turn's history
    /// with it.
    ///
    /// The summary request is not part of the conversation: it goes through the
    /// same provider but with no tools, no capabilities, and no live deltas.
    /// Both the request (by reference) and the answer (in full) are recorded
    /// around it, so the compaction is auditable and a failed one is a fact in
    /// the log rather than a silent fallback.
    ///
    /// A failure changes nothing: the turn sends the history it was going to
    /// send, and the trimmer remains the only protection.
    async fn compact(
        &self,
        turn: &mut LoopTurn,
        step_id: &StepId,
        candidate: CompactionCandidate,
        events: &mut Vec<EventDraft>,
    ) -> Result<(), AgentLoopError> {
        let ctx = turn.ctx.clone();
        let attempt_id = AttemptId::new();

        let mut refs = Vec::with_capacity(candidate.messages.len());
        for message in &candidate.messages {
            refs.push(serde_json::json!({
                "message_uid": message.message_uid.as_str(),
                "role": message.role.as_str(),
                "hash": digest::json(&serde_json::to_value(message)?),
            }));
        }
        let summarized_hash = digest::json(&serde_json::Value::Array(refs.clone()));

        // An in-flight candidate is this turn's own traffic, and user text is
        // never part of it — the input stays verbatim in the request that
        // continues. Without it the summarizer sees steps that begin mid-turn
        // and writes a summary of a conversation with no stated goal (live,
        // 2026-10-20: "No task or goal has been stated by the user yet"). The
        // input is given as context only: it is not summarized and not removed.
        let input_message = if candidate.scope == CompactionScope::InFlight {
            let uid = format!("msg/user/{}", ctx.turn_id);
            turn.messages.iter().find(|message| message.message_uid.as_str() == uid).cloned()
        } else {
            None
        };
        let input_uid =
            input_message.as_ref().map(|message| message.message_uid.as_str().to_string());

        events.push(draft(
            &ctx,
            step_id,
            EventType::CONTEXT_COMPACTION_STARTED,
            serde_json::json!({
                "attempt_id": attempt_id,
                "provider": self.model.provider_id(),
                "route": serde_json::to_value(&turn.route)?,
                "context_revision": turn.context_revision.as_u64(),
                "prompt_version": "summary/5",
                "prompt": COMPACTION_PROMPT,
                "instruction": COMPACTION_INSTRUCTION,
                "message_count": refs.len(),
                "messages": refs,
                "messages_hash": summarized_hash,
                "estimated_tokens": candidate.estimated_tokens,
                "retained_tokens": candidate.retained_tokens,
                "budget_tokens": candidate.budget_tokens,
                // Which measure proposed this compaction: the provider's own
                // input count when it reported one, `null` when the estimate
                // was the only measure there was.
                "observed_input_tokens": candidate.observed_input_tokens,
                "scope": candidate.scope.as_str(),
                // The input shown to the summarizer as context but not
                // summarized: an in-flight proposal never covers user text.
                "context_message_uid": input_uid,
            }),
            Actor::Agent,
        ));

        let mut request_messages = Vec::with_capacity(candidate.messages.len() + 3);
        request_messages.push(ModelMessage::new(
            MessageRole::System,
            vec![ContentBlock::text(COMPACTION_PROMPT)],
        ));
        request_messages.extend(input_message);
        request_messages.extend(candidate.messages.iter().cloned());
        request_messages.push(ModelMessage::new(
            MessageRole::User,
            vec![ContentBlock::text(COMPACTION_INSTRUCTION)],
        ));
        let request = ModelRequest {
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.clone(),
            step_id: step_id.clone(),
            context_revision: turn.context_revision.as_u64(),
            messages: request_messages,
            capabilities: Vec::new(),
            response_format: None,
            model_route: Some(turn.route.clone()),
            budget: ModelBudget::default(),
            tool_manifest: ToolManifest::empty(),
        };

        let response = match self.model.stream(&request, &NullSink).await {
            Ok(response) => response,
            Err(error) => {
                events.push(draft(
                    &ctx,
                    step_id,
                    EventType::CONTEXT_COMPACTION_FAILED,
                    serde_json::json!({
                        "attempt_id": attempt_id,
                        "error_kind": error.kind(),
                        "message": truncate(&error.to_string(), MAX_ERROR_CHARS),
                    }),
                    Actor::Agent,
                ));
                return Ok(());
            }
        };

        // The summary is model-authored text headed for the durable log and
        // every later request, so it is scrubbed like tool output — and the
        // message the next step sends is the same scrubbed text the log holds.
        let (text, redaction) = Redactor::new().redact(&response.text_content());
        if text.trim().is_empty() {
            events.push(draft(
                &ctx,
                step_id,
                EventType::CONTEXT_COMPACTION_FAILED,
                serde_json::json!({
                    "attempt_id": attempt_id,
                    "error_kind": "empty_summary",
                    "message": "the provider returned no summary text",
                }),
                Actor::Agent,
            ));
            return Ok(());
        }
        // The steps the summary replaces are recorded from the messages
        // themselves as a deterministic ledger beside it. The ledger is not
        // another model answer: the same messages under the same view version
        // render the same lines, and a call cannot appear in it that did not
        // run. Its files and failures are exactly the facts a summary is known
        // to drop, so the two no longer compete for the same text.
        let ledger = Ledger::build(&candidate.messages, &turn.write_tools);
        let ledger_text = ledger.render();
        let truncated = text.chars().count() > MAX_SUMMARY_CHARS;
        let summary = truncate(&text, MAX_SUMMARY_CHARS);

        // The user's own words are recorded verbatim beside the summary and
        // the ledger. A model-written summary dropped the standing constraints
        // the second time it was summarized and quoted another document as the
        // user's first message (real use, 2026-10-21); the record is built from
        // the transcript, so it cannot drop them or invent a source.
        let usertext = UserText::build(&candidate.messages);
        let usertext_text = usertext.render();

        // Failures get their own record for the same reason, and one more: the
        // ledger carries only its tail into the next compaction, so the oldest
        // failure — the baseline a later question asks about — is the first
        // thing lost. This record is carried whole.
        let failures = Failures::build(&candidate.messages);
        let failures_text = failures.render();

        // The transcript loses exactly the messages the event names, and the
        // summary takes the place of the earliest of them: at the head of
        // settled history for a turn-start compaction, inside this turn's own
        // traffic for an in-flight one. The fold finds the same first removed
        // message when it replays the event, so both sides put it in one place.
        // A candidate message already trimmed out of this turn is not in
        // `messages`; it is still in the log and still named, so the fold
        // removes it there too.
        let uids: Vec<&str> =
            candidate.messages.iter().map(|message| message.message_uid.as_str()).collect();
        let insert_at = turn
            .messages
            .iter()
            .position(|message| uids.contains(&message.message_uid.as_str()))
            .unwrap_or_else(|| match candidate.scope {
                CompactionScope::Settled => turn.history_start.min(turn.messages.len()),
                CompactionScope::InFlight => turn.messages.len(),
            });
        let counts_as_history =
            |index: usize| index >= turn.history_start && index < turn.history_end;
        let removed_from_history = turn
            .messages
            .iter()
            .enumerate()
            .filter(|(index, message)| {
                counts_as_history(*index) && uids.contains(&message.message_uid.as_str())
            })
            .count();
        let before = turn.messages.len();
        turn.messages.retain(|message| !uids.contains(&message.message_uid.as_str()));
        let removed = before - turn.messages.len();
        let summary_message = ModelMessage::summary_for(&attempt_id, summary.clone());
        turn.messages.insert(insert_at, summary_message);
        // The ledger, the user's-words record and the failures record follow the
        // summary at the same position: all four stand in for the removed
        // messages. A later compaction finds each of them among its candidates
        // and carries it forward, so an earlier rewrite's steps, the user's own
        // words and the failures stay auditable after a second rewrite.
        let mut inserted = 1;
        if !ledger.is_empty() {
            turn.messages.insert(
                insert_at + inserted,
                ModelMessage::ledger_for(&attempt_id, ledger_text.clone()),
            );
            turn.ledger = Some(LedgerRef {
                message_uid: format!("msg/ledger/{attempt_id}"),
                view_version: agent_protocol::ledger::LEDGER_VIEW_VERSION.to_string(),
                hash: digest::json(&serde_json::Value::String(ledger_text.clone())),
                lines: ledger.lines.len(),
                elided: ledger.elided,
            });
            inserted += 1;
        }
        if !usertext.is_empty() {
            turn.messages.insert(
                insert_at + inserted,
                ModelMessage::usertext_for(&attempt_id, usertext_text.clone()),
            );
            turn.usertext = Some(UserTextRef {
                message_uid: format!("msg/usertext/{attempt_id}"),
                view_version: agent_protocol::usertext::USER_TEXT_VIEW_VERSION.to_string(),
                hash: digest::json(&serde_json::Value::String(usertext_text.clone())),
                messages: usertext.entries().len(),
                elided: usertext.elided(),
            });
            inserted += 1;
        }
        if !failures.is_empty() {
            turn.messages.insert(
                insert_at + inserted,
                ModelMessage::failures_for(&attempt_id, failures_text.clone()),
            );
            turn.failures = Some(FailuresRef {
                message_uid: format!("msg/failures/{attempt_id}"),
                view_version: agent_protocol::failures::FAILURE_VIEW_VERSION.to_string(),
                hash: digest::json(&serde_json::Value::String(failures_text.clone())),
                entries: failures.entries().len(),
                elided: failures.elided,
            });
            inserted += 1;
        }
        if candidate.scope == CompactionScope::Settled {
            // The old range length is the derivation's history without its
            // preamble, which is not compacted. `context_message_count` counts
            // the derivation including that preamble, so the two are adjusted
            // separately rather than by a shared subtraction. In-flight
            // compaction never touches this range, and neither number moves.
            let previous_history = turn.history_end.saturating_sub(turn.history_start);
            let retained = previous_history.saturating_sub(removed_from_history);
            turn.history_end = turn.history_start + retained + inserted;
            turn.context_message_count =
                turn.context_message_count.saturating_sub(previous_history) + retained + inserted;
        }

        let mut payload = serde_json::json!({
            "attempt_id": attempt_id,
            "summary_message_uid": format!("msg/summary/{attempt_id}"),
            "summary": summary,
            "summarized": refs,
            "summarized_hash": summarized_hash,
            "context_revision": turn.context_revision.as_u64(),
            "message_count": candidate.messages.len(),
            "removed_messages": removed,
            "estimated_tokens": candidate.estimated_tokens,
            "retained_tokens": candidate.retained_tokens,
            "scope": candidate.scope.as_str(),
            "observed_input_tokens": candidate.observed_input_tokens,
            "truncated": truncated,
        });
        if !ledger.is_empty() {
            payload["ledger"] = serde_json::json!({
                "message_uid": format!("msg/ledger/{attempt_id}"),
                "view_version": agent_protocol::ledger::LEDGER_VIEW_VERSION,
                "text": ledger_text,
                "lines": ledger.lines.len(),
                "elided": ledger.elided,
                "files": ledger.files,
            });
        }
        if !usertext.is_empty() {
            payload["usertext"] = serde_json::json!({
                "message_uid": format!("msg/usertext/{attempt_id}"),
                "view_version": agent_protocol::usertext::USER_TEXT_VIEW_VERSION,
                "text": usertext_text,
                "messages": usertext.entries().len(),
                "carried": usertext.carried(),
                "elided": usertext.elided(),
                "considered": usertext.considered().iter().map(|item| serde_json::json!({
                    "message_uid": item.message_uid,
                    "score": item.score,
                    "kept": item.kept,
                })).collect::<Vec<_>>(),
            });
        }
        if !failures.is_empty() {
            payload["failures"] = serde_json::json!({
                "message_uid": format!("msg/failures/{attempt_id}"),
                "view_version": agent_protocol::failures::FAILURE_VIEW_VERSION,
                "text": failures_text,
                "entries": failures.entries().len(),
                "carried": failures.carried(),
                "elided": failures.elided,
            });
        }
        if redaction.redacted {
            payload["redaction"] = serde_json::json!({
                "redacted": true,
                "patterns": redaction.patterns,
            });
        }
        events.push(draft(&ctx, step_id, EventType::CONTEXT_COMPACTED, payload, Actor::Agent));
        Ok(())
    }
}

/// Publishes model deltas as live events. Nothing here ever becomes durable.
struct LiveBusSink {
    bus: Arc<dyn LiveEventBus>,
    session_id: SessionId,
    live_seq: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl ModelStreamSink for LiveBusSink {
    async fn on_event(&self, event: ModelStreamEvent) -> Result<(), ModelStreamError> {
        let live = LiveEvent {
            live_seq: self.live_seq.fetch_add(1, Ordering::Relaxed) + 1,
            event_type: EventType::new(event.live_event_type()),
            session_id: self.session_id.clone(),
            created_at: Utc::now(),
            payload: serde_json::to_value(&event).unwrap_or(serde_json::Value::Null),
            metadata: serde_json::json!({}),
        };
        self.bus.publish(live).await.map_err(|_| ModelStreamError::Closed)
    }
}

/// Drops deltas when no live bus is attached.
struct NullSink;

#[async_trait::async_trait]
impl ModelStreamSink for NullSink {
    async fn on_event(&self, _event: ModelStreamEvent) -> Result<(), ModelStreamError> {
        Ok(())
    }
}

/// What one turn of this loop carries between its steps.
///
/// It lives here and not in the kernel: the Scheduler decides when a turn
/// advances, and it has no reason to read the conversation, the route, or the
/// manifest it carries.
struct LoopTurn {
    ctx: TurnContext,
    messages: Vec<ModelMessage>,
    capabilities: Vec<ModelCapability>,
    manifest: ToolManifest,
    context_revision: EventSeq,
    /// How many settled messages the derivation handed over, before this turn's
    /// own input was appended.
    context_message_count: usize,
    /// What the derivation kept and dropped, recorded on every prepared request
    /// so a trim can be read back from the log.
    context_usage: ContextUsage,
    /// The reminder about the window this turn's request carries, if any.
    context_notice: Option<ContextNotice>,
    /// The text of the per-turn state and task layers, if this turn has the
    /// facts to place them. Recorded on every prepared request so the two
    /// messages the model saw can be read back without re-deriving the log.
    context_layers: Option<ContextLayers>,
    /// The run-scoped capability layer, when the caller supplied facts. It is
    /// recorded on prepared requests for the same reason as the per-turn
    /// layers, and it is not history.
    context_capability: Option<String>,
    /// The deterministic ledger this turn's compaction wrote, if it wrote one.
    /// Recorded on later prepared requests so a request can be matched to the
    /// directory of steps it carried without re-deriving the log.
    ledger: Option<LedgerRef>,
    /// The verbatim record of the user's words this turn's compaction wrote.
    /// Recorded for the same reason as the ledger: the request must be
    /// matchable to the text it sent, not to a re-derivation of it.
    usertext: Option<UserTextRef>,
    /// The failures this turn's compaction recorded, on the same terms.
    failures: Option<FailuresRef>,
    /// Where the derived history sits in `messages`: after the stable head
    /// (instructions and capability) and before the per-turn layers. Compaction
    /// rewrites exactly this range.
    history_start: usize,
    /// One past the last derived message. Kept for the same reason as
    /// `history_start`, and updated when compaction removes messages.
    history_end: usize,
    /// The engine's proposal to summarize older history, taken by the first
    /// step that acts on it. `None` means this turn has nothing to compact —
    /// either no trigger was configured, it was not reached, or it was already
    /// done this turn.
    compaction: Option<CompactionCandidate>,
    /// Tools whose declared side effect is `Write`, as of this turn's frozen
    /// manifest. Rehydration reads it to tell "this call changed a file" from
    /// "this call only read one".
    write_tools: Vec<String>,
    /// Tools whose declared side effect is `Read`. The task layer only asks
    /// for a look when the turn offers a way to look; advice the model has no
    /// call to satisfy is noise, not a reminder.
    read_tools: Vec<String>,
    /// Tools that change the world and cannot be retried safely (`Write`,
    /// `Network` or `Destructive`, and not idempotent). The task layer names
    /// them before the model's first call, so the two ways and the way back
    /// are stated while the choice is still open.
    irreversible_tools: Vec<String>,
    /// The signature (tool and arguments) of the last call that ran, and how
    /// many times in a row it has run unchanged. The repeat gate fires on the
    /// third, not the second: the second may be the one retry a transient
    /// failure deserves.
    last_signature: Option<String>,
    identical_streak: u32,
    /// Whether the repeat gate already refused once for this streak.
    repeat_refused: bool,
    /// How many mid-turn compactions this turn has attempted. A failed attempt
    /// counts (see `MAX_IN_FLIGHT_COMPACTIONS`), so this is a record of spent
    /// attempts, not of successes.
    in_flight_compactions: u32,
    /// Which message was newest when the last mid-turn attempt was made. A
    /// further attempt waits until new traffic has arrived — the model has
    /// acted since the group that was too large — so the group it could not
    /// summarize on the first pass is summarized only after it has been used.
    in_flight_newest: Option<String>,
    /// The judgment record this turn is building on, as of the turn's opening.
    /// Every accepted `judgment.update` replaces it with the merged record, so
    /// the task layer the next step reads is the one the log now holds.
    judgment: Option<Judgment>,
    /// The most recent real failure the record has not reconciled yet. Approval
    /// pending is a wait, not a failure, and never lands here.
    pending_failure: Option<PendingFailure>,
    /// The failure call the gate already blocked on. One block per failure:
    /// the gate is a reminder to think, not a loop that can hang a turn.
    gate_fired_for: Option<String>,
    /// A reconcile said the next thing is a retry rather than a stop. It is
    /// cleared when a real (non-ledger) tool call is made; a text answer
    /// before that is the prose-only retry this stage exists to catch.
    pending_retry: Option<String>,
    /// Whether the turn already refused one answer for a missing retry. One
    /// refusal, same as the failure gate: the second answer is taken.
    retry_blocked: bool,
    /// Whether a real tool call has looked at the world since the pending
    /// failure. A stop is a decision made after a check; a stop recorded
    /// without one is the same "gave up without looking" the gate exists to
    /// catch, so the loop refuses that reconcile until a call has been made.
    checked_since_failure: bool,
    /// Whether that refusal has already been made in this turn. One refusal,
    /// like the gate: a model that stops again has been told and is taken.
    stop_refused: bool,
    /// A read an earlier turn made that this turn's own words name again. The
    /// answer may restate that reading, which is a fact about then.
    stale: Option<StaleRead>,
    /// Whether the turn already refused one answer for restating it.
    stale_gate_fired: bool,
    /// Whether any real tool call has been made in this turn. A turn that has
    /// looked at the world is not answering from an earlier turn alone.
    acted_this_turn: bool,
    route: ModelRoute,
}

/// A call that failed for a reason the next step could act on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFailure {
    call_id: String,
    tool: String,
    detail: String,
}

/// A read-only call an earlier turn made whose target this turn names again.
///
/// The match is mechanical — the exact argument string appears in the turn's
/// own words — because the kernel has no reading of meaning. It only has to
/// know that a fact now in the transcript was observed before this turn began.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleRead {
    tool: String,
    target: String,
}

/// What the task layer says about the tools this turn offers.
///
/// These two sentences used to be gates: the step was held until the model
/// recorded a branch or looked at its write. Measured on the fixtures, a
/// refusal cost two model calls per firing and changed no outcome, so the
/// requirement became advice — the same words, in a message every request in
/// the turn already carries. Advice cannot be enforced; the kernel only makes
/// sure it was said before the choice was made.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolAdvice {
    irreversible: Vec<String>,
    writes: Vec<String>,
    can_look: bool,
}

impl ToolAdvice {
    fn new(irreversible: &[String], writes: &[String], reads: &[String]) -> Self {
        Self {
            irreversible: irreversible.to_vec(),
            writes: writes.to_vec(),
            can_look: !reads.is_empty(),
        }
    }

    /// The text appended to the task layer, or nothing when the turn offers
    /// no call it would apply to.
    fn text(&self) -> String {
        let mut text = String::new();
        if !self.irreversible.is_empty() {
            text.push_str(&format!(
                "\nBefore a call that cannot be undone (`{}`): use judgment.update to record \
                 the two ways you could take and the fallback you retreat to (stage `branch`), \
                 then make the call.",
                self.irreversible.join("`, `")
            ));
        }
        if !self.writes.is_empty() && self.can_look {
            text.push_str(
                "\nAfter a write, read the result back or run the check that reads it before \
                 telling the user it is done.",
            );
        }
        text
    }
}

/// The advice this turn's task layer carries, from its frozen tool sets.
fn tool_advice(turn: &LoopTurn) -> ToolAdvice {
    ToolAdvice::new(&turn.irreversible_tools, &turn.write_tools, &turn.read_tools)
}

/// The two per-turn leading messages: where the turn stands, and what it is
/// for. Neither is history.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextLayers {
    state: String,
    task: String,
}

impl ContextLayers {
    fn to_json(&self, turn_id: &agent_protocol::ids::TurnId) -> serde_json::Value {
        serde_json::json!({
            "state": {
                "message_uid": format!("msg/state/{turn_id}"),
                "text": self.state,
            },
            "task": {
                "message_uid": format!("msg/task/{turn_id}"),
                "text": self.task,
            },
        })
    }
}

/// The state layer's text: the facts around this turn that a transcript does
/// not carry on its own.
fn state_text(facts: &TurnStateFacts) -> String {
    let queued = match facts.queued_inputs {
        0 => String::from("none"),
        1 => String::from("1"),
        count => count.to_string(),
    };
    let previous = match facts.previous_turn {
        None => "none",
        Some(TurnEnding::Completed) => "completed",
        Some(TurnEnding::Failed) => "failed",
        Some(TurnEnding::Cancelled) => "cancelled by the user",
        Some(TurnEnding::Interrupted) => "interrupted",
        Some(TurnEnding::Yielded) => "yielded (waiting on a decision)",
    };
    format!(
        "State at {}.\nQueued input(s): {queued}.\nPrevious turn: {previous}.",
        facts.started_at.to_rfc3339_opts(SecondsFormat::Secs, true)
    )
}

/// The task layer's text: what this turn is for, how it began, the run's step
/// ceiling, and the session's plan. The input is quoted and bounded; the user
/// message after the layers still carries it whole.
///
/// A plan that replaces the previous one changes this text, so the task
/// message is rewritten in place when the ledger moves — the layer the next
/// step sends is the one the model just wrote.
fn task_text(
    input: &serde_json::Value,
    facts: &TurnStateFacts,
    plan: Option<&Plan>,
    judgment: Option<&Judgment>,
    stale: Option<&StaleRead>,
    advice: Option<&ToolAdvice>,
) -> String {
    let task = truncate(&user_text(input), MAX_TASK_CHARS);
    let input_kind = match facts.input_kind {
        TurnInputKind::User => "user turn",
        TurnInputKind::Scheduled => "scheduled turn",
    };
    let steps = match facts.max_steps {
        1 => String::from("1 model step"),
        count => format!("{count} model steps"),
    };
    let mut text = format!(
        "Task for this turn: \"{task}\"\nInput: {input_kind}. This run allows at most {steps}."
    );
    // Time, stated where the model reads its task: an earlier turn's answer
    // is a fact about when it was read, not about now. When the kernel can
    // name the read this turn's words point at, it says that instead of the
    // general rule — a named fact is cheaper to act on than a maxim — and it
    // only speaks from the second turn on.
    if let Some(stale) = stale {
        text.push_str(&format!(
            "\n`{}` was read in an earlier turn, not in this one. Read it again before \
             answering: the world may have moved since.",
            stale.target
        ));
    } else if facts.previous_turn.is_some() {
        text.push_str(
            "\nThe world may have moved since the earlier turns: what was read then was true \
             then. Re-read anything the answer depends on before answering.",
        );
    }
    if let Some(plan) = plan {
        text.push('\n');
        text.push_str(&plan.render());
    }
    if let Some(judgment) = judgment {
        text.push('\n');
        text.push_str(&judgment.render());
    }
    if let Some(advice) = advice {
        text.push_str(&advice.text());
    }
    text
}

/// The read an earlier turn made that this turn's words name again.
///
/// Only `Read` tools count: a write's arguments are not observations of the
/// world, and a command's are not a fact the kernel can say was seen. The
/// latest match wins, because that is the reading an answer would reuse.
fn stale_read(history: &[ModelMessage], input: &str, read_tools: &[String]) -> Option<StaleRead> {
    let mut stale = None;
    for message in history {
        for block in &message.content {
            let ContentBlock::ToolUse { tool_name, arguments, .. } = block else { continue };
            if !read_tools.iter().any(|read| read == tool_name) {
                continue;
            }
            for target in argument_strings(arguments) {
                if target.chars().count() < 3 || !input.contains(target) {
                    continue;
                }
                stale = Some(StaleRead { tool: tool_name.clone(), target: target.to_string() });
            }
        }
    }
    stale
}

/// Every string inside an argument value, at any depth.
fn argument_strings(value: &serde_json::Value) -> Vec<&str> {
    match value {
        serde_json::Value::String(text) => vec![text.as_str()],
        serde_json::Value::Array(items) => items.iter().flat_map(argument_strings).collect(),
        serde_json::Value::Object(map) => map.values().flat_map(argument_strings).collect(),
        _ => Vec::new(),
    }
}

/// The capability layer's text: what this run's boundary allows, and an index
/// of the tools it offers.
///
/// The index is generated from the same manifest that goes on the wire, so it
/// cannot drift from what the provider was told is callable. Full argument
/// schemas stay in the tool definitions; this is the map, not the manual.
fn capability_text(facts: &TurnCapabilityFacts, manifest: &ToolManifest) -> String {
    let sandbox = match facts.sandbox {
        SandboxMode::ReadOnly => "read-only (no writes, no command execution)",
        SandboxMode::LocalProcess => "local (commands run on the host, confined to the workspace)",
        SandboxMode::WorkspaceSandbox => {
            "app-container (commands run under an OS confinement, confined to the workspace)"
        }
        SandboxMode::Container => {
            "container (commands run inside a container, workspace mounted into it)"
        }
    };
    let network = if facts.network_allowed { "allowed" } else { "denied" };
    let mut text = format!(
        "Capabilities in this run:\n\
         - sandbox: {sandbox}\n\
         - network for sandboxed commands: {network}\n"
    );
    if manifest.is_empty() {
        text.push_str("- no tools are available in this run.");
        return text;
    }
    text.push_str(&format!(
        "- {} tools are available; full arguments are in the tool definitions:\n",
        manifest.entries.len()
    ));
    for entry in &manifest.entries {
        text.push_str(&format!("  {} — {}\n", entry.tool_id, gist(&entry.description)));
    }
    text.pop();
    text
}

/// The first sentence of a description, collapsed to one line and bounded.
///
/// Descriptions are written for the full schema, where a second sentence is
/// affordable. The index only has to say what the tool is for; the detail is a
/// tool definition away.
fn gist(description: &str) -> String {
    let collapsed = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = collapsed.split_once(". ").map_or(collapsed.as_str(), |(sentence, _)| sentence);
    truncate(first, MAX_CAPABILITY_GIST_CHARS)
}

/// How full the configured input budget is before the model is told.
///
/// A percentage rather than a token count: the window is configured per run,
/// and at 85% one more large tool result can exhaust what is left.
const CONTEXT_NOTICE_PERCENT: u64 = 85;

/// A per-request reminder about what the configured window did.
///
/// It is not history: the notice lives in this turn's request only, and the
/// same notice is sent on every step of the turn. `model/request.prepared`
/// records it, so the sentence the model saw can be read back without
/// re-deriving the log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextNotice {
    kind: ContextNoticeKind,
    dropped_messages: usize,
    estimated_tokens: usize,
    max_input_tokens: u32,
    percent: u32,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextNoticeKind {
    Trimmed,
    NearLimit,
    TrimmedNearLimit,
}

impl ContextNoticeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trimmed => "trimmed",
            Self::NearLimit => "near-limit",
            Self::TrimmedNearLimit => "trimmed+near-limit",
        }
    }
}

impl ContextNotice {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.as_str(),
            "dropped_messages": self.dropped_messages,
            "estimated_tokens": self.estimated_tokens,
            "max_input_tokens": self.max_input_tokens,
            "percent": self.percent,
            "text": self.text,
        })
    }
}

/// Build the notice this turn's request deserves, if any.
///
/// No configured window means the derivation had nothing to drop and no
/// ceiling to be near, so the answer is `None` and the request is what it was
/// before notices existed. The percentage is measured against the budget
/// history may use, not the whole window the answer also draws from.
///
/// It is an estimate of the settled context only: this turn's own tool results
/// accumulate after `begin_turn`, and the loop does not trim mid-turn
/// (`CONTRACT.md` §7 #29). "About" is deliberate.
fn context_notice(usage: &ContextUsage) -> Option<ContextNotice> {
    let max_input_tokens = usage.max_input_tokens?;
    let budget =
        max_input_tokens.saturating_sub(usage.reserved_output_tokens.unwrap_or(0)) as usize;
    if budget == 0 {
        return None;
    }
    let percent = usage.estimated_tokens.saturating_mul(100) / budget;
    let percent = u32::try_from(percent).unwrap_or(u32::MAX);
    let trimmed = usage.dropped_messages > 0;
    let near = u64::from(percent) >= CONTEXT_NOTICE_PERCENT;
    let kind = match (trimmed, near) {
        (false, false) => return None,
        (true, false) => ContextNoticeKind::Trimmed,
        (false, true) => ContextNoticeKind::NearLimit,
        (true, true) => ContextNoticeKind::TrimmedNearLimit,
    };
    let dropped = usage.dropped_messages;
    let left = budget.saturating_sub(usage.estimated_tokens);
    let text = match kind {
        ContextNoticeKind::Trimmed => format!(
            "Context notice: {dropped} older message(s) were omitted to fit the configured \
             input budget. If an earlier fact matters, re-read the file or search instead of \
             assuming the omitted content is unchanged."
        ),
        ContextNoticeKind::NearLimit => format!(
            "Context notice: the settled context is using about {percent}% of the \
             {budget}-token input budget ({left} tokens left). Keep the next steps focused and \
             prefer targeted reads over broad dumps."
        ),
        ContextNoticeKind::TrimmedNearLimit => format!(
            "Context notice: {dropped} older message(s) were omitted to fit the configured \
             input budget, and the settled context is using about {percent}% of the \
             {budget}-token input budget ({left} tokens left). Re-read a file or search if an \
             earlier fact matters; keep the next steps focused."
        ),
    };
    Some(ContextNotice {
        kind,
        dropped_messages: dropped,
        estimated_tokens: usage.estimated_tokens,
        max_input_tokens,
        percent,
        text,
    })
}

#[async_trait::async_trait]
impl AgentLoop for DefaultAgentLoop {
    async fn begin_turn(&self, ctx: &TurnContext) -> Result<TurnStart, AgentLoopError> {
        // Derived once per turn, not once per step: the settled prefix cannot
        // change while this turn is in flight, since the Scheduler appends this
        // turn's events only after the turn ends.
        let derived = self.context.derive(&ctx.session_id).await?;
        let context_revision = derived.revision;
        let context_message_count = derived.messages.len();
        let context_usage = derived.usage;
        // The engine's proposal travels with the derivation. Nothing is
        // compacted here: `begin_turn` makes no model request, and the
        // proposal is only worth acting on if the first step is about to send
        // a request.
        let compaction = derived.compaction;

        // The run's instructions lead the request. They are durable for the
        // same reason this turn's input is: the Scheduler resolved them and is
        // about to record them in `run/started`, which lands only after this
        // call returns. The uid is derived from the run, so a fold that reads
        // the log later rebuilds this exact message.
        //
        // A derivation can still carry the previous run's message; it is
        // replaced rather than followed, because a request has one system
        // message and it is the one this run recorded. `None` leaves the
        // derived history alone — a harness with no source, or a session whose
        // log was written before runs carried instructions.
        let mut derived_messages = derived.messages;
        if let Some(instructions) = &ctx.instructions {
            let message = ModelMessage::system_for_run(&ctx.run_id, instructions.text.clone());
            match derived_messages.first_mut() {
                // A summary (or the ledger beside it) is history even though
                // its role is `system`; the instructions replace the run
                // preamble, never a compaction product.
                Some(first)
                    if first.role == MessageRole::System && !first.is_compaction_output() =>
                {
                    *first = message;
                }
                _ => derived_messages.insert(0, message),
            }
        }

        // The tool set for this turn, frozen before the first request. Building
        // it once keeps every step's manifest identical — a manifest id that
        // changed between steps of one turn would be noise, not information.
        // It is built here because the capability layer's tool index is
        // generated from it.
        //
        // `plan.update` is the loop's own tool and no provider implements it,
        // but it still reaches the model through this manifest. A gateway that
        // somehow offers the name is made to lose it: the loop intercepts the
        // call before dispatch, so the provider's definition would only
        // describe a tool that can never run.
        let (manifest, write_tools, read_tools, irreversible_tools) = if self.offers_tools() {
            // What the gateway offers this model, not everything it could
            // execute: a tool every possible call would be refused is not a
            // choice, and a manifest that lists it is one more entry to read.
            let mut definitions = self.gateway.model_definitions();
            definitions.retain(|definition| {
                definition.name != plan::TOOL_NAME && definition.name != judgment::TOOL_NAME
            });
            // Recorded beside the manifest because the manifest itself keeps
            // only what reaches the wire; rehydration needs the declared side
            // effect, which does not.
            let write_tools: Vec<String> = definitions
                .iter()
                .filter(|definition| definition.side_effect == ToolSideEffect::Write)
                .map(|definition| definition.name.clone())
                .collect();
            // The observations a later turn may restate. Read-only tools are
            // the only calls whose arguments are a fact about the world rather
            // than a change to it.
            let read_tools: Vec<String> = definitions
                .iter()
                .filter(|definition| definition.side_effect == ToolSideEffect::Read)
                .map(|definition| definition.name.clone())
                .collect();
            // A call with no way back: it changes the world (a file, a remote
            // service, a process) and a repeat is not safe. `idempotent` alone
            // is not enough — a write that can be replayed may still overwrite
            // what was there.
            let irreversible_tools: Vec<String> = definitions
                .iter()
                .filter(|definition| {
                    !definition.idempotent
                        && matches!(
                            definition.side_effect,
                            ToolSideEffect::Write
                                | ToolSideEffect::Network
                                | ToolSideEffect::Destructive
                        )
                })
                .map(|definition| definition.name.clone())
                .collect();
            definitions.push(plan::tool_definition());
            // The judgment tool is offered on the same terms as the plan: the
            // loop answers it itself, so a provider that claimed the name would
            // only describe a tool that can never run.
            if self.judgment_enabled {
                definitions.push(judgment::tool_definition());
            }
            (
                ToolManifest::from_definitions(&definitions),
                write_tools,
                read_tools,
                irreversible_tools,
            )
        } else {
            (ToolManifest::empty(), Vec::new(), Vec::new(), Vec::new())
        };
        let advice = ToolAdvice::new(&irreversible_tools, &write_tools, &read_tools);

        // The derivation is [run preamble, ...history]. A summary or ledger is
        // history even though it is a system message, so the split stops at
        // it; a log written before runs carried instructions may have no
        // preamble.
        let preamble_len = derived_messages
            .iter()
            .take_while(|message| {
                message.role == MessageRole::System && !message.is_compaction_output()
            })
            .count();
        let mut messages: Vec<ModelMessage> = derived_messages[..preamble_len].to_vec();

        // What this run can do, before the model tries something the boundary
        // will refuse. Run-scoped: the same uid on every turn, so a cached
        // prompt can reuse it. Not history — the projection never folds it.
        //
        // Position matters for cost: instructions and capability form the
        // stable head of every request in the run, so they sit before history
        // and never move. The per-turn layers below are volatile and go after
        // history instead, where rewriting them cannot break the cached prefix.
        let context_capability = ctx.capability.as_ref().map(|facts| {
            let text = capability_text(facts, &manifest);
            messages.push(ModelMessage::capability_for_run(&ctx.run_id, text.clone()));
            text
        });

        // Settled history, exactly. The range is the derivation minus its
        // preamble: code that rewrites history (compaction) inserts at
        // `history_start` and never touches the two stable head messages.
        let history_start = messages.len();
        messages.extend(derived_messages[preamble_len..].iter().cloned());
        let history_end = messages.len();

        // Time, read off the log: a read an earlier turn made that this turn's
        // own words name again. The history is the settled prefix only, so a
        // call this turn makes later is never mistaken for an old one.
        let stale = stale_read(&derived_messages, &user_text(&ctx.user_input), &read_tools);

        // The world around this turn, then what this turn is for. Both are
        // properties of the turn, not replies in the conversation: they sit
        // after settled history and before this turn's input, so every request
        // in this turn ends with the same live frame. They are not durable
        // history — the projection never folds them — and a caller with no
        // facts places neither layer rather than inventing a time or an empty
        // queue.
        let context_layers = ctx.state.as_ref().map(|facts| {
            let layers = ContextLayers {
                state: state_text(facts),
                task: task_text(
                    &ctx.user_input,
                    facts,
                    ctx.plan.as_ref(),
                    if self.judgment_enabled { ctx.judgment.as_ref() } else { None },
                    stale.as_ref(),
                    if self.judgment_enabled { Some(&advice) } else { None },
                ),
            };
            messages.push(ModelMessage::state_for_turn(&ctx.turn_id, layers.state.clone()));
            messages.push(ModelMessage::task_for_turn(&ctx.turn_id, layers.task.clone()));
            layers
        });

        // A standing condition of this turn, not a reply to anything. It is
        // volatile like the layers above, so it goes with them, immediately
        // before this turn's input. It is not history — the projection never
        // folds it — and the prepared request records it.
        let context_notice = context_notice(&context_usage);
        if let Some(notice) = &context_notice {
            messages.push(ModelMessage::context_notice_for_turn(&ctx.turn_id, notice.text.clone()));
        }

        // Then this turn's input. It is durable too — the Scheduler read it out
        // of the `inbox/appended` event it is holding — but this turn's
        // `turn/started` does not exist yet, so the loop is the one that places
        // it.
        messages.push(ModelMessage::user_for_turn(&ctx.turn_id, user_text(&ctx.user_input)));

        let capabilities = required_capabilities(&manifest);

        // Routing happens once per turn, before the first request. A tier that
        // changed mid-turn would make `context_revision` and the route it was
        // chosen from impossible to reconcile, and there is nothing a step can
        // learn that the turn's shape did not already say.
        let route_request = RouteRequest {
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.clone(),
            // Every turn the Scheduler can start today is a user turn. When a
            // caller can ask for summarization or review, the class arrives
            // with the turn rather than being guessed here.
            task: TaskClass::new(TaskClass::INTERACTIVE),
            context_revision,
            message_count: messages.len(),
            required_capabilities: capabilities.clone(),
            budget: ModelBudget::default(),
        };
        let route = self.router.route(&route_request);

        let mut events: Vec<EventDraft> = Vec::new();

        // What this turn offered, in full. The prepared requests below carry
        // the manifest id and count; only this event carries the definitions
        // themselves, so a request can be read back exactly after the process
        // that assembled it is gone. Once per turn, not once per step: the set
        // is frozen above and cannot change while the turn runs.
        let frozen_entries = serde_json::to_value(&manifest.entries)?;
        events.push(draft_turn(
            ctx,
            EventType::MODEL_TOOLS_FROZEN,
            serde_json::json!({
                "manifest_id": &manifest.manifest_id,
                "created_at": manifest.created_at,
                "entries_hash": digest::json(&frozen_entries),
                "tool_count": manifest.entries.len(),
                "entries": frozen_entries,
            }),
            Actor::Agent,
        ));

        if let Some(advisor) = &self.decisions {
            // The trace is recorded as an observation. The answer is not read:
            // in shadow mode a disagreement between the advisor and the router
            // is visible in this event, and nowhere else.
            let trace = advisor.advise_model_route(&route_request, &route).await;
            events.push(draft_turn(
                ctx,
                EventType::DECISION_TRACE,
                serde_json::json!({ "trace": trace }),
                Actor::Agent,
            ));
        }

        Ok(TurnStart {
            state: TurnState::new(LoopTurn {
                ctx: ctx.clone(),
                messages,
                capabilities,
                manifest,
                context_revision,
                context_message_count,
                context_usage,
                context_notice,
                context_layers,
                context_capability,
                ledger: None,
                usertext: None,
                failures: None,
                history_start,
                history_end,
                compaction,
                write_tools,
                read_tools,
                irreversible_tools,
                last_signature: None,
                identical_streak: 0,
                repeat_refused: false,
                in_flight_compactions: 0,
                in_flight_newest: None,
                judgment: ctx.judgment.clone(),
                // A resumed turn opens on a failure the previous turn left
                // behind; the caller read it back from the log. An ordinary
                // turn opens clean.
                pending_failure: ctx.pending_failure.as_ref().map(|pending| PendingFailure {
                    call_id: pending.call_id.clone(),
                    tool: pending.tool.clone(),
                    detail: pending.detail.clone(),
                }),
                gate_fired_for: None,
                pending_retry: None,
                retry_blocked: false,
                checked_since_failure: false,
                stop_refused: false,
                stale,
                stale_gate_fired: false,
                acted_this_turn: false,
                route,
            }),
            events,
        })
    }

    async fn step(
        &self,
        turn: &mut TurnState,
        steering: &[SteeringInput],
    ) -> Result<StepOutcome, AgentLoopError> {
        let turn = turn.get_mut::<LoopTurn>().ok_or(AgentLoopError::StateNotOwned)?;
        // A copy rather than a borrow: compaction rewrites `turn.messages`
        // mid-step, and holding a borrow of `turn.ctx` would make the whole
        // turn unavailable for that. The fields are small and read once.
        let ctx = turn.ctx.clone();
        let ctx = &ctx;
        let context_revision = turn.context_revision;
        let mut events: Vec<EventDraft> = Vec::new();

        // Steering goes in before anything else this step does: it was injected
        // at the boundary this step begins at, so it precedes the request that
        // will carry it — and the step/started below.
        for input in steering {
            turn.messages.push(ModelMessage::user_for_steering(
                &ctx.turn_id,
                &input.item_id,
                input.text.clone(),
            ));
        }

        let step_id = StepId::new();
        {
            events.push(draft(
                ctx,
                &step_id,
                EventType::STEP_STARTED,
                serde_json::json!({}),
                Actor::Agent,
            ));

            // A compaction proposal is acted on before this step's own request,
            // so the request the model sees is already the smaller one. It is
            // taken, not read: one turn summarizes settled history at most
            // once.
            if let Some(candidate) = turn.compaction.take() {
                self.compact(turn, &step_id, candidate, &mut events).await?;
            }

            // The turn's own traffic has no other protection: the engine
            // compacts settled history only, and the trimmer does too. Once a
            // turn has carried more than the ceiling in its own steps, compact
            // it — at most `MAX_IN_FLIGHT_COMPACTIONS` times, and a later
            // attempt only once the newest group has changed. The first
            // attempt cannot summarize the group that crossed the ceiling; it
            // is the newest one and is always kept. The second attempt, after
            // the model has acted on it, is what turns that group into a
            // summary. A failed attempt counts: retrying the same messages
            // every step would spend a summary request per step without
            // changing anything.
            let newest = turn.messages.last().map(|message| message.message_uid.to_string());
            if turn.in_flight_compactions < MAX_IN_FLIGHT_COMPACTIONS
                && newest.as_deref() != turn.in_flight_newest.as_deref()
                && let Some(candidate) = in_flight_candidate(turn)
            {
                turn.in_flight_compactions += 1;
                turn.in_flight_newest = newest;
                self.compact(turn, &step_id, candidate, &mut events).await?;
            }

            let request = ModelRequest {
                session_id: ctx.session_id.clone(),
                turn_id: ctx.turn_id.clone(),
                step_id: step_id.clone(),
                // `ModelRequest` is a serialized event payload and still
                // carries the raw number; the routing request carries the
                // domain type.
                context_revision: context_revision.as_u64(),
                messages: turn.messages.clone(),
                capabilities: turn.capabilities.clone(),
                response_format: None,
                model_route: Some(turn.route.clone()),
                budget: ModelBudget::default(),
                tool_manifest: turn.manifest.clone(),
            };

            // The exact ordered messages sent, as references. Their content is
            // durable elsewhere already — run instructions, this turn's layers,
            // the input item, the folded history, every tool result — and
            // repeating it on each step would make the log grow with the whole
            // context on every step. The hash is what makes a reference
            // checkable: a read-back can prove the message it found is the one
            // this request carried.
            let mut message_refs = Vec::with_capacity(request.messages.len());
            for message in &request.messages {
                message_refs.push(serde_json::json!({
                    "message_uid": message.message_uid.as_str(),
                    "role": message.role.as_str(),
                    "hash": digest::json(&serde_json::to_value(message)?),
                }));
            }
            let messages_hash = digest::json(&serde_json::Value::Array(message_refs.clone()));

            // What this request costs, on the same estimate the context engine
            // uses. `estimated_context_tokens` above describes only the
            // settled derivation; these cover the whole request: the stable
            // head, every layer, this turn's input, any in-flight tool traffic,
            // and the tool definitions sent beside the messages. The provider's
            // own count is recorded with the response, so the two can be
            // compared without re-deriving anything.
            let estimated_messages_tokens: usize =
                request.messages.iter().map(estimate_message).sum();
            // The wire shape, not the manifest entry: `registration_version`,
            // `source`, and `schema_hash` never reach the provider, and counting
            // them inflated this estimate by roughly 12% (2026-10-19).
            let estimated_tool_tokens: usize = request
                .tool_manifest
                .entries
                .iter()
                .map(|entry| {
                    let wire = serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": entry.tool_id,
                            "description": entry.description.trim(),
                            "parameters": entry.input_schema,
                        },
                    });
                    estimate_text(&wire.to_string())
                })
                .sum();
            let estimated_request_tokens = estimated_messages_tokens + estimated_tool_tokens;

            let attempt_id = AttemptId::new();
            let mut prepared = serde_json::json!({
                "attempt_id": attempt_id,
                "context_revision": request.context_revision,
                // `message_count` is what this request actually sends;
                // the context keys describe the derivation behind it, so a
                // trimmed history can be read back without re-deriving.
                "message_count": request.messages.len(),
                "context_message_count": turn.context_message_count,
                "dropped_message_count": turn.context_usage.dropped_messages,
                "layout_version": LAYOUT_VERSION,
                "estimated_context_tokens": turn.context_usage.estimated_tokens,
                "estimated_messages_tokens": estimated_messages_tokens,
                "estimated_tool_tokens": estimated_tool_tokens,
                "estimated_request_tokens": estimated_request_tokens,
                "max_input_tokens": turn.context_usage.max_input_tokens,
                "reserved_output_tokens": turn.context_usage.reserved_output_tokens,
                "messages": message_refs,
                "messages_hash": messages_hash,
                "tool_manifest_id": request.tool_manifest.manifest_id,
                "tool_count": request.tool_manifest.entries.len(),
                "required_capabilities": request
                    .capabilities
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>(),
            });
            // Absent, not null, when no reminder was placed: a request with no
            // notice has exactly the payload it had before notices existed.
            if let Some(notice) = &turn.context_notice {
                prepared["context_notice"] = notice.to_json();
            }
            // Absent, not null, when the caller had no facts: a harness's
            // request keeps exactly the payload it had before layers existed.
            if let Some(layers) = &turn.context_layers {
                prepared["context_layers"] = layers.to_json(&ctx.turn_id);
            }
            // The run-scoped capability layer, recorded the same way so
            // `context inspect` can read back what the model was told about
            // the boundary without deriving it again.
            if let Some(text) = &turn.context_capability {
                prepared["context_capability"] = serde_json::json!({
                    "message_uid": format!("msg/capability/{}", ctx.run_id),
                    "text": text,
                });
            }
            // The ledger this turn's compaction wrote, if any: which message
            // carries it and a hash of exactly the text this request sent, so
            // inspect can point at the directory without re-deriving it.
            if let Some(ledger) = &turn.ledger {
                prepared["context_ledger"] = ledger.to_json();
            }
            // The user's-words record this turn's compaction wrote, on the same
            // terms: which message carries it, and a hash of the exact text.
            if let Some(usertext) = &turn.usertext {
                prepared["context_usertext"] = usertext.to_json();
            }
            if let Some(failures) = &turn.failures {
                prepared["context_failures"] = failures.to_json();
            }
            events.push(draft(
                ctx,
                &step_id,
                EventType::MODEL_REQUEST_PREPARED,
                prepared,
                Actor::Agent,
            ));

            let declared = self.model.capabilities();
            // Fail closed rather than let a provider silently drop a feature
            // the request depends on.
            let missing: Vec<String> = request
                .capabilities
                .iter()
                .filter(|need| !declared.iter().any(|have| have.as_str() == need.as_str()))
                .map(|need| need.as_str().to_string())
                .collect();

            events.push(draft(
                ctx,
                &step_id,
                EventType::MODEL_ATTEMPT_STARTED,
                serde_json::json!({
                    "attempt_id": attempt_id,
                    "provider": self.model.provider_id(),
                    "route": serde_json::to_value(&request.model_route)?,
                }),
                Actor::Agent,
            ));

            if !missing.is_empty() {
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::MODEL_ATTEMPT_FAILED,
                    serde_json::json!({
                        "attempt_id": attempt_id,
                        "error_kind": "capability_missing",
                        "missing_capabilities": missing,
                    }),
                    Actor::Agent,
                ));
                events.push(step_failed(ctx, &step_id));
                return Ok(StepOutcome {
                    step_id,
                    events,
                    stop: Some(StopReason::FatalError),
                    final_content: None,
                });
            }

            let sink = self.sink(ctx);
            let redactor = Redactor::new();
            let response = match self.model.stream(&request, sink.as_ref()).await {
                Ok(response) => {
                    // The durable copy is scrubbed: a proposal's arguments are
                    // the model's, and a secret copied into them must not sit in
                    // a long-lived log (§7 row 4). `response` keeps its raw
                    // arguments — the live turn shows the model what it actually
                    // asked for, and only the record is redacted.
                    let durable = redacted_proposal(&response, &redactor);
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::MODEL_RESPONSE_COMPLETED,
                        serde_json::json!({
                            "attempt_id": attempt_id,
                            "response": serde_json::to_value(&durable)?,
                        }),
                        Actor::Agent,
                    ));
                    response
                }
                Err(error) => {
                    // The attempt's deltas stayed live-only; no partial response
                    // is written, and the failure is a first-class fact.
                    let cancelled = error.is_cancelled();
                    events.push(draft(
                        ctx,
                        &step_id,
                        if cancelled {
                            EventType::MODEL_ATTEMPT_CANCELLED
                        } else {
                            EventType::MODEL_ATTEMPT_FAILED
                        },
                        serde_json::json!({
                            "attempt_id": attempt_id,
                            "error_kind": error.kind(),
                            "message": truncate(&error.to_string(), MAX_ERROR_CHARS),
                        }),
                        Actor::Agent,
                    ));
                    events.push(step_failed(ctx, &step_id));
                    return Ok(StepOutcome {
                        step_id,
                        events,
                        stop: Some(if cancelled {
                            StopReason::UserCancelled
                        } else {
                            StopReason::FatalError
                        }),
                        final_content: None,
                    });
                }
            };

            // The assistant turn — its text and any tool proposals — enters the
            // conversation before its tool results. That is the order every
            // provider expects when the history is replayed.
            turn.messages.push(ModelMessage::assistant_for_attempt(
                &attempt_id,
                response.content_blocks.clone(),
            ));

            let tool_calls = response.tool_calls();
            if tool_calls.is_empty() {
                // A retry that was only described. The reconcile said the
                // next step is to retry, and the model then answered without
                // making the call — the prose form of the same stop the gate
                // exists to catch. Refused once, like the gate: a second
                // answer is taken.
                if let Some(retry) = turn.pending_retry.take()
                    && !turn.retry_blocked
                    && turn
                        .manifest
                        .entries
                        .iter()
                        .any(|entry| entry.tool_id == judgment::TOOL_NAME)
                {
                    let text = retry_text(&retry, listing_hint(&turn.manifest));
                    turn.retry_blocked = true;
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::JUDGMENT_REQUIRED,
                        serde_json::json!({
                            "tool_name": judgment::TOOL_NAME,
                            "content": text,
                        }),
                        Actor::Agent,
                    ));
                    let uid = format!("msg/kernel/{}", ctx.turn_id);
                    match turn
                        .messages
                        .iter_mut()
                        .find(|message| message.message_uid.as_str() == uid)
                    {
                        Some(message) => {
                            *message = ModelMessage::kernel_note_for_turn(&ctx.turn_id, text);
                        }
                        None => turn
                            .messages
                            .push(ModelMessage::kernel_note_for_turn(&ctx.turn_id, text)),
                    }
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::STEP_COMPLETED,
                        serde_json::json!({}),
                        Actor::Agent,
                    ));
                    return Ok(StepOutcome { step_id, events, stop: None, final_content: None });
                }
                // The gate. A real failure left a question the model has not
                // answered; an answer now would be the measured behaviour this
                // stage exists to stop (stopped without a check). The block
                // fires once per failure: the second answer is taken, because
                // a gate that can hang a turn is a worse defect than the one
                // it is fixing.
                // Nothing is asked twice: a turn that has made a real call
                // since the failure has done what the gate exists to ask for,
                // even if it has not written the reconcile yet.
                if let Some(failure) = turn.pending_failure.clone()
                    && turn.gate_fired_for.as_deref() != Some(failure.call_id.as_str())
                    && !turn.checked_since_failure
                    && turn
                        .manifest
                        .entries
                        .iter()
                        .any(|entry| entry.tool_id == judgment::TOOL_NAME)
                {
                    let text = gate_text(&failure, listing_hint(&turn.manifest));
                    turn.gate_fired_for = Some(failure.call_id.clone());
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::JUDGMENT_REQUIRED,
                        serde_json::json!({
                            "tool_name": failure.tool,
                            "provider_tool_call_id": failure.call_id,
                            "content": text,
                        }),
                        Actor::Agent,
                    ));
                    // Replace rather than append: a second gate in the same
                    // turn must not leave two notes with the same uid.
                    let uid = format!("msg/kernel/{}", ctx.turn_id);
                    match turn
                        .messages
                        .iter_mut()
                        .find(|message| message.message_uid.as_str() == uid)
                    {
                        Some(message) => {
                            *message = ModelMessage::kernel_note_for_turn(&ctx.turn_id, text);
                        }
                        None => turn
                            .messages
                            .push(ModelMessage::kernel_note_for_turn(&ctx.turn_id, text)),
                    }
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::STEP_COMPLETED,
                        serde_json::json!({}),
                        Actor::Agent,
                    ));
                    return Ok(StepOutcome { step_id, events, stop: None, final_content: None });
                }
                // The time gate. This turn's words name a read an earlier turn
                // made, and nothing has been looked at since the turn began:
                // the answer would restate a fact about then. Refused once,
                // like the others — a kernel note, not a hang.
                if let Some(stale) = turn.stale.clone()
                    && !turn.stale_gate_fired
                    && !turn.acted_this_turn
                    && turn.manifest.entries.iter().any(|entry| entry.tool_id == stale.tool)
                {
                    let text = stale_text(&stale);
                    turn.stale_gate_fired = true;
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::JUDGMENT_REQUIRED,
                        serde_json::json!({
                            "tool_name": stale.tool,
                            "content": text,
                        }),
                        Actor::Agent,
                    ));
                    let uid = format!("msg/kernel/{}", ctx.turn_id);
                    match turn
                        .messages
                        .iter_mut()
                        .find(|message| message.message_uid.as_str() == uid)
                    {
                        Some(message) => {
                            *message = ModelMessage::kernel_note_for_turn(&ctx.turn_id, text);
                        }
                        None => turn
                            .messages
                            .push(ModelMessage::kernel_note_for_turn(&ctx.turn_id, text)),
                    }
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::STEP_COMPLETED,
                        serde_json::json!({}),
                        Actor::Agent,
                    ));
                    return Ok(StepOutcome { step_id, events, stop: None, final_content: None });
                }
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::STEP_COMPLETED,
                    serde_json::json!({}),
                    Actor::Agent,
                ));
                return Ok(StepOutcome {
                    step_id,
                    events,
                    stop: Some(StopReason::FinalAnswer),
                    final_content: Some(response.text_content()),
                });
            }

            // The repeat gate. A third identical call is not a retry: after
            // two runs the world has already answered. The second is allowed,
            // because a transient failure deserves exactly one unchanged
            // retry (`transient`); the third is refused once, so a model with
            // no other idea is not left looping until the step budget ends.
            if self.judgment_enabled
                && !turn.repeat_refused
                && turn.identical_streak >= 2
                && let Some(call) = tool_calls.iter().find(|call| {
                    turn.last_signature.as_deref().is_some_and(|last| {
                        last == call_signature(&call.tool_name, &call.arguments).as_str()
                    })
                })
            {
                let text = repeat_text(&call.tool_name, turn.identical_streak);
                let trigger = call.tool_name.clone();
                turn.repeat_refused = true;
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::JUDGMENT_REQUIRED,
                    serde_json::json!({
                        "tool_name": call.tool_name,
                        "provider_tool_call_id": call.tool_call_id,
                        "content": text,
                    }),
                    Actor::Agent,
                ));
                let contents: Vec<String> = tool_calls
                    .iter()
                    .map(|call| {
                        if turn.last_signature.as_deref()
                            == Some(call_signature(&call.tool_name, &call.arguments).as_str())
                        {
                            repeat_text(&call.tool_name, turn.identical_streak)
                        } else {
                            format!(
                                "not executed: this step was held because `{trigger}` was about \
                                 to run again with the same arguments."
                            )
                        }
                    })
                    .collect();
                hold_step(ctx, &step_id, turn, &mut events, &tool_calls, &contents);
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::STEP_COMPLETED,
                    serde_json::json!({}),
                    Actor::Agent,
                ));
                return Ok(StepOutcome { step_id, events, stop: None, final_content: None });
            }

            for call in &tool_calls {
                // The arguments are written scrubbed: the log outlives the run
                // and is read back on resume, so what is recorded must not be
                // the secret the model happened to include (§7 row 4). The
                // invocation below still runs with the raw arguments — only the
                // record is redacted.
                let (arguments, redaction) = redactor.redact_value(&call.arguments);
                let mut payload = serde_json::json!({
                    "tool_name": call.tool_name,
                    "provider_tool_call_id": call.tool_call_id,
                    "arguments": arguments,
                });
                if redaction.redacted {
                    // Same convention as `TOOL_RESULT_REDACTED`: a reader can
                    // tell that what it sees is not the whole of what was asked.
                    payload["redaction"] = serde_json::json!({
                        "redacted": true,
                        "patterns": redaction.patterns,
                    });
                }
                events.push(draft(ctx, &step_id, EventType::TOOL_STARTED, payload, Actor::Tool));

                // `plan.update` is the loop's own tool: no provider implements
                // it, and the gateway is never asked. The ledger is written by
                // the same event the log keeps, so what the next step shows is
                // what the log says. A refused plan records no event — the
                // model gets the reason and may try again.
                if call.tool_name == plan::TOOL_NAME {
                    let (status, event_type, content) = match Plan::from_arguments(&call.arguments)
                    {
                        Ok(plan) => {
                            // The ledger is model-authored text and the log
                            // outlives the run, so it is stored scrubbed
                            // like the arguments above — and the model is
                            // handed the same scrubbed plan, so the message
                            // it sees next step is the one the log holds.
                            let (value, redaction) =
                                redactor.redact_value(&serde_json::to_value(&plan)?);
                            let plan: Plan = serde_json::from_value(value)?;
                            let mut payload = serde_json::json!({
                                "provider_tool_call_id": call.tool_call_id,
                                "plan": &plan,
                            });
                            if redaction.redacted {
                                payload["redaction"] = serde_json::json!({
                                    "redacted": true,
                                    "patterns": redaction.patterns,
                                });
                            }
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::PLAN_UPDATED,
                                payload,
                                Actor::Agent,
                            ));
                            // The plan is part of the task layer this turn
                            // already placed, so that message is rewritten
                            // in place: the next step of this turn reads
                            // it without waiting for a new turn to open.
                            if let Some(facts) = turn.ctx.state.as_ref() {
                                let advice = tool_advice(turn);
                                let text = task_text(
                                    &turn.ctx.user_input,
                                    facts,
                                    Some(&plan),
                                    turn.judgment.as_ref(),
                                    turn.stale.as_ref(),
                                    self.judgment_enabled.then_some(&advice),
                                );
                                if let Some(layers) = turn.context_layers.as_mut() {
                                    layers.task = text.clone();
                                }
                                let uid = format!("msg/task/{}", ctx.turn_id);
                                if let Some(message) = turn
                                    .messages
                                    .iter_mut()
                                    .find(|message| message.message_uid.as_str() == uid)
                                {
                                    *message = ModelMessage::task_for_turn(&ctx.turn_id, text);
                                }
                            }
                            let content = format!(
                                "plan recorded: {} step(s), {} fact(s)",
                                plan.items.len(),
                                plan.facts.len()
                            );
                            (ToolStatus::Completed, EventType::TOOL_COMPLETED, content)
                        }
                        Err(error) => (
                            ToolStatus::Failed,
                            EventType::TOOL_FAILED,
                            format!("plan not recorded: {error}"),
                        ),
                    };
                    events.push(draft(
                        ctx,
                        &step_id,
                        event_type,
                        serde_json::json!({
                            "tool_name": call.tool_name,
                            "provider_tool_call_id": call.tool_call_id,
                            "status": status,
                            "content": content,
                        }),
                        Actor::Tool,
                    ));
                    turn.messages.push(ModelMessage::tool_for_call(
                        &step_id,
                        call.tool_call_id.clone(),
                        call.tool_name.clone(),
                        content,
                        status != ToolStatus::Completed,
                    ));
                    continue;
                }

                // `judgment.update` is the loop's second own tool. One call
                // records one stage; the loop merges it into the running
                // record and records the merged snapshot, so the task layer
                // and a later fold read the same chain. A reconcile must name
                // the call that actually failed — a reconcile of something
                // else is refused with the tool it should have named.
                if self.judgment_enabled && call.tool_name == judgment::TOOL_NAME {
                    // `reviewed` names a fact the loop already has: the call
                    // that just failed. The model may send it, but a reconcile
                    // that leaves it out is filled in rather than refused —
                    // making a weak model restate something the kernel knows
                    // only spends a call.
                    let mut arguments = call.arguments.clone();
                    if arguments.is_object()
                        && arguments.get("stage").and_then(|value| value.as_str())
                            == Some(JudgmentStage::Reconcile.as_str())
                        && arguments
                            .get("reviewed")
                            .and_then(|value| value.as_str())
                            .is_none_or(|reviewed| reviewed.trim().is_empty())
                        && let Some(pending) = turn.pending_failure.as_ref()
                    {
                        arguments["reviewed"] = serde_json::json!(reviewed_from(pending));
                    }
                    let accepted: Result<String, String> =
                        match Judgment::from_arguments(&arguments) {
                            Err(error) => Err(format!("judgment not recorded: {error}")),
                            Ok(update) => {
                                // Refused once per turn, like the gate: the
                                // invariant is stated, and a model that still
                                // stops has been given its chance rather than
                                // being held in a loop. What it owes the check
                                // to is either the failure it is reconciling
                                // or the retry it promised and has not made.
                                let outstanding = turn
                                    .pending_failure
                                    .as_ref()
                                    .map(|pending| pending.tool.clone())
                                    .or_else(|| turn.pending_retry.clone());
                                let stop_needs_check = update.stage
                                    == Some(JudgmentStage::Reconcile)
                                    && update.stop
                                    && !turn.checked_since_failure
                                    && !turn.stop_refused
                                    && outstanding.is_some();
                                let refusal = turn.pending_failure.as_ref().and_then(|pending| {
                                    if update.stage != Some(JudgmentStage::Reconcile) {
                                        return None;
                                    }
                                    if !update.reconciles(&pending.tool) {
                                        return Some(format!(
                                            "judgment not recorded: the last failure was \
                                                 `{}` ({}); `reviewed` must name that tool, \
                                                 for example `{} ...`",
                                            pending.tool, pending.detail, pending.tool
                                        ));
                                    }
                                    None
                                });
                                // A stop is a decision taken after a check.
                                // Stopping first and looking never is the
                                // silent give-up in a different shape.
                                let refusal = refusal.or_else(|| {
                                    if !stop_needs_check {
                                        return None;
                                    }
                                    Some(format!(
                                        "judgment not recorded: a stop needs a check — no tool \
                                         has been called since `{}` left something open. Make \
                                         one call that looks ({}), then record reconcile again \
                                         with `stop` true if it still does not settle it.",
                                        outstanding.clone().unwrap_or_default(),
                                        listing_hint(&turn.manifest),
                                    ))
                                });
                                match refusal {
                                    Some(text) => {
                                        if stop_needs_check {
                                            turn.stop_refused = true;
                                        }
                                        Err(text)
                                    }
                                    None => {
                                        let merged = match turn.judgment.as_ref() {
                                            Some(current) => current.merged_with(&update),
                                            None => update.clone(),
                                        };
                                        turn.judgment = Some(merged.clone());
                                        // The ledger is model-authored text
                                        // and the log outlives the run, so it
                                        // is stored scrubbed like the plan —
                                        // and the model is handed the same
                                        // scrubbed record next step.
                                        let (value, redaction) =
                                            redactor.redact_value(&serde_json::to_value(&merged)?);
                                        let merged: Judgment = serde_json::from_value(value)?;
                                        let mut payload = serde_json::json!({
                                            "provider_tool_call_id": call.tool_call_id,
                                            "judgment": &merged,
                                        });
                                        if redaction.redacted {
                                            payload["redaction"] = serde_json::json!({
                                                "redacted": true,
                                                "patterns": redaction.patterns,
                                            });
                                        }
                                        events.push(draft(
                                            ctx,
                                            &step_id,
                                            EventType::JUDGMENT_UPDATED,
                                            payload,
                                            Actor::Agent,
                                        ));
                                        if update.stage == Some(JudgmentStage::Reconcile) {
                                            turn.pending_failure = None;
                                            // A retry is a promise until a
                                            // tool call makes it; the answer
                                            // gate holds the turn to it. The
                                            // promise is also a demand for a
                                            // check: a stop recorded after it
                                            // without a call is not taken.
                                            turn.pending_retry = if update.stop {
                                                None
                                            } else {
                                                Some(update.next.clone())
                                            };
                                            if turn.pending_retry.is_some() {
                                                turn.checked_since_failure = false;
                                            }
                                        }
                                        if let Some(facts) = turn.ctx.state.as_ref() {
                                            let advice = tool_advice(turn);
                                            let text = task_text(
                                                &turn.ctx.user_input,
                                                facts,
                                                turn.ctx.plan.as_ref(),
                                                Some(&merged),
                                                turn.stale.as_ref(),
                                                self.judgment_enabled.then_some(&advice),
                                            );
                                            if let Some(layers) = turn.context_layers.as_mut() {
                                                layers.task = text.clone();
                                            }
                                            let uid = format!("msg/task/{}", ctx.turn_id);
                                            if let Some(message) = turn
                                                .messages
                                                .iter_mut()
                                                .find(|message| message.message_uid.as_str() == uid)
                                            {
                                                *message =
                                                    ModelMessage::task_for_turn(&ctx.turn_id, text);
                                            }
                                        }
                                        let stage =
                                            update.stage.map(|stage| stage.as_str()).unwrap_or("");
                                        Ok(format!("judgment recorded: {stage}"))
                                    }
                                }
                            }
                        };
                    let status =
                        if accepted.is_ok() { ToolStatus::Completed } else { ToolStatus::Failed };
                    let content = match accepted {
                        Ok(text) | Err(text) => text,
                    };
                    events.push(draft(
                        ctx,
                        &step_id,
                        if status == ToolStatus::Completed {
                            EventType::TOOL_COMPLETED
                        } else {
                            EventType::TOOL_FAILED
                        },
                        serde_json::json!({
                            "tool_name": call.tool_name,
                            "provider_tool_call_id": call.tool_call_id,
                            "status": status,
                            "content": content,
                        }),
                        Actor::Tool,
                    ));
                    turn.messages.push(ModelMessage::tool_for_call(
                        &step_id,
                        call.tool_call_id.clone(),
                        call.tool_name.clone(),
                        content,
                        status != ToolStatus::Completed,
                    ));
                    continue;
                }

                // A real tool call is the retry the reconcile promised. The
                // ledgers above are not: writing a record is not checking the
                // world, so they leave `pending_retry` standing. The flags are
                // set where the call actually ran, not here: a call the gateway
                // refused never looked at anything, and counting it as a check
                // would let a stop rest on a decision rather than an
                // observation.
                let invocation = ToolInvocation::from_tool_call(
                    call.tool_call_id.clone(),
                    call.tool_name.clone(),
                    call.arguments.clone(),
                );
                match self.gateway.execute(&ctx.session_id, &invocation).await {
                    Ok(result) => {
                        // The call ran. It is the retry a reconcile promised,
                        // the check a stop may rest on, and newer than the
                        // earlier turn the task layer warned about.
                        turn.pending_retry = None;
                        turn.checked_since_failure = true;
                        turn.acted_this_turn = true;
                        turn.stale = None;
                        // The repeat gate's bookkeeping: a call that ran is
                        // the one a third identical call would repeat. The
                        // loop's own ledgers never reach this point.
                        let signature = call_signature(&call.tool_name, &call.arguments);
                        if turn.last_signature.as_deref() == Some(signature.as_str()) {
                            turn.identical_streak += 1;
                        } else {
                            turn.last_signature = Some(signature);
                            turn.identical_streak = 1;
                            turn.repeat_refused = false;
                        }
                        // What happened to the result on its way here, recorded
                        // before the result itself. These are the facts a reader
                        // needs to know that what it is looking at may not be
                        // everything: content that was cut, secrets that were
                        // removed, and where the whole of it went.
                        if !result.artifacts.is_empty() {
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_RESULT_ARTIFACT_OFFLOADED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                    "artifact_refs": artifact_refs(&result),
                                }),
                                Actor::Tool,
                            ));
                        }
                        if result.truncated {
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_RESULT_TRUNCATED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                }),
                                Actor::Tool,
                            ));
                        }
                        if result.redaction_metadata.redacted {
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_RESULT_REDACTED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                    "patterns": result.redaction_metadata.patterns,
                                }),
                                Actor::Tool,
                            ));
                        }

                        let mut payload = serde_json::json!({
                            "tool_name": call.tool_name,
                            "provider_tool_call_id": call.tool_call_id,
                            "status": result.status,
                            "content": result.content,
                            // Structured, not a hash interpolated into prose: a
                            // reader that wants the bytes needs the locator, and
                            // the model needs the same thing the log keeps.
                            "artifact_refs": artifact_refs(&result),
                            "truncated": result.truncated,
                        });
                        // Facts the tool stated, kept beside the text so a
                        // reader does not have to parse prose that only looks
                        // structured. Omitted when nothing is known, which
                        // leaves a result without facts byte-identical to the
                        // payload this loop wrote before the field existed.
                        if !result.facts.is_empty() {
                            payload["facts"] = serde_json::to_value(&result.facts)?;
                        }
                        let event_type = match result.status {
                            ToolStatus::Completed => EventType::TOOL_COMPLETED,
                            ToolStatus::Failed => EventType::TOOL_FAILED,
                            ToolStatus::Cancelled => EventType::TOOL_CANCELLED,
                        };
                        events.push(draft(ctx, &step_id, event_type, payload, Actor::Tool));
                        let is_error = result.status != ToolStatus::Completed;
                        if is_error {
                            // A real failure is one the next step could act
                            // on: the gate will not let the turn end on an
                            // answer until the record reconciles this call.
                            // The call that failed does not count as the check
                            // a stop needs — it is the question, not an answer.
                            turn.checked_since_failure = false;
                            turn.pending_failure = Some(PendingFailure {
                                call_id: call.tool_call_id.clone(),
                                tool: call.tool_name.clone(),
                                detail: first_line(&result.content),
                            });
                        }
                        turn.messages.push(ModelMessage::tool_for_call(
                            &step_id,
                            call.tool_call_id.clone(),
                            call.tool_name.clone(),
                            result.content,
                            is_error,
                        ));
                    }
                    Err(e) => {
                        if let ToolGatewayError::ApprovalPending { approval_id, .. } = &e {
                            // The call did not run, so it has no outcome of its
                            // own — but it still gets a result. The turn ends
                            // here (this loop returns `NeedsUserInput`), so the
                            // message is never sent to a provider; it is pushed
                            // to keep the turn's own message list complete and
                            // to mirror the result the log records for this
                            // step. "Nothing was executed" is a fact worth
                            // recording rather than a gap.
                            let content = format!(
                                "approval required: {approval_id}; the tool was not executed"
                            );
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_FAILED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                    "status": ToolStatus::Failed,
                                    "content": content,
                                    "outcome": APPROVAL_PENDING_OUTCOME,
                                    "approval_id": approval_id,
                                }),
                                Actor::Tool,
                            ));
                            turn.messages.push(ModelMessage::tool_for_call(
                                &step_id,
                                call.tool_call_id.clone(),
                                call.tool_name.clone(),
                                content,
                                true,
                            ));
                            return Ok(StepOutcome {
                                step_id,
                                events,
                                stop: Some(StopReason::NeedsUserInput),
                                final_content: Some(format!("approval pending: {approval_id}")),
                            });
                        }
                        // The text the model is given has to be the text the
                        // log keeps: a gateway failure that lives only in
                        // memory could never be rebuilt into this message.
                        let content = e.to_string();
                        events.push(draft(
                            ctx,
                            &step_id,
                            EventType::TOOL_FAILED,
                            serde_json::json!({
                                "tool_name": call.tool_name,
                                "provider_tool_call_id": call.tool_call_id,
                                "status": ToolStatus::Failed,
                                "content": content,
                                // Read back by the Scheduler when a resumed
                                // turn opens: a refusal is not a failure the
                                // gate should hold the turn on.
                                "outcome": GATEWAY_REFUSED_OUTCOME,
                            }),
                            Actor::Tool,
                        ));
                        // A gateway refusal (policy, resource, unknown tool)
                        // is a decision with its own actionable text, not an
                        // observation about the world: the gate is for a tool
                        // that ran and failed, which is the class measured to
                        // end in a silent stop.
                        turn.messages.push(ModelMessage::tool_for_call(
                            &step_id,
                            call.tool_call_id.clone(),
                            call.tool_name.clone(),
                            content,
                            true,
                        ));
                    }
                }
            }

            events.push(draft(
                ctx,
                &step_id,
                EventType::STEP_COMPLETED,
                serde_json::json!({}),
                Actor::Agent,
            ));
        }

        // The step is done and the turn could continue. Whether it does is the
        // caller's decision: the step budget is the Scheduler's policy, not the
        // loop's, and a loop that stopped itself could not be steered past the
        // step it just finished.
        Ok(StepOutcome { step_id, events, stop: None, final_content: None })
    }
}

fn user_text(input: &serde_json::Value) -> String {
    input.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

/// A copy of a model response whose tool proposals carry redacted arguments.
///
/// This is the copy that goes into the durable `response.completed` record, so
/// the long-lived log never holds a secret the model copied into a call. The
/// live turn keeps the raw response: the model is shown what it actually asked
/// for, and only the record is scrubbed (§7 row 4). The response shape is
/// preserved — a resumed call reads its arguments back out of the log.
fn redacted_proposal(response: &ModelResponse, redactor: &Redactor) -> ModelResponse {
    let mut durable = response.clone();
    for block in &mut durable.content_blocks {
        if let ContentBlock::ToolUse { arguments, .. } = block {
            *arguments = redactor.redact_value(arguments).0;
        }
    }
    durable
}

/// A result's offloaded artifacts as locators a reader can follow, rather than
/// as content the model has to hold.
fn artifact_refs(result: &ToolResult) -> Vec<serde_json::Value> {
    result.artifacts.iter().map(ArtifactRef::reference).collect()
}

/// The capabilities a request depends on: streaming always, plus native tool
/// calling whenever tools are on offer.
fn required_capabilities(manifest: &ToolManifest) -> Vec<ModelCapability> {
    let mut capabilities = vec![ModelCapability::new(ModelCapability::STREAMING)];
    if !manifest.is_empty() {
        capabilities.push(ModelCapability::new(ModelCapability::TOOL_CALL));
    }
    capabilities
}

fn step_failed(ctx: &TurnContext, step_id: &StepId) -> EventDraft {
    draft(ctx, step_id, EventType::STEP_FAILED, serde_json::json!({}), Actor::Agent)
}

/// The first line of a tool result, bounded — what the gate quotes back.
fn first_line(text: &str) -> String {
    truncate(text.lines().next().unwrap_or_default(), 200)
}

/// What the model is told when it answers after recording a retry without
/// making it.
fn retry_text(retry: &str, hint: String) -> String {
    format!(
        "Kernel check: you recorded a retry — `{retry}` — but answered without making the \
         call. Make it now with a tool call ({hint}). If it cannot be made, record reconcile \
         again with `stop` true and say in `next` why stopping is the right answer. A retry \
         that is only described was not made."
    )
}

/// What the model is told when its answer restates a read from an earlier
/// turn. Names the read and the one call that refreshes it.
fn stale_text(stale: &StaleRead) -> String {
    format!(
        "Kernel check: `{}` was read in an earlier turn ({}), not in this one — the answer \
         restates an old reading. Read it again now with {}, then answer.",
        stale.target, stale.tool, stale.tool
    )
}

/// What the model is told when the same call would run a third time unchanged.
fn repeat_text(tool: &str, count: u32) -> String {
    format!(
        "Kernel check: `{tool}` has already run {count} times in a row with the same \
         arguments; another identical run will do the same thing. Change an argument or use \
         another tool, or record a reconcile saying why this time will be different."
    )
}

/// What makes two calls the same call: the tool and the arguments, byte for
/// byte. This is how `kernel_eval` classifies an identical retry, so the gate
/// and the report agree on what "the same call" means.
fn call_signature(tool_name: &str, arguments: &serde_json::Value) -> String {
    format!("{tool_name}\u{0}{arguments}")
}

/// Answer a held step's calls without running them.
///
/// A step-level gate refuses the whole batch, but a provider response's tool
/// calls each need a tool result in the history that follows: a dropped call
/// id makes the next request invalid (an OpenAI-compatible endpoint answers
/// HTTP 400), and a chat template rendered locally would be just as wrong. The
/// held calls did not run, so each result says so — `contents[i]` is the gate's
/// own text for call `i` — and carries `gate_refused`, which no reader of the
/// log mistakes for an observation about the world.
fn hold_step(
    ctx: &TurnContext,
    step_id: &StepId,
    turn: &mut LoopTurn,
    events: &mut Vec<EventDraft>,
    calls: &[ToolCall],
    contents: &[String],
) {
    for (call, content) in calls.iter().zip(contents) {
        events.push(draft(
            ctx,
            step_id,
            EventType::TOOL_FAILED,
            serde_json::json!({
                "tool_name": call.tool_name,
                "provider_tool_call_id": call.tool_call_id,
                "status": ToolStatus::Failed,
                "content": content,
                "outcome": GATE_REFUSED_OUTCOME,
            }),
            Actor::Tool,
        ));
        turn.messages.push(ModelMessage::tool_for_call(
            step_id,
            call.tool_call_id.clone(),
            call.tool_name.clone(),
            content.clone(),
            true,
        ));
    }
}

/// The call to suggest when the kernel asks the model to look. Naming the
/// run's own listing tool, when it has one, is advice that can be followed;
/// "list the directory" assumes the model maps that to a call by itself.
fn listing_hint(manifest: &ToolManifest) -> String {
    if manifest.entries.iter().any(|entry| entry.tool_id == "fs.list") {
        String::from("for example fs.list on the directory")
    } else {
        String::from("list the directory")
    }
}

/// The `reviewed` line the loop fills in for a reconcile that left it out:
/// the failed call, cut to the field's own bound rather than to 160 plus an
/// ellipsis that would fail it.
fn reviewed_from(failure: &PendingFailure) -> String {
    format!("{}: {}", failure.tool, failure.detail)
        .chars()
        .take(Judgment::MAX_RECONCILE_CHARS)
        .collect()
}

/// What the model is told when it tries to answer past a real failure.
///
/// Written the way the failure feedback is written: it names the call, the
/// one thing to do about it, and the fields that do it — not "think again".
fn gate_text(failure: &PendingFailure, hint: String) -> String {
    format!(
        "Kernel check: `{}` failed ({}). Make the retry now with a tool call — {hint}, or \
         read a corrected path. Only if no retry is possible, record a reconcile with \
         judgment.update, `stop` true, and why.",
        failure.tool, failure.detail
    )
}

/// Bound durable error text: a provider message can be arbitrarily long.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut bounded: String = text.chars().take(max_chars).collect();
    bounded.push_str("...");
    bounded
}

fn draft(
    ctx: &TurnContext,
    step_id: &StepId,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    draft_scoped(ctx, Some(step_id), event_type, payload, actor)
}

/// A turn-level event: it belongs to the turn and to no step.
///
/// Which step was running is a fact worth recording for a step's own events and
/// a lie for a turn's, so the route decision — taken before any step exists —
/// carries no `step_id` rather than borrowing the first one's.
fn draft_turn(
    ctx: &TurnContext,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    draft_scoped(ctx, None, event_type, payload, actor)
}

fn draft_scoped(
    ctx: &TurnContext,
    step_id: Option<&StepId>,
    event_type: &str,
    payload: serde_json::Value,
    actor: Actor,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: ctx.session_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        turn_id: Some(ctx.turn_id.clone()),
        step_id: step_id.cloned(),
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::agent_loop::TurnFailureFacts;
    use agent_ports::decision::{
        DecisionAnswer, DecisionBackendOutcome, DecisionBackendReport, DecisionFamily,
        DecisionMode, DecisionOption, DecisionQuestion, DecisionResult, DecisionTrace,
        DecisionValue, QuestionPack,
    };
    use agent_ports::live_event_bus::LiveEventBusError;
    use agent_ports::model_provider::ModelProviderError;
    use agent_ports::model_router::{ModelRoute, ModelTier};
    use agent_ports::tool_provider::ToolSideEffect;
    use agent_protocol::instructions::RunInstructions;
    use agent_testkit::{
        DrivenTurn, FakeContextEngine, FakeDecisionAdvisor, FakeModelProvider, FakeModelStep,
        FakeToolGateway, FakeToolProvider, FixedModelRouter, NullContextEngine, drive_turn,
        one_model_router,
    };
    use std::sync::Mutex;

    /// How many steps these tests let a turn take. The loop has no opinion about
    /// this any more — the budget is the Scheduler's policy — so the tests pick
    /// one that is comfortably more than any of them needs.
    const BUDGET: u32 = 8;

    /// Drive a turn to its end without steering, the way the Scheduler does.
    async fn run_turn(loop_: &DefaultAgentLoop, ctx: &TurnContext) -> DrivenTurn {
        drive_turn(loop_, ctx, BUDGET).await.expect("the turn ran")
    }

    /// No derived history: the state of a session at its first turn. Tests about
    /// one turn's own events are not about history, so they derive nothing.
    fn no_context() -> Arc<dyn ContextEngine> {
        Arc::new(NullContextEngine::new())
    }

    /// A minimal live bus that keeps every event, for asserting that deltas
    /// stayed live. The kernel's bounded bus is too lossy for assertions.
    #[derive(Default)]
    struct RecordingBus {
        events: Mutex<Vec<LiveEvent>>,
        /// Unbounded on purpose: this fake exists so a test can assert on
        /// everything published, and a queue that dropped under load would make
        /// that assertion depend on timing.
        subscribers: Mutex<Vec<futures_channel::mpsc::UnboundedSender<LiveEvent>>>,
    }

    impl RecordingBus {
        fn new() -> Self {
            Self::default()
        }

        fn events(&self) -> Vec<LiveEvent> {
            self.events.lock().map(|e| e.clone()).unwrap_or_default()
        }

        fn event_types(&self) -> Vec<String> {
            self.events().into_iter().map(|e| e.event_type.to_string()).collect()
        }

        fn len(&self) -> usize {
            self.events.lock().map(|e| e.len()).unwrap_or(0)
        }
    }

    /// Hands out what the recording bus publishes.
    struct RecordingSubscription {
        rx: futures_channel::mpsc::UnboundedReceiver<LiveEvent>,
    }

    #[async_trait::async_trait]
    impl agent_ports::live_event_bus::LiveEventSubscription for RecordingSubscription {
        async fn next(&mut self) -> Option<LiveEvent> {
            futures_util::StreamExt::next(&mut self.rx).await
        }
    }

    #[async_trait::async_trait]
    impl LiveEventBus for RecordingBus {
        async fn publish(&self, event: LiveEvent) -> Result<(), LiveEventBusError> {
            {
                let Ok(mut events) = self.events.lock() else {
                    return Err(LiveEventBusError::Closed);
                };
                events.push(event.clone());
            }
            let Ok(mut subscribers) = self.subscribers.lock() else {
                return Err(LiveEventBusError::Closed);
            };
            subscribers.retain(|tx| tx.unbounded_send(event.clone()).is_ok());
            Ok(())
        }

        async fn subscribe(
            &self,
        ) -> Result<Box<dyn agent_ports::live_event_bus::LiveEventSubscription>, LiveEventBusError>
        {
            let Ok(mut subscribers) = self.subscribers.lock() else {
                return Err(LiveEventBusError::Closed);
            };
            let (tx, rx) = futures_channel::mpsc::unbounded();
            subscribers.push(tx);
            Ok(Box::new(RecordingSubscription { rx }))
        }
    }

    fn ctx() -> TurnContext {
        TurnContext {
            session_id: agent_protocol::ids::SessionId::from_string("s"),
            turn_id: agent_protocol::ids::TurnId::new(),
            run_id: agent_protocol::ids::RunId::new(),
            user_input: serde_json::json!({"text": "hi"}),
            instructions: None,
            state: None,
            capability: None,
            plan: None,
            judgment: None,
            pending_failure: None,
        }
    }

    #[tokio::test]
    async fn one_step_model_tool_model_answer() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("echo", serde_json::json!({"s": "hi"})),
            FakeModelProvider::answer("done"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        // Two steps: the first proposes a tool call, the second answers.
        let mut expected = vec!["model/tools.frozen"];
        expected.extend(expected_successful_step(true));
        expected.extend(expected_successful_step(false));
        assert_eq!(types, expected);
        assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
        assert_eq!(outcome.final_content.as_deref(), Some("done"));
    }

    /// The `model/request.prepared` payload of the first step.
    fn prepared_payload(outcome: &DrivenTurn) -> serde_json::Value {
        outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
            .expect("a request was prepared")
            .payload
            .clone()
    }

    #[tokio::test]
    async fn the_request_offers_every_registered_tool() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let tools = Arc::new(
            FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure).with_tool(
                "shout",
                "HELLO",
                ToolSideEffect::Pure,
            ),
        );
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;
        let payload = prepared_payload(&outcome);

        // The gateway's two tools plus `plan.update` and `judgment.update`,
        // which the loop owns and offers whenever it offers anything.
        assert_eq!(payload["tool_count"], 4);
        let required = payload["required_capabilities"].as_array().unwrap();
        assert!(
            required.iter().any(|c| c == ModelCapability::TOOL_CALL),
            "offering tools must require the capability to call them: {required:?}"
        );

        // The manifest id above is only a name; the definitions themselves are
        // what the model was shown, and they are recorded once for the turn.
        let frozen_index = outcome
            .events
            .iter()
            .position(|e| e.event_type.as_str() == EventType::MODEL_TOOLS_FROZEN)
            .expect("the turn froze its tools");
        let prepared_index = outcome
            .events
            .iter()
            .position(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
            .expect("a request was prepared");
        assert!(frozen_index < prepared_index, "the freeze precedes the request that uses it");

        let frozen = &outcome.events[frozen_index];
        assert!(frozen.step_id.is_none(), "the tool set belongs to the turn");
        assert_eq!(frozen.payload["manifest_id"], payload["tool_manifest_id"]);
        let entries = frozen.payload["entries"].as_array().expect("the definitions");
        let ids: Vec<&str> = entries.iter().filter_map(|e| e["tool_id"].as_str()).collect();
        assert_eq!(
            ids,
            ["echo", "judgment.update", "plan.update", "shout"],
            "the loop's own tool is offered with the rest, sorted by tool id as they reach the wire"
        );
        assert_eq!(frozen.payload["entries_hash"], digest::json(&frozen.payload["entries"]));
    }

    /// A `plan.update` call is answered by the loop itself: it records the
    /// ledger, the gateway is never asked, and the next step's request carries
    /// the plan the model just wrote in its task layer.
    #[tokio::test]
    async fn a_plan_update_is_recorded_and_reaches_the_next_step_task_layer() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                plan::TOOL_NAME,
                serde_json::json!({
                    "goal": "ship the parser fix",
                    "items": [
                        { "text": "read the failing test", "status": "done" },
                        { "text": "fix the tokenizer", "status": "doing" }
                    ],
                    "facts": ["the failure is in tokenize()"],
                }),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.plan = Some(Plan::default());
        let outcome = run_turn(&loop_, &turn).await;

        // The call is settled like any other, with the ledger event between the
        // start and the completion. No `tool/failed` means the empty gateway
        // was never asked.
        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(&types[5..8], ["tool/started", "plan/updated", "tool/completed"], "{types:?}");
        let update = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::PLAN_UPDATED)
            .expect("the ledger was recorded");
        assert_eq!(update.turn_id.as_ref(), Some(&turn.turn_id));
        assert!(update.step_id.is_some(), "the call belongs to the step that made it");
        assert_eq!(update.payload["plan"]["goal"], "ship the parser fix");
        assert_eq!(update.payload["plan"]["items"][1]["status"], "doing");

        // The next step of the same turn sees it: the task message was
        // rewritten in place, not added as a second layer.
        let task = task_message(&model.requests()[1], &turn.turn_id);
        assert!(task.contains("Goal: ship the parser fix"), "{task}");
        assert!(task.contains("1. [done] read the failing test"), "{task}");
        assert!(task.contains("2. [doing] fix the tokenizer"), "{task}");
        assert!(task.contains("- the failure is in tokenize()"), "{task}");
        let first = task_message(&model.requests()[0], &turn.turn_id);
        assert!(first.contains("Plan: none recorded yet."), "{first}");
        let task_messages = model.requests()[1]
            .messages
            .iter()
            .filter(|message| message.message_uid.as_str() == format!("msg/task/{}", turn.turn_id))
            .count();
        assert_eq!(task_messages, 1, "the layer is replaced, not duplicated");
    }

    /// A plan the loop refuses records nothing. The failure goes back to the
    /// model as an actionable result, and the task layer still says there is
    /// no plan rather than showing the half it could read.
    #[tokio::test]
    async fn a_refused_plan_records_no_event_and_the_model_is_told_why() {
        let long = "x".repeat(Plan::MAX_GOAL_CHARS + 1);
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(plan::TOOL_NAME, serde_json::json!({ "goal": long })),
            FakeModelProvider::answer("done"),
        ]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.plan = Some(Plan::default());
        let outcome = run_turn(&loop_, &turn).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(!types.contains(&EventType::PLAN_UPDATED), "{types:?}");
        let failed = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::TOOL_FAILED)
            .expect("the call failed");
        let content = failed.payload["content"].as_str().expect("a reason");
        assert!(content.starts_with("plan not recorded: goal is 501 characters"), "{content}");

        let second = &model.requests()[1];
        let tool_message = second
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Tool)
            .expect("the model was told what happened");
        let carried = tool_message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult { content, is_error, .. } => Some((content, is_error)),
                _ => None,
            })
            .expect("the tool message carries its result");
        assert!(carried.0.contains("plan not recorded"), "{carried:?}");
        assert!(*carried.1, "the model is told the call did not take effect");
        let task = task_message(second, &turn.turn_id);
        assert!(task.contains("Plan: none recorded yet."), "{task}");
        assert!(!task.contains("Goal:"), "{task}");
    }

    /// A `judgment.update` call is answered by the loop itself: the stage is
    /// merged into the record, the merged snapshot is the event, and the next
    /// step's task layer shows it.
    #[tokio::test]
    async fn a_judgment_update_is_recorded_and_reaches_the_next_step_task_layer() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "situate",
                    "time": "second step of three",
                    "space": "workspace root, one file",
                    "people": "the owner asked; the overwrite is theirs to decide",
                    "state": "report.txt is not in the root",
                }),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        let outcome = run_turn(&loop_, &turn).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            &types[5..8],
            ["tool/started", "judgment/updated", "tool/completed"],
            "{types:?}"
        );
        let update = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::JUDGMENT_UPDATED)
            .expect("the record was written");
        assert_eq!(update.payload["judgment"]["stage"], "situate");
        assert_eq!(update.payload["judgment"]["time"], "second step of three");

        let task = task_message(&model.requests()[1], &turn.turn_id);
        assert!(task.contains("Judgment so far:"), "{task}");
        assert!(task.contains("Time: second step of three"), "{task}");
        assert!(task.contains("People: the owner asked"), "{task}");
        let first = task_message(&model.requests()[0], &turn.turn_id);
        assert!(first.contains("Judgment: none recorded yet."), "{first}");
    }

    /// One call carries one stage; the record is a chain, so a later stage
    /// keeps the situation it was recorded against.
    #[tokio::test]
    async fn a_later_stage_keeps_the_situation_it_was_recorded_against() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "situate",
                    "time": "first step",
                    "space": "root",
                    "people": "the owner",
                    "state": "nothing read yet",
                }),
            ),
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "frame",
                    "criterion": "cat out.txt prints pong",
                }),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        let outcome = run_turn(&loop_, &turn).await;

        let updates: Vec<&serde_json::Value> = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_UPDATED)
            .map(|e| &e.payload["judgment"])
            .collect();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1]["stage"], "frame");
        assert_eq!(updates[1]["criterion"], "cat out.txt prints pong");
        assert_eq!(updates[1]["time"], "first step", "the situation is kept");
        assert_eq!(updates[1]["space"], "root");
    }

    /// The gate: after a real failure, a text-only answer is refused once, and
    /// the model is told which call to reconcile — by name, with the fields.
    #[tokio::test]
    async fn the_gate_refuses_one_final_answer_after_a_real_failure() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "report.txt"})),
            FakeModelProvider::answer("done"),
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "reconcile",
                    "reviewed": "fs.read report.txt",
                    "observed": "no such file in the root",
                    "verdict": "the path is wrong, not the file",
                    "next": "list the directory, then read reports/report.txt",
                }),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new().with_result(
            "fs.read",
            ToolResult::failed("no such file"),
            ToolSideEffect::Read,
        ));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        let outcome = run_turn(&loop_, &turn).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&EventType::JUDGMENT_REQUIRED), "{types:?}");
        assert!(types.contains(&EventType::JUDGMENT_UPDATED), "{types:?}");
        assert_eq!(outcome.final_content.as_deref(), Some("done"));

        let required = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
            .expect("the answer was refused");
        assert_eq!(required.payload["tool_name"], "fs.read");
        let text = required.payload["content"].as_str().expect("the note");
        assert!(text.contains("`fs.read` failed"), "{text}");
        assert!(text.contains("Make the retry now"), "{text}");
        assert!(text.contains("`stop` true"), "{text}");

        // The next request carries the note, and the note says it is the
        // kernel's, not the user's.
        let requests = model.requests();
        let note = requests[2]
            .messages
            .iter()
            .find(|message| message.message_uid.as_str() == format!("msg/kernel/{}", turn.turn_id))
            .expect("the note reaches the model");
        assert!(note.text_content().contains("Kernel check:"), "{}", note.text_content());
        assert_eq!(note.role, MessageRole::User);
    }

    /// The failure may belong to a turn that has already closed: an approval
    /// held the call, that turn yielded, and the resumed call failed in a turn
    /// of its own. The resumed turn opens on the failure the Scheduler read
    /// back, and the gate covers it before the first answer.
    #[tokio::test]
    async fn a_resumed_turn_is_gated_on_the_failure_of_the_turn_before_it() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::answer("the file is missing"),
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "reconcile",
                    "reviewed": "shell.run cat missing.txt",
                    "observed": "exit code 1, no such file",
                    "verdict": "the file is not there",
                    "next": "list the directory before reading again",
                }),
            ),
            FakeModelProvider::answer("the file is missing"),
        ]));
        let tools = Arc::new(FakeToolProvider::new());
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        turn.pending_failure = Some(TurnFailureFacts {
            call_id: "call-1".to_string(),
            tool: "shell.run".to_string(),
            detail: "exit code 1: no such file".to_string(),
        });
        let outcome = run_turn(&loop_, &turn).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&EventType::JUDGMENT_REQUIRED), "{types:?}");
        assert!(types.contains(&EventType::JUDGMENT_UPDATED), "{types:?}");
        assert_eq!(outcome.final_content.as_deref(), Some("the file is missing"));

        let required = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
            .expect("the answer was refused");
        assert_eq!(required.payload["tool_name"], "shell.run");
    }

    /// A reconcile that omits `reviewed` is completed with the call that
    /// actually failed, instead of costing the model a call to restate a fact
    /// the loop already holds.
    #[tokio::test]
    async fn a_reconcile_without_reviewed_is_filled_in_from_the_failure() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "report.txt"})),
            FakeModelProvider::answer("done"),
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "reconcile",
                    "next": "list the directory first, then read the file that is there",
                }),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new().with_result(
            "fs.read",
            ToolResult::failed("no such file: report.txt"),
            ToolSideEffect::Read,
        ));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        let outcome = run_turn(&loop_, &turn).await;

        let updated = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::JUDGMENT_UPDATED)
            .expect("the reconcile was recorded");
        let reviewed =
            updated.payload["judgment"]["reviewed"].as_str().expect("reviewed is present");
        assert!(reviewed.contains("fs.read"), "{reviewed}");
        assert!(reviewed.contains("no such file"), "{reviewed}");
    }

    /// A reconcile whose `next` is a retry holds the turn to it: the model
    /// cannot write "I will list the directory" and then answer. The retry is
    /// refused once; the call it promised clears it.
    #[tokio::test]
    async fn a_promised_retry_must_be_made_before_the_answer() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "report.txt"})),
            FakeModelProvider::answer("the file is missing"),
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "reconcile",
                    "next": "list the directory to find where report.txt is",
                }),
            ),
            FakeModelProvider::answer("I will list the directory"),
            FakeModelProvider::tool_call("fs.list", serde_json::json!({"path": ""})),
            FakeModelProvider::answer("reports/report.txt holds the value"),
        ]));
        let tools = Arc::new(
            FakeToolProvider::new()
                .with_result(
                    "fs.read",
                    ToolResult::failed("no such file: report.txt"),
                    ToolSideEffect::Read,
                )
                .with_result("fs.list", ToolResult::completed("reports/"), ToolSideEffect::Read),
        );
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        let outcome = run_turn(&loop_, &turn).await;

        let required: Vec<&EventDraft> = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
            .collect();
        assert_eq!(required.len(), 2, "one for the failure, one for the described retry");
        assert!(
            required[1].payload["content"].as_str().unwrap_or_default().contains("was not made"),
            "{}",
            required[1].payload["content"]
        );
        let listed = outcome.events.iter().any(|e| {
            e.event_type.as_str() == EventType::TOOL_STARTED && e.payload["tool_name"] == "fs.list"
        });
        assert!(listed, "the retry was actually made");
        assert_eq!(outcome.final_content.as_deref(), Some("reports/report.txt holds the value"));
    }

    /// A stop is a decision taken after a check. The first stop reconcile,
    /// recorded with nothing having looked at the world since the failure, is
    /// refused once; after one real call the same stop is taken.
    #[tokio::test]
    async fn a_stop_needs_a_check_before_it_is_taken() {
        let stop = serde_json::json!({
            "stage": "reconcile",
            "reviewed": "fs.read",
            "next": "stop: the file is not in this workspace",
            "stop": true,
        });
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "report.txt"})),
            FakeModelProvider::answer("I give up"),
            FakeModelProvider::tool_call(judgment::TOOL_NAME, stop.clone()),
            FakeModelProvider::tool_call("fs.list", serde_json::json!({"path": ""})),
            FakeModelProvider::tool_call(judgment::TOOL_NAME, stop),
            FakeModelProvider::answer("stopping: the file is not in this workspace"),
        ]));
        let tools = Arc::new(
            FakeToolProvider::new()
                .with_result(
                    "fs.read",
                    ToolResult::failed("no such file: report.txt"),
                    ToolSideEffect::Read,
                )
                .with_result("fs.list", ToolResult::completed("notes/"), ToolSideEffect::Read),
        );
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        let outcome = run_turn(&loop_, &turn).await;

        let refused = outcome.events.iter().find(|e| {
            e.event_type.as_str() == EventType::TOOL_FAILED
                && e.payload["tool_name"] == judgment::TOOL_NAME
        });
        let refused = refused.expect("the premature stop was refused");
        assert!(
            refused.payload["content"]
                .as_str()
                .unwrap_or_default()
                .contains("a stop needs a check"),
            "{}",
            refused.payload["content"]
        );
        let recorded: Vec<&EventDraft> = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_UPDATED)
            .collect();
        assert_eq!(recorded.len(), 1, "only the stop after the check was recorded");
        assert_eq!(recorded[0].payload["judgment"]["stop"], true);
        assert_eq!(
            outcome.final_content.as_deref(),
            Some("stopping: the file is not in this workspace")
        );
    }

    /// Time: this turn's words name a file an earlier turn read, and the first
    /// answer restates that reading. The kernel refuses once and asks for the
    /// read; the answer after the call is taken.
    #[tokio::test]
    async fn an_answer_that_restates_an_earlier_read_is_checked_once() {
        let step = StepId::from_string("step-old");
        let call = ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::tool_use(
                "c-old",
                "fs.read",
                serde_json::json!({"path": "value.txt"}),
            )],
        );
        let result = ModelMessage::tool_for_call(&step, "c-old", "fs.read", "41\n", false);
        let context = Arc::new(FakeContextEngine::new(vec![call, result], EventSeq::new(2)));
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::answer("41"),
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "value.txt"})),
            FakeModelProvider::answer("42"),
        ]));
        let tools = Arc::new(FakeToolProvider::new().with_result(
            "fs.read",
            ToolResult::completed("42\n"),
            ToolSideEffect::Read,
        ));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            context,
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.user_input =
            serde_json::json!({"text": "What number does `value.txt` contain right now?"});
        let outcome = run_turn(&loop_, &turn).await;

        let required: Vec<&EventDraft> = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
            .collect();
        assert_eq!(required.len(), 1, "one refusal, then the answer is taken");
        let content = required[0].payload["content"].as_str().unwrap_or_default();
        assert!(content.contains("read in an earlier turn"), "{content}");
        assert!(content.contains("value.txt"), "{content}");
        assert_eq!(required[0].payload["tool_name"], "fs.read");
        let read = outcome.events.iter().any(|e| {
            e.event_type.as_str() == EventType::TOOL_STARTED && e.payload["tool_name"] == "fs.read"
        });
        assert!(read, "the named read was made");
        assert_eq!(outcome.final_content.as_deref(), Some("42"));
    }

    /// An irreversible call is not held. The task layer carries the reminder
    /// before the model chooses, and the call the model makes on the first
    /// attempt runs.
    #[tokio::test]
    async fn an_irreversible_call_carries_the_branch_reminder() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({"path": "app.log", "content": "rotated"}),
            ),
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "app.log"})),
            FakeModelProvider::answer("app.log now holds rotated"),
        ]));
        let tools = Arc::new(
            FakeToolProvider::new()
                .with_tool("fs.write", "wrote", ToolSideEffect::Write)
                .with_non_idempotent("fs.write")
                .with_tool("fs.read", "rotated", ToolSideEffect::Read),
        );
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        let outcome = run_turn(&loop_, &turn).await;

        let text = task_message(&model.requests()[0], &turn.turn_id);
        assert!(text.contains("cannot be undone"), "{text}");
        assert!(text.contains("fs.write"), "the reminder names the call: {text}");
        assert!(text.contains("judgment.update"), "{text}");
        assert!(text.contains("branch"), "{text}");

        let required = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
            .count();
        assert_eq!(required, 0, "a reminder does not hold the call");
        let writes = outcome
            .events
            .iter()
            .filter(|e| {
                e.event_type.as_str() == EventType::TOOL_STARTED
                    && e.payload["tool_name"] == "fs.write"
            })
            .count();
        assert_eq!(writes, 1, "the call ran on the first attempt");
        assert_eq!(outcome.final_content.as_deref(), Some("app.log now holds rotated"));
    }

    /// A turn that offers a read tool is told to look after a write. One
    /// sentence in the task layer, no call spent on a refusal.
    #[tokio::test]
    async fn a_write_turn_is_reminded_to_read_its_write_back() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({"path": "out.txt", "content": "pong"}),
            ),
            FakeModelProvider::answer("wrote pong"),
        ]));
        let tools = Arc::new(
            FakeToolProvider::new()
                .with_tool("fs.write", "wrote", ToolSideEffect::Write)
                .with_tool("fs.read", "pong", ToolSideEffect::Read),
        );
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        let outcome = run_turn(&loop_, &turn).await;

        let text = task_message(&model.requests()[0], &turn.turn_id);
        assert!(text.contains("read the result back"), "{text}");
        assert!(
            !text.contains("cannot be undone"),
            "an idempotent write has no way back to record: {text}"
        );
        let required = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
            .count();
        assert_eq!(required, 0);
        assert_eq!(outcome.final_content.as_deref(), Some("wrote pong"));
    }

    /// A turn with no read tool is not told to look: "read it back" would name
    /// a call the model cannot make.
    #[tokio::test]
    async fn a_write_only_turn_is_not_reminded_to_look() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({"path": "out.txt", "content": "pong"}),
            ),
            FakeModelProvider::answer("wrote pong"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("fs.write", "wrote", ToolSideEffect::Write));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        let outcome = run_turn(&loop_, &turn).await;

        let text = task_message(&model.requests()[0], &turn.turn_id);
        assert!(!text.contains("read the result back"), "{text}");
        assert_eq!(outcome.final_content.as_deref(), Some("wrote pong"));
    }

    /// A turn whose tools can neither write nor change anything carries
    /// neither sentence.
    #[tokio::test]
    async fn a_read_only_turn_carries_no_advice() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "value.txt"})),
            FakeModelProvider::answer("42"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("fs.read", "42", ToolSideEffect::Read));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        let outcome = run_turn(&loop_, &turn).await;

        let text = task_message(&model.requests()[0], &turn.turn_id);
        assert!(!text.contains("cannot be undone"), "{text}");
        assert!(!text.contains("read the result back"), "{text}");
        assert_eq!(outcome.final_content.as_deref(), Some("42"));
    }

    /// The second run of an unchanged call is the one retry a transient
    /// failure deserves; the third is where the world has already answered.
    #[tokio::test]
    async fn the_third_identical_call_is_refused_and_the_second_is_not() {
        let same = serde_json::json!({"path": "x"});
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("echo", same.clone()),
            FakeModelProvider::tool_call("echo", same.clone()),
            FakeModelProvider::tool_call("echo", same.clone()),
            FakeModelProvider::tool_call("echo", serde_json::json!({"path": "y"})),
            FakeModelProvider::answer("done"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "same", ToolSideEffect::Read));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        let outcome = run_turn(&loop_, &turn).await;

        let required: Vec<&EventDraft> = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED)
            .collect();
        assert_eq!(required.len(), 1, "the third call was refused once");
        let text = required[0].payload["content"].as_str().unwrap_or_default();
        assert!(text.contains("has already run 2 times"), "{text}");
        assert!(text.contains("same arguments"), "{text}");

        let ran = outcome
            .events
            .iter()
            .filter(|e| {
                e.event_type.as_str() == EventType::TOOL_STARTED && e.payload["tool_name"] == "echo"
            })
            .count();
        assert_eq!(ran, 3, "the one retry and the changed call both ran");
        assert_eq!(outcome.final_content.as_deref(), Some("done"));
    }

    /// The pipeline is one switch: with it off, the task layer carries no
    /// advice and the repeat gate does not fire — the turn behaves the way it
    /// did before the stage opened.
    #[tokio::test]
    async fn turning_the_thinking_pipeline_off_turns_its_gates_off() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({"path": "out.txt", "content": "pong"}),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(
            FakeToolProvider::new()
                .with_tool("fs.write", "wrote", ToolSideEffect::Write)
                .with_non_idempotent("fs.write"),
        );
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        )
        .without_judgment();

        let mut turn = ctx();
        turn.state = Some(facts());
        let outcome = run_turn(&loop_, &turn).await;

        let text = task_message(&model.requests()[0], &turn.turn_id);
        assert!(!text.contains("cannot be undone"), "no reminder with the pipeline off: {text}");
        assert!(
            !outcome.events.iter().any(|e| e.event_type.as_str() == EventType::JUDGMENT_REQUIRED),
            "no gate fires with the pipeline off"
        );
        assert_eq!(outcome.final_content.as_deref(), Some("done"));
    }

    /// A reconcile that names another tool is refused with the tool it should
    /// have named; the record is not written.
    #[tokio::test]
    async fn a_reconcile_must_name_the_call_that_failed() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("fs.read", serde_json::json!({"path": "report.txt"})),
            FakeModelProvider::answer("done"),
            FakeModelProvider::tool_call(
                judgment::TOOL_NAME,
                serde_json::json!({
                    "stage": "reconcile",
                    "reviewed": "memory.search",
                    "observed": "nothing",
                    "verdict": "unclear",
                    "next": "stop",
                }),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new().with_result(
            "fs.read",
            ToolResult::failed("no such file"),
            ToolSideEffect::Read,
        ));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.judgment = Some(Judgment::default());
        let outcome = run_turn(&loop_, &turn).await;

        assert!(
            !outcome.events.iter().any(|e| e.event_type.as_str() == EventType::JUDGMENT_UPDATED),
            "a reconcile of another call records nothing"
        );
        let refusal = outcome
            .events
            .iter()
            .find(|e| {
                e.event_type.as_str() == EventType::TOOL_FAILED
                    && e.payload["tool_name"] == judgment::TOOL_NAME
            })
            .expect("the call was refused");
        let content = refusal.payload["content"].as_str().expect("a reason");
        assert!(content.contains("the last failure was `fs.read`"), "{content}");
        assert!(content.contains("must name that tool"), "{content}");
    }

    /// A secret the model puts in the plan is stored scrubbed, and the model
    /// sees the scrubbed copy next step: the ledger the log holds and the one
    /// the model reads stay the same text (§7 row 4).
    #[tokio::test]
    async fn a_secret_in_a_plan_is_recorded_scrubbed_and_read_back_the_same_way() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                plan::TOOL_NAME,
                serde_json::json!({ "goal": "deploy with sk-live-123456" }),
            ),
            FakeModelProvider::answer("done"),
        ]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.state = Some(facts());
        turn.plan = Some(Plan::default());
        let outcome = run_turn(&loop_, &turn).await;

        let update = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::PLAN_UPDATED)
            .expect("the ledger was recorded");
        assert_eq!(update.payload["plan"]["goal"], "deploy with ***");
        assert_eq!(update.payload["redaction"]["redacted"], serde_json::json!(true));
        let task = task_message(&model.requests()[1], &turn.turn_id);
        assert!(task.contains("Goal: deploy with ***"), "{task}");
        assert!(!task.contains("sk-live-123456"), "the secret is not shown either: {task}");
    }

    /// The task message a request carried, by uid.
    fn task_message(request: &ModelRequest, turn_id: &agent_protocol::ids::TurnId) -> String {
        request
            .messages
            .iter()
            .find(|message| message.message_uid.as_str() == format!("msg/task/{turn_id}"))
            .map(|message| message.text_content())
            .unwrap_or_else(|| panic!("the request carried a task layer: {:?}", request.messages))
    }

    /// The request records the exact messages it sent, as ordered references.
    /// The content of each uid is durable where it was produced; the list and
    /// the hashes are what prove which messages, in which order, the model saw.
    #[tokio::test]
    async fn the_prepared_request_records_the_exact_messages_it_sent() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;
        let payload = prepared_payload(&outcome);
        let sent = model.requests().into_iter().next().expect("one request");
        let refs = payload["messages"].as_array().expect("ordered message refs");

        assert_eq!(refs.len(), sent.messages.len());
        for (recorded, message) in refs.iter().zip(&sent.messages) {
            assert_eq!(recorded["message_uid"], message.message_uid.as_str());
            assert_eq!(recorded["role"], message.role.as_str());
            assert_eq!(
                recorded["hash"],
                serde_json::json!(digest::json(
                    &serde_json::to_value(message).expect("a message serializes")
                ))
            );
        }
        assert_eq!(
            payload["messages_hash"],
            serde_json::json!(digest::json(&serde_json::Value::Array(refs.clone())))
        );
    }

    /// What a window left out is part of what the request was: the record
    /// carries the derivation's own numbers, so a reader can tell a short
    /// conversation from a trimmed one without re-deriving the log.
    #[tokio::test]
    async fn the_prepared_request_records_what_the_window_left_out() {
        let ctx = ctx();
        let history = ModelMessage::user_for_turn(&agent_protocol::ids::TurnId::new(), "history");
        let context: Arc<dyn ContextEngine> = Arc::new(
            FakeContextEngine::new(vec![history], EventSeq::new(7)).with_usage(ContextUsage {
                dropped_messages: 4,
                estimated_tokens: 120,
                max_input_tokens: Some(500),
                reserved_output_tokens: Some(100),
            }),
        );
        let loop_ = DefaultAgentLoop::new(
            Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")])),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            context,
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx).await;
        let payload = prepared_payload(&outcome);
        assert_eq!(payload["context_revision"], 7);
        assert_eq!(payload["context_message_count"], 1);
        assert_eq!(payload["dropped_message_count"], 4);
        assert_eq!(payload["estimated_context_tokens"], 120);
        assert_eq!(payload["max_input_tokens"], 500);
        assert_eq!(payload["reserved_output_tokens"], 100);
        assert_eq!(
            payload["message_count"], 3,
            "one kept message, the trim notice, and this turn's input"
        );
        assert_eq!(payload["context_notice"]["kind"], "trimmed");
        assert_eq!(payload["context_notice"]["dropped_messages"], 4);
        let text = payload["context_notice"]["text"].as_str().unwrap();
        assert!(text.contains("4 older message(s) were omitted"), "{text}");
    }

    /// A window that is configured but not yet full is still worth one line
    /// when the next large read could exhaust it. The reminder goes after
    /// settled history, immediately before this turn's input, so it reads as a
    /// condition of this turn and cannot break the cached prefix.
    #[tokio::test]
    async fn a_context_near_the_budget_is_reminded_once_after_history() {
        let mut ctx = ctx();
        ctx.instructions = Some(RunInstructions {
            text: "the run's rules".to_string(),
            project_file: None,
            builtin: "test".to_string(),
        });
        let history = ModelMessage::user_for_turn(&agent_protocol::ids::TurnId::new(), "history");
        let context: Arc<dyn ContextEngine> = Arc::new(
            FakeContextEngine::new(vec![history], EventSeq::new(3)).with_usage(ContextUsage {
                dropped_messages: 0,
                estimated_tokens: 480,
                max_input_tokens: Some(500),
                reserved_output_tokens: Some(0),
            }),
        );
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            context,
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx).await;
        let payload = prepared_payload(&outcome);
        assert_eq!(payload["context_notice"]["kind"], "near-limit");
        assert_eq!(payload["context_notice"]["percent"], 96);
        assert_eq!(payload["context_notice"]["max_input_tokens"], 500);

        let request = &model.requests()[0];
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert_eq!(request.messages[0].text_content(), "the run's rules");
        assert_eq!(request.messages[1].text_content(), "history");
        assert_eq!(request.messages[2].role, MessageRole::System);
        assert_eq!(request.messages[2].message_uid.as_str(), format!("msg/notice/{}", ctx.turn_id));
        let notice = request.messages[2].text_content();
        assert!(notice.contains("about 96%"), "{notice}");
        assert!(notice.contains("20 tokens left"), "{notice}");
        assert_eq!(request.messages[3], ModelMessage::user_for_turn(&ctx.turn_id, "hi"));
    }

    #[tokio::test]
    async fn a_context_below_the_threshold_carries_no_notice() {
        let context: Arc<dyn ContextEngine> =
            Arc::new(FakeContextEngine::new(vec![], EventSeq::new(2)).with_usage(ContextUsage {
                dropped_messages: 0,
                estimated_tokens: 424,
                max_input_tokens: Some(500),
                reserved_output_tokens: Some(0),
            }));
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            context,
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;
        let payload = prepared_payload(&outcome);
        assert!(payload.get("context_notice").is_none(), "84% is below the threshold: {payload}");
        assert_eq!(payload["message_count"], 1, "history is empty; only this turn's input");
    }

    #[tokio::test]
    async fn a_context_without_a_configured_window_carries_no_notice() {
        let context: Arc<dyn ContextEngine> =
            Arc::new(FakeContextEngine::new(vec![], EventSeq::new(1)).with_usage(ContextUsage {
                dropped_messages: 0,
                estimated_tokens: 1_000_000,
                max_input_tokens: None,
                reserved_output_tokens: None,
            }));
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            context,
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;
        let payload = prepared_payload(&outcome);
        assert!(payload.get("context_notice").is_none(), "{payload}");
        assert_eq!(payload["message_count"], 1);
    }

    #[tokio::test]
    async fn a_model_that_cannot_call_tools_is_offered_none() {
        // The fake is scripted to answer directly; the point is that the request
        // degrades rather than failing closed on a capability it never had.
        let model = Arc::new(
            FakeModelProvider::new(vec![FakeModelProvider::answer("done")])
                .with_capabilities(vec![ModelCapability::new(ModelCapability::STREAMING)]),
        );
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;
        let payload = prepared_payload(&outcome);

        assert_eq!(payload["tool_count"], 0);
        assert_eq!(payload["required_capabilities"], serde_json::json!(["streaming"]));
        assert_eq!(outcome.stop_reason, StopReason::FinalAnswer);
    }

    #[tokio::test]
    async fn a_tool_calling_model_never_ends_the_turn_by_itself() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::tool_call(
            "echo",
            serde_json::json!({}),
        )]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router());

        // The budget is the caller's. Given one, the loop spends exactly it and
        // still reports no stop of its own: a tool call is not an ending, so a
        // model that only ever calls tools would loop forever if the caller did
        // not decide otherwise.
        let outcome = drive_turn(&loop_, &ctx(), 2).await.unwrap();
        assert_eq!(outcome.stop_reason, StopReason::MaxSteps);
        assert!(outcome.final_content.is_none());

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types.iter().filter(|t| **t == "step/started").count(), 2, "{types:?}");
    }

    /// The module doc's claim, read where it is made rather than through the
    /// caller's driver: the loop never names the budget as its own reason, so
    /// every step of a turn it cannot finish on its own ends reporting `None` —
    /// however many steps the caller pays for. `MaxSteps` next door is the
    /// driver's word, not the loop's.
    #[tokio::test]
    async fn the_step_budget_is_never_the_reason_the_loop_stops() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::tool_call(
            "echo",
            serde_json::json!({}),
        )]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router());

        let TurnStart { mut state, .. } = loop_.begin_turn(&ctx()).await.unwrap();
        for step in 1..=2 {
            let outcome = loop_.step(&mut state, &[]).await.unwrap();
            assert_eq!(outcome.stop, None, "step {step} ended the turn by itself");
        }
    }

    #[tokio::test]
    async fn an_annotated_tool_result_records_what_was_done_to_it() {
        use agent_protocol::permissions::RedactionMetadata;
        use agent_protocol::tool::ToolFacts;

        let offloaded = ToolResult {
            status: ToolStatus::Completed,
            content: "head\n... output truncated; full content saved to \
                      artifact://sha256/abc (60000 bytes, 2500 lines) ...\ntail"
                .to_string(),
            artifacts: vec![ArtifactRef {
                hash: "abc".to_string(),
                mime_type: "text/plain".to_string(),
                size: 60_000,
            }],
            truncated: true,
            facts: ToolFacts { exit_code: Some(0), line_count: Some(2500) },
            redaction_metadata: RedactionMetadata {
                redacted: true,
                patterns: vec!["token".to_string()],
            },
            duration_ms: Some(12),
        };
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("dump", serde_json::json!({})),
            FakeModelProvider::answer("done"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_result("dump", offloaded, ToolSideEffect::Read));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        // The annotations precede the settlement: they describe the result that
        // is about to be committed, and a log read top-down has to learn that
        // the content was cut before it is told the call completed.
        assert_eq!(
            types[5..10],
            [
                "tool/started",
                "tool/result.artifact_offloaded",
                "tool/result.truncated",
                "tool/result.redacted",
                "tool/completed",
            ]
        );

        let completed = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::TOOL_COMPLETED)
            .expect("the call completed");
        // Structured, so a reader that wants the bytes has a locator to follow
        // rather than a hash to parse back out of a sentence.
        assert_eq!(completed.payload["artifact_refs"][0]["locator"], "artifact://sha256/abc");
        assert_eq!(completed.payload["artifact_refs"][0]["bytes"], 60_000);
        assert_eq!(completed.payload["artifact_refs"][0]["mime"], "text/plain");
        assert_eq!(completed.payload["truncated"], true);
        // The facts the tool stated travel beside the text, so a later reader
        // can compare results without parsing the preview sentence.
        assert_eq!(
            completed.payload["facts"],
            serde_json::json!({ "exit_code": 0, "line_count": 2500 })
        );

        let redacted = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::TOOL_RESULT_REDACTED)
            .expect("a redaction was recorded");
        assert_eq!(redacted.payload["patterns"][0], "token");
    }

    #[tokio::test]
    async fn a_plain_tool_result_gets_no_annotation_events() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("done"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;

        // Nothing was offloaded, cut, or redacted, so nothing is claimed: an
        // annotation that fired on every call would say nothing.
        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(!types.iter().any(|t| t.starts_with("tool/result.")), "{types:?}");

        // A result that states no facts adds no key: a payload from a tool that
        // knows nothing must not look like one that reported "no exit status".
        let completed = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::TOOL_COMPLETED)
            .expect("the call completed");
        assert!(completed.payload.get("facts").is_none(), "{}", completed.payload);
    }

    /// An approval-pending call is recorded on the `tool/failed` *channel*, and
    /// the payload's `outcome` sentinel is what says it is a wait, not a
    /// failure. Three facts have to agree, so they are pinned together: the
    /// event type, the sentinel, and the turn's stop reason. The sentinel is the
    /// only thing that keeps a reader from reading "waited for a person" as "ran
    /// and failed" — `ToolStatus` has no "not executed" value, so the wait is
    /// forced through `Failed` (CONTRACT §5 C1.4, §7 row 8).
    #[tokio::test]
    async fn an_approval_pending_call_is_a_yield_not_a_failure() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("write_file", serde_json::json!({ "path": "x" })),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new().with_tool(
            "write_file",
            "wrote",
            ToolSideEffect::Write,
        ));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::requiring_approval(tools, "ap-42")),
            no_context(),
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;

        // Stop layer: waiting for a person, not a failure.
        assert_eq!(outcome.stop_reason, StopReason::NeedsUserInput);

        // Tool layer: the wait rides the `tool/failed` channel, and the payload
        // says as much. The sentinel is the classification the event type is not.
        let held: Vec<&_> = outcome
            .events
            .iter()
            .filter(|e| e.event_type.as_str() == EventType::TOOL_FAILED)
            .collect();
        assert_eq!(held.len(), 1, "exactly one tool/failed for the held call");
        assert_eq!(held[0].payload["outcome"], serde_json::json!(APPROVAL_PENDING_OUTCOME));
        assert_eq!(held[0].payload["approval_id"], serde_json::json!("ap-42"));

        // And it is *not* the "decidedly did not run" encoding — the two are the
        // exit-code difference of §3, so they must not be conflated.
        assert!(
            !outcome.events.iter().any(|e| e.event_type.as_str() == EventType::TOOL_NOT_STARTED),
            "a wait is not a tool/not_started"
        );
    }

    #[tokio::test]
    async fn a_secret_in_tool_arguments_never_reaches_the_durable_log() {
        // A model that copies a secret into a call's arguments must not plant it
        // in the log, which outlives the run and is read back on resume (§7 row
        // 4). Both carriers are checked: the `tool/started` dispatch and the
        // `response.completed` record the projection rebuilds the model's
        // history from.
        const KEY: &str = "ghp_SECRETVALUE0123456789";
        let arguments = serde_json::json!({
            "url": "https://api.test/",
            "headers": { "Authorization": format!("Bearer {KEY}") },
            "retries": 2,
        });
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("upload", arguments),
            FakeModelProvider::answer("done"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("upload", "ok", ToolSideEffect::Pure));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;

        // No durable payload anywhere carries the secret bytes.
        let log: Vec<&serde_json::Value> = outcome.events.iter().map(|e| &e.payload).collect();
        let log = serde_json::to_string(&log).unwrap();
        assert!(!log.contains(KEY), "secret leaked into the log: {log}");
        assert!(!log.contains("SECRETVALUE"), "secret fragment leaked: {log}");

        // The dispatch is scrubbed and shape-preserving, and flags that it was.
        let started = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::TOOL_STARTED)
            .expect("the call was dispatched");
        assert_eq!(started.payload["arguments"]["url"], serde_json::json!("https://api.test/"));
        assert_eq!(
            started.payload["arguments"]["headers"]["Authorization"],
            serde_json::json!("Bearer ***")
        );
        assert_eq!(started.payload["arguments"]["retries"], serde_json::json!(2));
        assert_eq!(started.payload["redaction"]["redacted"], true);
        assert!(
            started.payload["redaction"]["patterns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "ghp_")
        );

        // The response record carries the scrubbed proposal too.
        let completed = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::MODEL_RESPONSE_COMPLETED)
            .expect("the response was recorded");
        let response = serde_json::to_string(&completed.payload["response"]).unwrap();
        assert!(!response.contains(KEY), "secret leaked into the response record: {response}");
    }

    /// The event stream of one successful step: tool-less (`false`) or with a
    /// single tool round trip (`true`).
    fn expected_successful_step(with_tool: bool) -> Vec<&'static str> {
        let mut types = vec![
            "step/started",
            "model/request.prepared",
            "model/attempt.started",
            "model/response.completed",
        ];
        if with_tool {
            types.extend(["tool/started", "tool/completed"]);
        }
        types.push("step/completed");
        types
    }

    /// A candidate from the engine becomes one summary request and a rewritten
    /// history, all before this step's own request is sent.
    #[tokio::test]
    async fn a_compaction_summary_precedes_the_first_request_of_the_turn() {
        use agent_protocol::ids::TurnId;

        let older = ModelMessage::user_for_turn(&TurnId::from_string("t-old"), "older");
        let newer = ModelMessage::user_for_turn(&TurnId::from_string("t-new"), "newer");
        let candidate = CompactionCandidate {
            messages: vec![older.clone()],
            estimated_tokens: 1200,
            retained_tokens: 300,
            budget_tokens: 2000,
            observed_input_tokens: None,
            scope: CompactionScope::Settled,
        };
        let context = Arc::new(
            FakeContextEngine::new(vec![older.clone(), newer.clone()], EventSeq::new(3))
                .with_compaction(candidate),
        );
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::answer("the conversation greeted twice"),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "model/tools.frozen",
                "step/started",
                "context/compaction.started",
                "context/compacted",
                "model/request.prepared",
                "model/attempt.started",
                "model/response.completed",
                "step/completed",
            ]
        );

        let compacted = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTED)
            .expect("a compaction event");
        assert_eq!(compacted.payload["summary"], "the conversation greeted twice");
        assert_eq!(compacted.payload["summarized"][0]["message_uid"], older.message_uid.as_str());
        assert_eq!(compacted.payload["removed_messages"], 1);
        let summary_uid =
            compacted.payload["summary_message_uid"].as_str().expect("the summary uid").to_string();

        // Two provider calls: the summary request, then the step's own.
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages.len(), 3);
        assert_eq!(requests[0].messages[0].role, MessageRole::System);
        assert_eq!(requests[0].messages[0].text_content(), COMPACTION_PROMPT);
        assert_eq!(requests[0].messages[1], older);
        // The closing user turn is what makes this a summary request rather
        // than a continuation of the last assistant message (found under
        // summary/2, 2026-10-19).
        let instruction = requests[0].messages.last().expect("the closing instruction");
        assert_eq!(instruction.role, MessageRole::User);
        assert_eq!(instruction.text_content(), COMPACTION_INSTRUCTION);
        assert!(requests[0].capabilities.is_empty());
        assert!(requests[0].tool_manifest.entries.is_empty());

        // The step's request carries the summary in the older message's place,
        // ahead of the retained one.
        let sent = &requests[1];
        assert!(!sent.messages.iter().any(|m| m.message_uid == older.message_uid));
        assert!(sent.messages.iter().any(|m| m.message_uid == newer.message_uid));
        let summary_at = sent
            .messages
            .iter()
            .position(|m| m.message_uid.as_str() == summary_uid)
            .expect("the summary in the step's request");
        let newer_at =
            sent.messages.iter().position(|m| m.message_uid == newer.message_uid).unwrap();
        assert!(summary_at < newer_at);
        assert_eq!(sent.messages[summary_at].text_content(), "the conversation greeted twice");

        // Both the compaction and the request record what they did.
        let started = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTION_STARTED)
            .expect("a compaction start event");
        assert_eq!(started.payload["prompt_version"], "summary/5");
        assert_eq!(started.payload["message_count"], 1);
        assert_eq!(started.payload["estimated_tokens"], 1200);
        assert_eq!(started.payload["budget_tokens"], 2000);

        let prepared = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
            .expect("a prepared request");
        let refs = prepared.payload["messages"].as_array().expect("message refs");
        let uids: Vec<&str> = refs.iter().filter_map(|r| r["message_uid"].as_str()).collect();
        assert!(!uids.contains(&older.message_uid.as_str()));
        assert!(uids.contains(&summary_uid.as_str()));
    }

    /// A summarized turn's tool traffic is recorded as a ledger beside the
    /// summary, not left to the model's prose.
    #[tokio::test]
    async fn a_settled_compaction_records_the_steps_it_summarized() {
        use agent_protocol::ids::TurnId;

        let step = StepId::from_string("step-old");
        let call = ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::tool_use(
                "c1",
                "fs.write",
                serde_json::json!({ "path": "notes/a.txt" }),
            )],
        );
        let result = ModelMessage::tool_for_call(&step, "c1", "fs.write", "wrote 12 bytes", false);
        let newer = ModelMessage::user_for_turn(&TurnId::from_string("t-new"), "newer");
        let candidate = CompactionCandidate {
            messages: vec![call.clone(), result.clone()],
            estimated_tokens: 1200,
            retained_tokens: 300,
            budget_tokens: 2000,
            observed_input_tokens: None,
            scope: CompactionScope::Settled,
        };
        let context = Arc::new(
            FakeContextEngine::new(
                vec![call.clone(), result.clone(), newer.clone()],
                EventSeq::new(3),
            )
            .with_compaction(candidate),
        );
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::answer("we wrote the notes"),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let compacted = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTED)
            .expect("a compaction event");
        assert_eq!(compacted.payload["summary"], "we wrote the notes");
        assert_eq!(compacted.payload["ledger"]["view_version"], "ledger/2");
        assert_eq!(compacted.payload["ledger"]["lines"], 1);
        assert_eq!(compacted.payload["ledger"]["files"][0], "notes/a.txt");
        let ledger_text = compacted.payload["ledger"]["text"].as_str().expect("the ledger text");
        assert!(ledger_text.contains("fs.write"), "{ledger_text}");
        assert!(ledger_text.contains("-> ok: wrote 12 bytes"), "{ledger_text}");
        assert_eq!(compacted.payload["removed_messages"], 2);

        // The step's request carries summary, then ledger, then the retained
        // message, and the ledger's uid is recorded on the request side too.
        let sent = &model.requests()[1];
        let position = |needle: &str| {
            sent.messages
                .iter()
                .position(|m| m.message_uid.as_str().starts_with(needle))
                .expect("a message in the request")
        };
        let summary_at = position("msg/summary/");
        let ledger_at = position("msg/ledger/");
        let newer_at =
            sent.messages.iter().position(|m| m.message_uid == newer.message_uid).unwrap();
        assert!(summary_at < ledger_at && ledger_at < newer_at);
        assert_eq!(sent.messages[ledger_at].text_content(), ledger_text);

        let prepared = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
            .expect("a prepared request");
        assert_eq!(prepared.payload["context_ledger"]["view_version"], "ledger/2");
        assert_eq!(prepared.payload["context_ledger"]["lines"], 1);
        // Two messages replaced two: the settled count is unchanged.
        assert_eq!(prepared.payload["context_message_count"], 3);
    }

    /// A failed call leaves the window with its own record, carried whole by
    /// the next compaction — the ledger would keep only its tail.
    #[tokio::test]
    async fn a_settled_compaction_records_the_failures_it_removed() {
        use agent_protocol::ids::TurnId;

        let call = ModelMessage::new(
            MessageRole::Assistant,
            vec![ContentBlock::ToolUse {
                tool_call_id: "c1".to_string(),
                tool_name: "shell.run".to_string(),
                arguments: serde_json::json!({ "command": "scripts\\test.cmd" }),
            }],
        );
        let failed = ModelMessage::new(
            MessageRole::Tool,
            vec![ContentBlock::ToolResult {
                tool_call_id: "c1".to_string(),
                tool_name: "shell.run".to_string(),
                content: "exit code 101\n\
                          thread 'allocate' panicked at tests/spec.rs:4:5:\n\
                          expected Ok(111), got Ok(74)"
                    .to_string(),
                is_error: true,
            }],
        );
        let newer = ModelMessage::user_for_turn(&TurnId::from_string("t-new"), "newer");
        let candidate = CompactionCandidate {
            messages: vec![call.clone(), failed.clone()],
            estimated_tokens: 1200,
            retained_tokens: 300,
            budget_tokens: 2000,
            observed_input_tokens: None,
            scope: CompactionScope::Settled,
        };
        let context = Arc::new(
            FakeContextEngine::new(vec![call, failed, newer.clone()], EventSeq::new(4))
                .with_compaction(candidate),
        );
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::answer("the suite failed"),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let compacted = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTED)
            .expect("a compaction event");
        assert_eq!(compacted.payload["failures"]["view_version"], "failures/1");
        let text = compacted.payload["failures"]["text"].as_str().expect("the failure text");
        assert!(text.contains("expected Ok(111), got Ok(74)"), "{text}");
        assert_eq!(compacted.payload["failures"]["entries"], 1);
        assert_eq!(compacted.payload["failures"]["carried"], false);

        // The request carries summary, ledger, the failures record, then the
        // retained message.
        let sent = &model.requests()[1];
        let position = |needle: &str| {
            sent.messages
                .iter()
                .position(|m| m.message_uid.as_str().starts_with(needle))
                .expect("a message in the request")
        };
        let summary_at = position("msg/summary/");
        let ledger_at = position("msg/ledger/");
        let failures_at = position("msg/failures/");
        let newer_at =
            sent.messages.iter().position(|m| m.message_uid == newer.message_uid).unwrap();
        assert!(summary_at < ledger_at && ledger_at < failures_at && failures_at < newer_at);
        assert_eq!(sent.messages[failures_at].text_content(), text);

        let prepared = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
            .expect("a prepared request");
        assert_eq!(prepared.payload["context_failures"]["view_version"], "failures/1");
        assert_eq!(prepared.payload["context_failures"]["entries"], 1);
        // Two messages replaced by three: the ledger still has the call too.
        assert_eq!(prepared.payload["context_message_count"], 4);
    }

    /// The user's own words leave the window with a verbatim record beside the
    /// summary, so a later compaction cannot lose them to a model's prose.
    #[tokio::test]
    async fn a_settled_compaction_records_the_user_words_it_removed() {
        use agent_protocol::ids::TurnId;

        let older = ModelMessage::user_for_turn(
            &TurnId::from_string("t-old"),
            "never edit tests/spec.rs and keep every public item",
        );
        let newer = ModelMessage::user_for_turn(&TurnId::from_string("t-new"), "newer");
        let candidate = CompactionCandidate {
            messages: vec![older.clone()],
            estimated_tokens: 1200,
            retained_tokens: 300,
            budget_tokens: 2000,
            observed_input_tokens: None,
            scope: CompactionScope::Settled,
        };
        let context = Arc::new(
            FakeContextEngine::new(vec![older.clone(), newer.clone()], EventSeq::new(3))
                .with_compaction(candidate),
        );
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::answer("we agreed on the constraints"),
            FakeModelProvider::answer("done"),
        ]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let compacted = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::CONTEXT_COMPACTED)
            .expect("a compaction event");
        assert_eq!(compacted.payload["usertext"]["view_version"], "usertext/1");
        let text = compacted.payload["usertext"]["text"].as_str().expect("the record text");
        assert!(text.contains("never edit tests/spec.rs"), "{text}");
        assert!(text.contains("keep every public item"), "{text}");
        assert_eq!(compacted.payload["usertext"]["messages"], 1);
        assert_eq!(compacted.payload["usertext"]["carried"], false);
        // The ranking that kept it is recorded, so the choice is auditable.
        assert_eq!(
            compacted.payload["usertext"]["considered"][0]["message_uid"],
            older.message_uid.as_str()
        );
        assert!(compacted.payload["usertext"]["considered"][0]["score"].as_u64().unwrap() > 0);
        assert_eq!(compacted.payload["usertext"]["considered"][0]["kept"], true);

        // The step's request carries summary, then the record, then the
        // retained message; the user message itself is gone.
        let sent = &model.requests()[1];
        let position = |needle: &str| {
            sent.messages
                .iter()
                .position(|m| m.message_uid.as_str().starts_with(needle))
                .expect("a message in the request")
        };
        let summary_at = position("msg/summary/");
        let usertext_at = position("msg/usertext/");
        let newer_at =
            sent.messages.iter().position(|m| m.message_uid == newer.message_uid).unwrap();
        assert!(summary_at < usertext_at && usertext_at < newer_at);
        assert_eq!(sent.messages[usertext_at].text_content(), text);
        assert!(!sent.messages.iter().any(|m| m.message_uid == older.message_uid));

        let prepared = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::MODEL_REQUEST_PREPARED)
            .expect("a prepared request");
        assert_eq!(prepared.payload["context_usertext"]["view_version"], "usertext/1");
        assert_eq!(prepared.payload["context_usertext"]["messages"], 1);
        assert_eq!(prepared.payload["context_message_count"], 3);
    }

    /// A summary attempt that fails is recorded, and the history the step sends
    /// is the one it would have sent without the candidate.
    #[tokio::test]
    async fn a_failed_compaction_leaves_the_history_unchanged() {
        use agent_protocol::ids::TurnId;

        let older = ModelMessage::user_for_turn(&TurnId::from_string("t-old"), "older");
        let newer = ModelMessage::user_for_turn(&TurnId::from_string("t-new"), "newer");
        let candidate = CompactionCandidate {
            messages: vec![older.clone()],
            estimated_tokens: 1200,
            retained_tokens: 300,
            budget_tokens: 2000,
            observed_input_tokens: None,
            scope: CompactionScope::Settled,
        };
        let context = Arc::new(
            FakeContextEngine::new(vec![older.clone(), newer.clone()], EventSeq::new(3))
                .with_compaction(candidate),
        );
        let model = Arc::new(FakeModelProvider::script(vec![
            FakeModelStep::failed(ModelProviderError::Http {
                status: 503,
                body: "busy".to_string(),
            }),
            FakeModelStep::settled(FakeModelProvider::answer("done")),
        ]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "model/tools.frozen",
                "step/started",
                "context/compaction.started",
                "context/compaction.failed",
                "model/request.prepared",
                "model/attempt.started",
                "model/response.completed",
                "step/completed",
            ]
        );
        let failed = &outcome.events[3].payload;
        assert_eq!(failed["error_kind"], "http");
        assert!(failed["message"].as_str().is_some_and(|m| !m.is_empty()));

        let sent = &model.requests()[1];
        assert!(sent.messages.iter().any(|m| m.message_uid == older.message_uid));
        assert!(sent.messages.iter().any(|m| m.message_uid == newer.message_uid));
        assert!(!sent.messages.iter().any(|m| m.message_uid.as_str().starts_with("msg/summary/")));
    }

    /// A turn that outgrows the window in its own traffic is compacted once,
    /// in place: the input stays, older steps become one summary.
    #[tokio::test]
    async fn in_flight_traffic_is_compacted_once_mid_turn() {
        // Two 6,000-character tool results are about 1,560 estimated tokens
        // each against a 500-token window: the ceiling is 1,000, so the
        // second result crosses it and the first is proposed.
        let big = "x".repeat(6_000);
        let tools =
            Arc::new(
                FakeToolProvider::new()
                    .with_tool("fs.write", &big, ToolSideEffect::Write)
                    .with_tool("echo", &big, ToolSideEffect::Pure),
            );
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let context = Arc::new(FakeContextEngine::new(Vec::new(), EventSeq::new(1)).with_usage(
            ContextUsage {
                dropped_messages: 0,
                estimated_tokens: 0,
                max_input_tokens: Some(500),
                reserved_output_tokens: Some(0),
            },
        ));
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({ "path": "notes/a.txt", "content": "x" }),
            ),
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("the steps so far"),
            FakeModelProvider::answer("done"),
        ]));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());
        let ctx = ctx();

        let outcome = run_turn(&loop_, &ctx).await;

        let compacted: Vec<&agent_protocol::events::EventDraft> = outcome
            .events
            .iter()
            .filter(|event| event.event_type.as_str() == EventType::CONTEXT_COMPACTED)
            .collect();
        assert_eq!(compacted.len(), 1, "the first attempt keeps the newest group");
        assert_eq!(compacted[0].payload["scope"], "in-flight");
        let summary =
            compacted[0].payload["summary"].as_str().expect("the summary text").to_string();
        assert!(summary.starts_with("the steps so far"), "{summary}");
        // The ledger, not the model summary, names the file the summarized
        // call wrote: it is read back from the messages and cannot be dropped
        // by the model.
        let ledger =
            compacted[0].payload["ledger"]["text"].as_str().expect("the ledger text").to_string();
        assert!(ledger.contains("notes/a.txt"), "{ledger}");
        assert_eq!(compacted[0].payload["ledger"]["files"][0], "notes/a.txt");
        assert_eq!(compacted[0].payload["ledger"]["view_version"], "ledger/2");

        // The requests: two tool steps, the summary, then the final answer.
        let requests = model.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[2].messages[0].text_content(), COMPACTION_PROMPT);
        // The turn's input is shown to the summarizer as context but is not
        // part of what gets summarized: a summary of a slice that begins
        // mid-turn otherwise claims no goal was ever stated.
        assert_eq!(requests[2].messages[1].text_content(), "hi");
        let summarized = &requests[2].messages[2..requests[2].messages.len() - 1];
        assert!(summarized.iter().any(|m| m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolUse { tool_name, .. } if tool_name == "fs.write")
        })));
        let started = outcome
            .events
            .iter()
            .find(|event| event.event_type.as_str() == EventType::CONTEXT_COMPACTION_STARTED)
            .expect("the start of the compaction");
        assert_eq!(
            started.payload["context_message_uid"].as_str(),
            Some(format!("msg/user/{}", ctx.turn_id).as_str())
        );

        // The turn's own input is untouched and still first; the summary sits
        // where the summarized steps were, not before the input.
        let sent = requests.last().expect("the final request");
        let input_uid = format!("msg/user/{}", ctx.turn_id);
        assert_eq!(sent.messages[0].message_uid.as_str(), input_uid);
        assert_eq!(sent.messages[0].text_content(), "hi");
        assert!(sent.messages[1].message_uid.as_str().starts_with("msg/summary/"));
        assert!(
            sent.messages[2].message_uid.as_str().starts_with("msg/ledger/"),
            "the ledger follows the summary"
        );
        assert!(!sent.messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::ToolUse { tool_name, .. } if tool_name == "fs.write"),
            )
        }));
    }

    /// The group the first mid-turn attempt had to keep is summarized by a
    /// second attempt, once the model has acted and newer traffic exists — and
    /// no third attempt follows while the turn keeps growing.
    #[tokio::test]
    async fn a_second_mid_turn_compaction_summarizes_the_group_the_first_kept() {
        let big = "x".repeat(6_000);
        let tools =
            Arc::new(
                FakeToolProvider::new()
                    .with_tool("fs.write", &big, ToolSideEffect::Write)
                    .with_tool("echo", &big, ToolSideEffect::Pure),
            );
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let context = Arc::new(FakeContextEngine::new(Vec::new(), EventSeq::new(1)).with_usage(
            ContextUsage {
                dropped_messages: 0,
                estimated_tokens: 0,
                max_input_tokens: Some(500),
                reserved_output_tokens: Some(0),
            },
        ));
        // Four tool steps, each adding a result far past the 500-token
        // window. The queue interleaves the two summary requests: step 3
        // compacts the first result, step 4 the second, and step 5 has
        // eligible older groups but no attempt left.
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call(
                "fs.write",
                serde_json::json!({ "path": "notes/a.txt", "content": "x" }),
            ),
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("the first handover"),
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("the second handover"),
            FakeModelProvider::tool_call("echo", serde_json::json!({})),
            FakeModelProvider::answer("done"),
        ]));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let compacted: Vec<&agent_protocol::events::EventDraft> = outcome
            .events
            .iter()
            .filter(|event| event.event_type.as_str() == EventType::CONTEXT_COMPACTED)
            .collect();
        assert_eq!(compacted.len(), 2, "the second attempt happens, the third does not");
        assert!(compacted.iter().all(|event| event.payload["scope"] == "in-flight"));
        // The second attempt summarizes what the first had to keep: its
        // summary text is the one that survives, and the ledger carries the
        // first attempt's line forward instead of restating the call.
        assert_eq!(compacted[1].payload["summary"], "the second handover");
        let ledger = compacted[1].payload["ledger"]["text"]
            .as_str()
            .expect("the carried-forward ledger text");
        assert!(ledger.contains("notes/a.txt"), "{ledger}");

        let requests = model.requests();
        assert_eq!(requests.len(), 7);
        assert_eq!(requests[2].messages[0].text_content(), COMPACTION_PROMPT);
        assert_eq!(requests[4].messages[0].text_content(), COMPACTION_PROMPT);
        // The second summary request carries the first summary as one of the
        // messages it replaces.
        let second = &requests[4].messages[2..requests[4].messages.len() - 1];
        assert!(
            second.iter().any(|m| m.text_content() == "the first handover"),
            "the retained group and its summary are what the second attempt absorbs"
        );

        // The turn ends with one summary standing in for both big results,
        // the input still first and the newest step still verbatim.
        let sent = requests.last().expect("the final request");
        assert_eq!(sent.messages[0].text_content(), "hi");
        assert_eq!(
            sent.messages
                .iter()
                .filter(|m| m.message_uid.as_str().starts_with("msg/summary/"))
                .count(),
            1
        );
        assert_eq!(
            sent.messages
                .iter()
                .filter(|m| m.message_uid.as_str().starts_with("msg/ledger/"))
                .count(),
            1
        );
        assert!(!sent.messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::ToolUse { tool_name, .. } if tool_name == "fs.write"),
            )
        }));
    }

    #[tokio::test]
    async fn deltas_reach_the_live_bus_and_never_the_durable_events() {
        let model = Arc::new(FakeModelProvider::script(vec![FakeModelStep::streaming(
            vec![
                ModelStreamEvent::Started,
                ModelStreamEvent::TextDelta { text: "he".to_string() },
                ModelStreamEvent::TextDelta { text: "llo".to_string() },
            ],
            FakeModelProvider::answer("hello"),
        )]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let bus = Arc::new(RecordingBus::new());
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router())
            .with_live_bus(bus.clone());

        let outcome = run_turn(&loop_, &ctx()).await;

        assert_eq!(
            bus.event_types(),
            vec![
                "model/stream.started".to_string(),
                "model/stream.delta".to_string(),
                "model/stream.delta".to_string(),
            ]
        );
        // Live numbers are monotonic and never collide with durable seqs.
        let live_seqs: Vec<u64> = bus.events().iter().map(|e| e.live_seq).collect();
        assert_eq!(live_seqs, vec![1, 2, 3]);

        let durable: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(
            !durable.iter().any(|t| t.starts_with("model/stream.")),
            "stream deltas leaked into durable events: {durable:?}"
        );
        let mut expected = vec!["model/tools.frozen"];
        expected.extend(expected_successful_step(false));
        assert_eq!(durable, expected);
    }

    #[tokio::test]
    async fn live_seq_keeps_increasing_across_turns() {
        let model = Arc::new(FakeModelProvider::script(vec![FakeModelStep::streaming(
            vec![ModelStreamEvent::Started],
            FakeModelProvider::answer("ok"),
        )]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let bus = Arc::new(RecordingBus::new());
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router())
            .with_live_bus(bus.clone());

        run_turn(&loop_, &ctx()).await;
        run_turn(&loop_, &ctx()).await;

        let live_seqs: Vec<u64> = bus.events().iter().map(|e| e.live_seq).collect();
        assert_eq!(live_seqs, vec![1, 2]);
    }

    #[tokio::test]
    async fn interrupted_stream_writes_no_response_and_settles_as_failed() {
        let model = Arc::new(FakeModelProvider::interrupted(3));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let bus = Arc::new(RecordingBus::new());
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router())
            .with_live_bus(bus.clone());

        let outcome = run_turn(&loop_, &ctx()).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "model/tools.frozen",
                "step/started",
                "model/request.prepared",
                "model/attempt.started",
                "model/attempt.failed",
                "step/failed",
            ]
        );
        assert_eq!(outcome.stop_reason, StopReason::FatalError);
        assert!(outcome.final_content.is_none());

        // The deltas that did arrive stayed live; no half response was written.
        assert_eq!(bus.len(), 3);
        assert!(!types.contains(&"model/response.completed"));

        let failure = &outcome.events[4].payload;
        assert_eq!(failure["error_kind"], "stream_interrupted");
        assert_eq!(failure["message"], "model stream interrupted after 3 deltas");
    }

    #[tokio::test]
    async fn a_failed_attempt_is_a_fact_not_a_dropped_error() {
        let model = Arc::new(FakeModelProvider::failing(ModelProviderError::Http {
            status: 429,
            body: "slow down".to_string(),
        }));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        assert_eq!(outcome.stop_reason, StopReason::FatalError);
        let failure = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == "model/attempt.failed")
            .expect("an attempt failure event");
        assert_eq!(failure.payload["error_kind"], "http");
        assert!(failure.payload["attempt_id"].is_string());
    }

    #[tokio::test]
    async fn a_cancelled_attempt_maps_to_user_cancelled() {
        let model = Arc::new(FakeModelProvider::failing(ModelProviderError::Cancelled));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        assert_eq!(outcome.stop_reason, StopReason::UserCancelled);
        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert!(types.contains(&"model/attempt.cancelled"), "{types:?}");
        assert!(!types.contains(&"model/attempt.failed"), "{types:?}");
    }

    #[tokio::test]
    async fn a_provider_missing_a_required_capability_fails_closed() {
        let model = Arc::new(
            FakeModelProvider::new(vec![FakeModelProvider::answer("never sent")])
                .with_capabilities(vec![]),
        );
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let loop_ = DefaultAgentLoop::new(model, gateway, no_context(), one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        assert_eq!(outcome.stop_reason, StopReason::FatalError);
        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "model/tools.frozen",
                "step/started",
                "model/request.prepared",
                "model/attempt.started",
                "model/attempt.failed",
                "step/failed",
            ]
        );
        let failure = &outcome.events[4].payload;
        assert_eq!(failure["error_kind"], "capability_missing");
        assert_eq!(failure["missing_capabilities"][0], "streaming");
    }

    /// A route a test can recognise in a request.
    fn labelled_route() -> ModelRoute {
        ModelRoute::new(ModelTier::Main, "routed-provider", "routed-model")
    }

    #[tokio::test]
    async fn the_routers_choice_reaches_the_request_and_the_attempt_event() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let router = Arc::new(FixedModelRouter::new(labelled_route()));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            router.clone(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;

        // The route is in the request the provider received...
        assert_eq!(model.requests()[0].model_route.as_ref(), Some(&labelled_route()));
        // ...and in the attempt event, so a log reader can tell which model
        // answered without consulting this process's configuration.
        let started = outcome
            .events
            .iter()
            .find(|e| e.event_type.as_str() == EventType::MODEL_ATTEMPT_STARTED)
            .expect("an attempt started");
        assert_eq!(started.payload["route"]["model"], "routed-model");
        assert_eq!(started.payload["route"]["tier"], "main");
    }

    #[tokio::test]
    async fn a_turn_routes_once_however_many_steps_it_takes() {
        let model = Arc::new(FakeModelProvider::new(vec![
            FakeModelProvider::tool_call("echo", serde_json::json!({"s": "hi"})),
            FakeModelProvider::answer("done"),
        ]));
        let tools =
            Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
        let router = Arc::new(FixedModelRouter::new(labelled_route()));
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            router.clone(),
        );

        let outcome = run_turn(&loop_, &ctx()).await;

        assert_eq!(
            outcome.events.iter().filter(|e| e.event_type.as_str() == "step/started").count(),
            2
        );
        assert_eq!(
            router.requests().len(),
            1,
            "a tier that changed mid-turn would be unexplainable"
        );
        assert_eq!(router.requests()[0].message_count, 1, "the turn's first request");
    }

    /// A pack the decision fakes answer against.
    fn tier_pack() -> QuestionPack {
        QuestionPack::new(
            DecisionFamily::new(DecisionFamily::MODEL_ROUTE),
            "v1",
            vec![DecisionQuestion::categorical(
                "tier",
                "Which tier?",
                vec![DecisionOption::new("main", "main"), DecisionOption::new("fast", "fast")],
            )],
        )
    }

    /// Four shadow outcomes the turn must be indifferent to: an answer that
    /// disagrees with the router, an abstention, a backend failure, and a call
    /// that never reached the backends.
    fn shadow_traces() -> Vec<(&'static str, DecisionTrace)> {
        let pack = tier_pack();

        let mut answered = DecisionTrace::new(&pack, DecisionMode::Shadow);
        answered.chosen = Some(DecisionResult::answered(
            &pack,
            vec![DecisionAnswer::new(
                "tier",
                DecisionValue::Categorical { choice: "fast".to_string() },
            )],
        ));
        answered.chosen_backend = Some("local".to_string());

        let abstained = DecisionTrace::new(&pack, DecisionMode::Shadow);

        let mut failed = DecisionTrace::new(&pack, DecisionMode::Shadow);
        failed.backends.push(DecisionBackendReport {
            backend_id: "local".to_string(),
            revision: Some("rev-1".to_string()),
            outcome: DecisionBackendOutcome::Failed { error_kind: "timeout".to_string() },
            latency_ms: 250,
        });

        let mut aborted = DecisionTrace::new(&pack, DecisionMode::Shadow);
        aborted.aborted = Some("state_too_large".to_string());

        vec![
            ("answered", answered),
            ("abstained", abstained),
            ("failed", failed),
            ("aborted", aborted),
        ]
    }

    /// What the model saw, stripped of this process's own identifiers.
    ///
    /// Turn ids, step ids, message uids and manifest timestamps differ between
    /// two runs of the same turn by construction. Comparing them would make the
    /// test about the id generator; what has to be identical is the content,
    /// the route, and the tool manifest's entries.
    fn what_the_model_saw(requests: &[agent_protocol::model::ModelRequest]) -> serde_json::Value {
        const VOLATILE: &[&str] =
            &["message_uid", "step_id", "turn_id", "manifest_id", "created_at", "attempt_id"];
        fn strip(value: &serde_json::Value) -> serde_json::Value {
            match value {
                serde_json::Value::Object(map) => serde_json::Value::Object(
                    map.iter()
                        .filter(|(key, _)| !VOLATILE.contains(&key.as_str()))
                        .map(|(key, value)| (key.clone(), strip(value)))
                        .collect(),
                ),
                serde_json::Value::Array(items) => {
                    serde_json::Value::Array(items.iter().map(strip).collect())
                }
                other => other.clone(),
            }
        }
        strip(&serde_json::to_value(requests).expect("a request serializes"))
    }

    /// The event types of a turn, with the shadow observation removed.
    fn types_without_decisions(outcome: &DrivenTurn) -> Vec<String> {
        outcome
            .events
            .iter()
            .map(|e| e.event_type.to_string())
            .filter(|ty| ty != EventType::DECISION_TRACE)
            .collect()
    }

    #[tokio::test]
    async fn a_shadow_decision_does_not_change_the_turn() {
        // The same turn, run with no advisor and then with four different shadow
        // outcomes. The acceptance criterion is that only the observation
        // differs — so the comparison is against the baseline run, not against
        // a hand-written expectation that could drift with the loop.
        let baseline = {
            let model = Arc::new(FakeModelProvider::new(vec![
                FakeModelProvider::tool_call("echo", serde_json::json!({"s": "hi"})),
                FakeModelProvider::answer("done"),
            ]));
            let tools =
                Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
            let loop_ = DefaultAgentLoop::new(
                model.clone(),
                Arc::new(FakeToolGateway::new(tools)),
                no_context(),
                Arc::new(FixedModelRouter::new(labelled_route())),
            );
            let outcome = run_turn(&loop_, &ctx()).await;
            let sent = what_the_model_saw(&model.requests());
            (types_without_decisions(&outcome), outcome, sent)
        };

        for (label, trace) in shadow_traces() {
            let model = Arc::new(FakeModelProvider::new(vec![
                FakeModelProvider::tool_call("echo", serde_json::json!({"s": "hi"})),
                FakeModelProvider::answer("done"),
            ]));
            let tools =
                Arc::new(FakeToolProvider::new().with_tool("echo", "hello", ToolSideEffect::Pure));
            let advisor = Arc::new(FakeDecisionAdvisor::returning(trace));
            let loop_ = DefaultAgentLoop::new(
                model.clone(),
                Arc::new(FakeToolGateway::new(tools)),
                no_context(),
                Arc::new(FixedModelRouter::new(labelled_route())),
            )
            .with_decisions(advisor.clone());

            let outcome = run_turn(&loop_, &ctx()).await;

            assert_eq!(types_without_decisions(&outcome), baseline.0, "shadow outcome '{label}'");
            assert_eq!(outcome.stop_reason, baseline.1.stop_reason, "shadow outcome '{label}'");
            assert_eq!(outcome.final_content, baseline.1.final_content, "shadow outcome '{label}'");
            assert_eq!(
                what_the_model_saw(&model.requests()),
                baseline.2,
                "shadow outcome '{label}' sent a different request"
            );
            assert_eq!(advisor.call_count(), 1, "asked once per turn, and it was asked");
        }
    }

    #[tokio::test]
    async fn the_decision_trace_is_a_turn_level_event_before_the_first_step() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let advisor = Arc::new(FakeDecisionAdvisor::new());
        let loop_ = DefaultAgentLoop::new(
            model,
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        )
        .with_decisions(advisor.clone());

        let ctx = ctx();
        let outcome = run_turn(&loop_, &ctx).await;

        let types: Vec<&str> = outcome.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types[1], EventType::DECISION_TRACE, "the route was decided before any step");
        assert_eq!(types[2], "step/started");

        let trace_event = &outcome.events[1];
        assert!(trace_event.step_id.is_none(), "no step was running when the route was chosen");
        assert_eq!(
            trace_event.turn_id.as_ref(),
            Some(&ctx.turn_id),
            "the trace is turn-level: it outlives the step it was taken for"
        );
        assert!(trace_event.payload["trace"]["trace_id"].is_string());

        // The advisor saw the turn's shape, not the conversation.
        let calls = advisor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.message_count, 1);
        assert_eq!(calls[0].0.context_revision, EventSeq::ZERO);
    }

    #[tokio::test]
    async fn the_advisors_answer_never_becomes_a_route() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        // An advisor that answered "fast" in a turn routed to the labelled route.
        let (_, disagreeing) = shadow_traces().remove(0);
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            Arc::new(FixedModelRouter::new(labelled_route())),
        )
        .with_decisions(Arc::new(FakeDecisionAdvisor::returning(disagreeing)));

        run_turn(&loop_, &ctx()).await;

        assert_eq!(
            model.requests()[0].model_route.as_ref(),
            Some(&labelled_route()),
            "the router keeps the final say: the advice was recorded, not obeyed"
        );
    }

    #[tokio::test]
    async fn the_request_carries_the_derived_context_revision() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let context =
            Arc::new(FakeContextEngine::new(Vec::new(), agent_protocol::ids::EventSeq::new(41)));
        let loop_ = DefaultAgentLoop::new(model, gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        // The revision names the log prefix the history came from, so a request
        // can be traced back to what it saw. A hardcoded zero would
        // look the same here and mean nothing.
        assert_eq!(prepared_payload(&outcome)["context_revision"], 41);
    }

    #[tokio::test]
    async fn the_loop_prepends_the_derived_history_to_the_current_input() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let tools = Arc::new(FakeToolProvider::new());
        let gateway = Arc::new(FakeToolGateway::new(tools));
        let earlier = ModelMessage::user_for_turn(
            &agent_protocol::ids::TurnId::from_string("t-earlier"),
            "earlier",
        );
        let context = Arc::new(FakeContextEngine::new(
            vec![earlier.clone()],
            agent_protocol::ids::EventSeq::new(7),
        ));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let turn = ctx();
        run_turn(&loop_, &turn).await;

        let sent = model.requests();
        assert_eq!(sent.len(), 1, "one request in a tool-less turn");
        let messages = &sent[0].messages;
        assert_eq!(messages.len(), 2, "history, then this turn's input: {messages:?}");
        assert_eq!(messages[0], earlier, "the derived history leads");
        assert_eq!(
            messages[1],
            ModelMessage::user_for_turn(&turn.turn_id, "hi"),
            "the current input follows, named after the turn it arrived in"
        );
    }

    #[tokio::test]
    async fn the_run_instructions_lead_the_history_and_the_input() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
        let earlier = ModelMessage::user_for_turn(
            &agent_protocol::ids::TurnId::from_string("t-earlier"),
            "earlier",
        );
        let context = Arc::new(FakeContextEngine::new(
            vec![earlier.clone()],
            agent_protocol::ids::EventSeq::new(7),
        ));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let mut turn = ctx();
        turn.instructions = Some(RunInstructions {
            text: "be careful".to_string(),
            project_file: None,
            builtin: "test/1".to_string(),
        });
        run_turn(&loop_, &turn).await;

        let messages = &model.requests()[0].messages;
        assert_eq!(messages.len(), 3, "instructions, history, input: {messages:?}");
        assert_eq!(
            messages[0],
            ModelMessage::system_for_run(&turn.run_id, "be careful"),
            "the run's instructions lead, named after the run"
        );
        assert_eq!(messages[1], earlier);
        assert_eq!(messages[2], ModelMessage::user_for_turn(&turn.turn_id, "hi"));
    }

    #[tokio::test]
    async fn a_previous_run_s_instructions_are_replaced_rather_than_followed() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
        let stale = ModelMessage::system_for_run(
            &agent_protocol::ids::RunId::from_string("run-earlier"),
            "old instructions",
        );
        let context = Arc::new(FakeContextEngine::new(
            vec![
                stale,
                ModelMessage::user_for_turn(
                    &agent_protocol::ids::TurnId::from_string("t-earlier"),
                    "earlier",
                ),
            ],
            agent_protocol::ids::EventSeq::new(7),
        ));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let mut turn = ctx();
        turn.instructions = Some(RunInstructions {
            text: "new instructions".to_string(),
            project_file: None,
            builtin: "test/1".to_string(),
        });
        run_turn(&loop_, &turn).await;

        let messages = &model.requests()[0].messages;
        assert_eq!(messages.len(), 3, "one system message, then history, then input");
        assert_eq!(
            messages[0],
            ModelMessage::system_for_run(&turn.run_id, "new instructions"),
            "the derived message is replaced, not preceded by a second one"
        );
        assert!(
            !messages.iter().any(|m| m.message_uid.as_str() == "msg/system/run-earlier"),
            "the previous run's message must not survive: {messages:?}"
        );
    }

    fn facts() -> TurnStateFacts {
        TurnStateFacts {
            started_at: chrono::DateTime::parse_from_rfc3339("2026-10-18T14:05:00Z")
                .unwrap()
                .with_timezone(&Utc),
            queued_inputs: 1,
            input_kind: TurnInputKind::User,
            previous_turn: Some(TurnEnding::Cancelled),
            max_steps: 16,
        }
    }

    /// The two layers are properties of the turn: they lead history, follow
    /// the run's instructions, and are recorded as what the request carried.
    #[tokio::test]
    async fn the_state_and_task_layers_lead_history_after_the_instructions() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
        let earlier = ModelMessage::user_for_turn(
            &agent_protocol::ids::TurnId::from_string("t-earlier"),
            "earlier",
        );
        let context = Arc::new(FakeContextEngine::new(
            vec![earlier.clone()],
            agent_protocol::ids::EventSeq::new(7),
        ));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let mut turn = ctx();
        turn.instructions = Some(RunInstructions {
            text: "be careful".to_string(),
            project_file: None,
            builtin: "test/1".to_string(),
        });
        turn.state = Some(facts());
        let outcome = run_turn(&loop_, &turn).await;

        let messages = &model.requests()[0].messages;
        assert_eq!(messages.len(), 5, "instructions, history, state, task, input: {messages:?}");
        assert_eq!(messages[0], ModelMessage::system_for_run(&turn.run_id, "be careful"));
        assert_eq!(messages[1], earlier);
        assert_eq!(messages[2], ModelMessage::state_for_turn(&turn.turn_id, state_text(&facts())));
        assert_eq!(
            messages[3],
            ModelMessage::task_for_turn(
                &turn.turn_id,
                task_text(&turn.user_input, &facts(), None, None, None, None),
            )
        );
        assert_eq!(messages[4], ModelMessage::user_for_turn(&turn.turn_id, "hi"));

        // The layers are sent but not settled: the derivation's own numbers
        // describe history, and placing two per-turn messages does not move
        // them.
        let payload = prepared_payload(&outcome);
        assert_eq!(payload["context_message_count"], 1);
        assert_eq!(payload["estimated_context_tokens"], 0);

        let layers = &payload["context_layers"];
        assert_eq!(layers["state"]["message_uid"], format!("msg/state/{}", turn.turn_id));
        let state = layers["state"]["text"].as_str().unwrap();
        assert!(state.contains("State at 2026-10-18T14:05:00Z."), "{state}");
        assert!(state.contains("Queued input(s): 1."), "{state}");
        assert!(state.contains("Previous turn: cancelled by the user."), "{state}");
        let task = layers["task"]["text"].as_str().unwrap();
        assert!(task.contains("Task for this turn: \"hi\""), "{task}");
        assert!(task.contains("Input: user turn."), "{task}");
        assert!(task.contains("at most 16 model steps"), "{task}");
    }

    /// The volatile frame goes after settled history, in one run: state, task,
    /// then the reminder about the window, then the input they frame.
    #[tokio::test]
    async fn the_layers_are_placed_after_history_before_the_input() {
        let mut turn = ctx();
        turn.instructions = Some(RunInstructions {
            text: "rules".to_string(),
            project_file: None,
            builtin: "test/1".to_string(),
        });
        turn.state = Some(facts());
        let history = ModelMessage::user_for_turn(&agent_protocol::ids::TurnId::new(), "history");
        let context: Arc<dyn ContextEngine> =
            Arc::new(FakeContextEngine::new(vec![history.clone()], EventSeq::new(3)).with_usage(
                ContextUsage {
                    dropped_messages: 0,
                    estimated_tokens: 480,
                    max_input_tokens: Some(500),
                    reserved_output_tokens: Some(0),
                },
            ));
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            context,
            one_model_router(),
        );

        run_turn(&loop_, &turn).await;

        let messages = &model.requests()[0].messages;
        let uids: Vec<&str> = messages.iter().map(|m| m.message_uid.as_str()).collect();
        assert_eq!(
            uids,
            vec![
                format!("msg/system/{}", turn.run_id),
                history.message_uid.as_str().to_string(),
                format!("msg/state/{}", turn.turn_id),
                format!("msg/task/{}", turn.turn_id),
                format!("msg/notice/{}", turn.turn_id),
                format!("msg/user/{}", turn.turn_id),
            ]
        );
    }

    /// No facts, no layers — and a payload byte-for-byte in the shape it had
    /// before the layers existed. A harness is not told a made-up time.
    #[tokio::test]
    async fn a_caller_without_facts_places_no_layers() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let gateway = Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new())));
        let context = Arc::new(FakeContextEngine::new(vec![], EventSeq::new(1)));
        let loop_ = DefaultAgentLoop::new(model.clone(), gateway, context, one_model_router());

        let outcome = run_turn(&loop_, &ctx()).await;

        let messages = &model.requests()[0].messages;
        assert_eq!(messages.len(), 1, "this turn's input only: {messages:?}");
        let payload = prepared_payload(&outcome);
        assert!(payload.get("context_layers").is_none(), "{payload}");
        assert!(payload.get("context_capability").is_none(), "{payload}");
    }

    /// The run's capability layer: the boundary first, then an index of what
    /// can be called. It is run-scoped and recorded rather than folded into
    /// history, so a replay does not grow a turn of its own.
    #[tokio::test]
    async fn the_capability_layer_states_the_boundary_and_indexes_the_tools() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let tools =
            Arc::new(
                FakeToolProvider::new()
                    .with_tool("fs.read", "text", ToolSideEffect::Read)
                    .with_tool("fs.glob", "paths", ToolSideEffect::Read),
            );
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(tools)),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.capability = Some(TurnCapabilityFacts {
            sandbox: SandboxMode::LocalProcess,
            network_allowed: false,
        });
        let outcome = run_turn(&loop_, &turn).await;

        let messages = &model.requests()[0].messages;
        assert_eq!(messages.len(), 2, "the layer, then this turn's input: {messages:?}");
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[0].message_uid.as_str(), format!("msg/capability/{}", turn.run_id));
        let text = messages[0].text_content();
        assert!(text.starts_with("Capabilities in this run:\n"), "{text}");
        assert!(text.contains("- sandbox: local (commands run on the host"), "{text}");
        assert!(text.contains("- network for sandboxed commands: denied"), "{text}");
        // The gateway's two tools plus the loop's own `judgment.update` and
        // `plan.update`.
        assert!(
            text.contains("4 tools are available; full arguments are in the tool definitions:"),
            "{text}"
        );
        // Sorted by tool id, and each entry is the description's first
        // sentence — the full argument list lives in the tool definitions.
        let glob = text.find("fs.glob — test tool fs.glob").expect("an index entry");
        let read = text.find("fs.read — test tool fs.read").expect("an index entry");
        let judgment = text.find("judgment.update — ").expect("the loop's own tool is indexed");
        let plan = text.find("plan.update — ").expect("the loop's own tool is indexed");
        assert!(
            glob < read && read < judgment && judgment < plan,
            "the index reads in tool order: {text}"
        );

        // Sent but not settled: the layer is not part of the derived context.
        let payload = prepared_payload(&outcome);
        assert_eq!(payload["context_message_count"], 0);
        assert_eq!(payload["estimated_context_tokens"], 0);
        assert_eq!(
            payload["context_capability"]["message_uid"],
            format!("msg/capability/{}", turn.run_id)
        );
        assert_eq!(payload["context_capability"]["text"], text);
    }

    /// The layer sits between the run's instructions and the turn's own frame:
    /// what this run is, then what this turn is.
    #[tokio::test]
    async fn the_capability_layer_follows_the_instructions_and_precedes_the_state() {
        let model = Arc::new(FakeModelProvider::new(vec![FakeModelProvider::answer("done")]));
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.instructions = Some(RunInstructions {
            text: "rules".to_string(),
            project_file: None,
            builtin: "test/1".to_string(),
        });
        turn.capability =
            Some(TurnCapabilityFacts { sandbox: SandboxMode::ReadOnly, network_allowed: false });
        turn.state = Some(facts());
        run_turn(&loop_, &turn).await;

        let messages = &model.requests()[0].messages;
        let uids: Vec<&str> = messages.iter().map(|m| m.message_uid.as_str()).collect();
        assert_eq!(
            uids,
            vec![
                format!("msg/system/{}", turn.run_id),
                format!("msg/capability/{}", turn.run_id),
                format!("msg/state/{}", turn.turn_id),
                format!("msg/task/{}", turn.turn_id),
                format!("msg/user/{}", turn.turn_id),
            ]
        );
    }

    /// A run that offers no tools says so rather than printing an empty index.
    /// No tool calling means no manifest at all — not even the loop's plan tool,
    /// which is offered only to a provider that can call something.
    #[tokio::test]
    async fn a_run_without_tools_says_nothing_is_available() {
        let model = Arc::new(
            FakeModelProvider::new(vec![FakeModelProvider::answer("done")])
                .with_capabilities(vec![ModelCapability::new(ModelCapability::STREAMING)]),
        );
        let loop_ = DefaultAgentLoop::new(
            model.clone(),
            Arc::new(FakeToolGateway::new(Arc::new(FakeToolProvider::new()))),
            no_context(),
            one_model_router(),
        );

        let mut turn = ctx();
        turn.capability =
            Some(TurnCapabilityFacts { sandbox: SandboxMode::Container, network_allowed: true });
        run_turn(&loop_, &turn).await;

        let text = model.requests()[0].messages[0].text_content();
        assert!(text.contains("- sandbox: container (commands run inside a container"), "{text}");
        assert!(text.contains("- network for sandboxed commands: allowed"), "{text}");
        assert!(text.contains("- no tools are available in this run."), "{text}");
        assert!(!text.contains("tools are available; full arguments"), "{text}");
    }

    /// An index entry is one bounded line: a long description is cut rather
    /// than allowed to grow the layer without limit.
    #[test]
    fn a_capability_index_entry_is_the_first_sentence_and_bounded() {
        assert_eq!(gist("Read a file. It returns the text."), "Read a file");
        assert_eq!(gist("  Find\n files  by pattern."), "Find files by pattern.");

        let long = "Search ".to_string() + &"the workspace ".repeat(20);
        let gist = gist(&long);
        assert!(gist.starts_with("Search the workspace"), "{gist}");
        assert_eq!(gist.chars().count(), MAX_CAPABILITY_GIST_CHARS + 3, "{gist}");
        assert!(gist.ends_with("..."), "{gist}");
    }

    #[test]
    fn the_state_layer_says_the_queue_and_the_previous_ending() {
        let mut facts = facts();
        facts.queued_inputs = 0;
        facts.previous_turn = None;
        let text = state_text(&facts);
        assert!(text.contains("Queued input(s): none."), "{text}");
        assert!(text.contains("Previous turn: none."), "{text}");

        for (ending, expected) in [
            (TurnEnding::Completed, "Previous turn: completed."),
            (TurnEnding::Failed, "Previous turn: failed."),
            (TurnEnding::Interrupted, "Previous turn: interrupted."),
            (TurnEnding::Yielded, "Previous turn: yielded (waiting on a decision)."),
        ] {
            facts.previous_turn = Some(ending);
            assert!(state_text(&facts).contains(expected), "{ending:?}");
        }
    }

    #[test]
    fn the_task_layer_bounds_a_long_input_and_names_the_input_kind() {
        let facts =
            TurnStateFacts { input_kind: TurnInputKind::Scheduled, max_steps: 1, ..facts() };
        let long = "x".repeat(MAX_TASK_CHARS + 10);
        let text = task_text(&serde_json::json!({ "text": long }), &facts, None, None, None, None);

        assert!(text.contains("Task for this turn: \"xxx"), "{text}");
        assert!(text.contains("...\""), "cut text is marked: {text}");
        assert!(!text.contains(&"x".repeat(MAX_TASK_CHARS + 1)), "the tail is not quoted");
        assert!(text.contains("Input: scheduled turn."), "{text}");
        assert!(text.contains("at most 1 model step."), "singular: {text}");
    }

    #[test]
    fn long_error_text_is_bounded() {
        let long = "x".repeat(MAX_ERROR_CHARS + 10);
        let bounded = truncate(&long, MAX_ERROR_CHARS);
        assert_eq!(bounded.chars().count(), MAX_ERROR_CHARS + 3);
        assert!(bounded.ends_with("..."));
        assert_eq!(truncate("short", MAX_ERROR_CHARS), "short");
    }
}
