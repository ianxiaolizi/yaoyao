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
    AgentLoop, AgentLoopError, EventSink, Journal, SteeringInput, StepOutcome, StopReason,
    TurnContext, TurnEnding, TurnInputKind, TurnStart, TurnState,
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
use agent_ports::model_router::{ModelRoute, ModelRouter, ModelTier, RouteRequest, TaskClass};
use agent_ports::tool_gateway::{ToolGateway, ToolGatewayError};
use agent_ports::tool_provider::{ToolInvocation, ToolResult, ToolStatus};
use agent_protocol::compaction::fit_leading_groups;
use agent_protocol::digest;
use agent_protocol::events::{Actor, EventDraft, EventType, LiveEvent};
use agent_protocol::failures::Failures;
use agent_protocol::ids::{AttemptId, EventSeq, SessionId, StepId};
use agent_protocol::judgment::{self, Judgment, JudgmentStage};
use agent_protocol::ledger::Ledger;
use agent_protocol::model::{ToolCall, estimate_message};
use agent_protocol::permissions::ArtifactRef;
use agent_protocol::plan::{self, Plan, PlanItemStatus};
use agent_protocol::profile::{ResolvedContextBudget, RunBudget, Spent};
use agent_protocol::reason::{
    self as reason_record, Basis, Mode, ReasonKind, ReasonSource, Recorder, Situation, SpeechAct,
    Stance,
};
use agent_protocol::redactor::Redactor;
use agent_protocol::state::{self, WorkState};
use agent_protocol::tool::{
    APPROVAL_PENDING_OUTCOME, GATE_REFUSED_OUTCOME, GATEWAY_REFUSED_OUTCOME, TrustLevel,
};
use agent_protocol::usertext::UserText;

use crate::checks::{check_command, first_line, is_check_command};
use crate::gates::{call_signature, repeat_text, retry_text, stale_text};
use crate::verdict;
use chrono::Utc;

use crate::context_layers::{
    ContextLayers, ContextNotice, PendingFailure, StaleRead, TaskLedger, ToolAdvice, ToolSets,
    capability_text, context_notice, identity_text, repair_note, stale_read, state_text, task_text,
    tool_source, truncate, user_text,
};
use event_drafts::{draft, draft_turn, reason_event};
use loop_call::{LoopCallAnswer, answer_loop_call};
use wording::{
    ending_word, gate_text, listing_hint, reviewed_from, sandbox_word, status_word,
    summarize_arguments,
};
mod budget;
mod capability;
mod completion;
mod context_recovery;
mod convergence;
mod cycles;
mod event_drafts;
mod loop_call;
mod output_recovery;
use output_recovery::RecoveryOutcome;
mod refusals;
mod request_record;
use request_record::message_references;
mod settle;
mod settlement;
mod stall;
mod wording;
mod work_state;
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
    /// What the loop scrubs before a proposal, a response or a ledger entry is
    /// written to the log.
    ///
    /// One redactor shared with the gateway in the assembled binary: the tool
    /// boundary and the loop must not disagree about what a secret looks like,
    /// or about whether the high-entropy layer is reporting or replacing. The
    /// default is the same policy a caller that never says anything gets.
    redactor: Redactor,
    /// Whether the judgment ledger, its tool and its advice are part of this
    /// loop.
    ///
    /// On by default. The switch exists for the evaluation that has to show
    /// what the ledger costs: the same fixtures are run with it off to get a
    /// baseline on the same build, rather than compared against a different
    /// one. The deterministic gates are a separate switch, so an arm can keep
    /// them without paying for the tool on every step.
    judgment_ledger: bool,
    /// Whether the gates that need no ledger run: the stale-read gate and the
    /// repeat gate. On in the full and gates arms, off in the baseline.
    ///
    /// These are refusals the kernel can decide from the manifest and the
    /// turn's own calls. The failure, retry and stop gates are not here: they
    /// are answers to a `judgment.update` call, so they cannot fire without
    /// the tool.
    judgment_gates: bool,
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
            redactor: Redactor::new(),
            judgment_ledger: true,
            judgment_gates: true,
        }
    }

    /// Use `redactor` for everything this loop writes into the log.
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Turn the thinking pipeline off: no ledger, no advice, no gates.
    ///
    /// This is the baseline arm of the evaluation, not a user preference: with
    /// the ledger off the tool is not offered and no record is written; with
    /// the gates off the stale-read and repeat refusals do not fire either.
    /// Everything else about the turn is unchanged.
    pub fn without_judgment(mut self) -> Self {
        self.judgment_ledger = false;
        self.judgment_gates = false;
        self
    }

    /// Keep the deterministic gates, drop the ledger.
    ///
    /// The middle arm of the evaluation: the repeat and stale-read gates still
    /// refuse, but the loop does not offer `judgment.update`, does not put the
    /// ledger or its advice in the task layer, and cannot run the reconcile
    /// gates that need the tool.
    pub fn without_ledger(mut self) -> Self {
        self.judgment_ledger = false;
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
        mut candidate: CompactionCandidate,
        fallback: &'static str,
        events: &mut Vec<EventDraft>,
    ) -> Result<(), AgentLoopError> {
        let ctx = turn.ctx.clone();
        let attempt_id = AttemptId::new();

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

        // The answer cap for the summary is the summary route's own when
        // assembly resolved one, else the main completion cap. The turn's
        // `max_output_tokens` is the wrong number twice over: it is sized for a
        // reasoning model this request may not be routed to, and a summary is
        // prose, not a train of thought.
        let summary_cap = turn
            .budget
            .summary_max_output_tokens
            .or(turn.budget.context.map(|context| context.completion_cap_tokens));
        let summary_budget = |mut budget: ModelBudget| {
            if summary_cap.is_some() {
                budget.max_tokens = summary_cap;
            }
            budget
        };

        // A summarizer is not the turn. It is asked a different question with a
        // different request, so it gets its own route: a configuration that
        // named a cheaper tier for summarization means this call, and reusing
        // the turn's route here would silently ignore that. On a run that named
        // no such tier the table falls back to `main`, which is the turn's
        // route again.
        let route = self.router.route(&RouteRequest {
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.clone(),
            task: TaskClass::new(TaskClass::SUMMARIZE),
            context_revision: turn.context_revision,
            message_count: candidate.messages.len() + 1 + usize::from(input_message.is_some()),
            // The compaction request asks nothing of the provider beyond text:
            // the same empty list the request below carries.
            required_capabilities: Vec::new(),
            budget: summary_budget(turn.model_budget()),
        });

        // The fixed parts of the summary request: the prompt, the in-flight
        // input shown as context, and the closing instruction. They are always
        // sent, so the room left for the candidate is what remains after them.
        let prompt_message =
            ModelMessage::new(MessageRole::System, vec![ContentBlock::text(COMPACTION_PROMPT)]);
        let instruction_message =
            ModelMessage::new(MessageRole::User, vec![ContentBlock::text(COMPACTION_INSTRUCTION)]);
        let fixed_tokens = estimate_message(&prompt_message)
            + input_message.as_ref().map(estimate_message).unwrap_or_default()
            + estimate_message(&instruction_message);

        // A summary routed to the fast tier is answered by a different model
        // with its own window, depths, and cap. Fitting the candidate to the
        // main route's million-token working set would send the fast model a
        // request it cannot accept. The frozen summary budget is the number
        // that request must fit; everything else uses the turn's own budget.
        let (mut request_route, mut route_fallback, mut profile_context, mut profile_effort) =
            budget::summary_route_selection(turn, route);
        let original_candidate_tokens = candidate.estimated_tokens;
        if !candidate.messages.is_empty() {
            let fit = |context: ResolvedContextBudget| {
                let available =
                    (context.input_working_set_tokens as usize).saturating_sub(fixed_tokens);
                fit_leading_groups(&candidate.messages, available)
            };
            let mut fitted = profile_context.map(fit);
            // A candidate larger than the fast model's window may still fit
            // the model that wrote the history, so the turn's route is the
            // second chance before the compaction is abandoned.
            if fitted.as_ref().is_some_and(|selected| selected.is_empty())
                && request_route.tier == ModelTier::Fast
            {
                request_route = turn.route.clone();
                (profile_context, profile_effort) =
                    budget::summary_route_profile(turn, &request_route);
                fitted = profile_context.map(fit);
                if fitted.as_ref().is_some_and(|selected| !selected.is_empty()) {
                    route_fallback = Some("input_working_set");
                }
            }
            if let Some(selected) = fitted {
                if selected.is_empty() {
                    events.push(draft(
                        &ctx,
                        step_id,
                        EventType::CONTEXT_COMPACTION_FAILED,
                        serde_json::json!({
                            "attempt_id": attempt_id,
                            "error_kind": "summary_input_too_large",
                            "message": "the summary request does not fit any route's input budget",
                        }),
                        Actor::Agent,
                    ));
                    return Ok(());
                }
                // Only the selected prefix is summarized and removed; the
                // newer messages stay in the transcript as ordinary history.
                candidate.messages = selected;
                candidate.estimated_tokens = candidate.messages.iter().map(estimate_message).sum();
                candidate.retained_tokens = candidate
                    .retained_tokens
                    .saturating_add(original_candidate_tokens)
                    .saturating_sub(candidate.estimated_tokens);
            }
        }

        let mut refs = Vec::with_capacity(candidate.messages.len());
        for message in &candidate.messages {
            refs.push(serde_json::json!({
                "message_uid": message.message_uid.as_str(),
                "role": message.role.as_str(),
                "hash": digest::json(&serde_json::to_value(message)?),
            }));
        }
        let summarized_hash = digest::json(&serde_json::Value::Array(refs.clone()));

        let mut started = serde_json::json!({
            "attempt_id": attempt_id,
            "provider": self.model.provider_id(),
            "route": serde_json::to_value(&request_route)?,
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
        });
        if let Some(fallback) = route_fallback {
            started["route_fallback"] = serde_json::json!(fallback);
        }
        events.push(draft(
            &ctx,
            step_id,
            EventType::CONTEXT_COMPACTION_STARTED,
            started,
            Actor::Agent,
        ));

        let mut request_messages = Vec::with_capacity(candidate.messages.len() + 3);
        request_messages.push(prompt_message);
        request_messages.extend(input_message);
        request_messages.extend(candidate.messages.iter().cloned());
        request_messages.push(instruction_message);
        // The request carries the profile's own cap and depth, not the turn's:
        // a depth the fast model never declared must not be sent to it.
        let mut request_budget = turn.model_budget();
        if let Some(context) = profile_context {
            request_budget.max_tokens = Some(context.completion_cap_tokens);
        }
        request_budget.reasoning_effort = profile_effort;
        let request = ModelRequest {
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.clone(),
            step_id: step_id.clone(),
            context_revision: turn.context_revision.as_u64(),
            messages: request_messages,
            capabilities: Vec::new(),
            response_format: None,
            model_route: Some(request_route),
            budget: request_budget,
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
        let (text, redaction) = self.redactor.redact(&response.text_content());
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
        let ledger = Ledger::build(&candidate.messages, &turn.tool_sets.writes);
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
            // Who wrote this text, and what was tried first: a summary is
            // either the turn-start strategy or the settlement's fallback, and
            // a reader can tell the two apart without reconstructing the turn.
            "strategy": "summary",
            "fallback": fallback,
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
        if let Some(fallback) = route_fallback {
            payload["route_fallback"] = serde_json::json!(fallback);
        }
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
        if redaction.redacted || !redaction.warnings.is_empty() {
            payload["redaction"] = serde_json::json!({
                "redacted": redaction.redacted,
                "patterns": redaction.patterns,
                "warnings": redaction.warnings,
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
    /// The tool sets this turn's frozen manifest declares, by side effect.
    /// Rehydration reads the writes to tell "this call changed a file" from
    /// "this call only read one", and the task layer reads all four to say
    /// what the turn's calls mean before the model makes one.
    tool_sets: ToolSets,
    /// The signature (tool and arguments) of the last call that ran, and how
    /// many times in a row it has run unchanged. The repeat gate fires on the
    /// third, not the second: the second may be the one retry a transient
    /// failure deserves.
    last_signature: Option<String>,
    identical_streak: u32,
    /// Whether the repeat gate already refused once for this streak.
    repeat_refused: bool,
    /// The highest budget checkpoint (50, 75, 90 per cent) this turn has
    /// recorded. A checkpoint is written once per turn, not once per step that
    /// sits above it: the demand to write the state down is a request, and
    /// repeating it every step would make it the loudest thing in the request.
    checkpoint_written: u32,
    /// How many mid-turn compactions this turn has attempted. A failed attempt
    /// counts (see `MAX_IN_FLIGHT_COMPACTIONS`), so this is a record of spent
    /// attempts, not of successes.
    in_flight_compactions: u32,
    /// Which message was newest when the last mid-turn attempt was made. A
    /// further attempt waits until new traffic has arrived — the model has
    /// acted since the group that was too large — so the group it could not
    /// summarize on the first pass is summarized only after it has been used.
    in_flight_newest: Option<String>,
    /// The provider's own input count for the last request, when it reported
    /// one. Settlement falls back to it when the estimate is too kind: it is
    /// the only measure that cannot be wrong about what the window held.
    observed_input_tokens: Option<u64>,
    /// The judgment record this turn is building on, as of the turn's opening.
    /// Every accepted `judgment.update` replaces it with the merged record, so
    /// the task layer the next step reads is the one the log now holds.
    judgment: Option<Judgment>,
    /// The posture this turn runs in. It opens from the situation and the
    /// reading; a real failure or a steering correction can switch it mid-turn.
    /// Every action and gate cites the stance node current at that moment.
    mode: Mode,
    /// Mints this turn's reason-node ids. One counter per turn, so an id is
    /// stable across a rebuild and unique in the session.
    reason: Recorder,
    /// The stance node the next action hangs from. Replaced when the posture
    /// switches, so the chain shows which posture a later call was made under.
    stance_node: String,
    /// What the current posture asks the next step to do, when it changed
    /// mid-turn. It goes into the task layer, so the switch is visible to the
    /// model as well as to a reader.
    stance_note: Option<String>,
    /// Whether the posture changed after the turn opened. The verdict cites the
    /// stance node only when it did, so a switch is never an orphan root.
    stance_switched: bool,
    /// The plan the task layer shows: the opening plan, replaced in place when
    /// `plan.update` lands. Kept here so a later rewrite (a stance switch) does
    /// not fall back to the plan the turn opened with.
    plan_view: Option<Plan>,
    /// The work state this turn shows at the tail of every request: opened from
    /// the caller's fold, replaced when `state.update` lands.
    work: Option<WorkState>,
    /// What this turn has learned, step by step: the digests of every distinct
    /// (call, answer) pair and the streak that has produced none. `stall` says
    /// why a turn that keeps answering itself is stopped rather than left to
    /// spend its step budget.
    progress: stall::Progress,
    /// Edits to each path since this turn's last passing check. `convergence`
    /// says why a turn that keeps rewriting one file is told to verify it.
    convergence: convergence::Edits,
    /// The capability gaps this turn has hit. One may be a call the model can
    /// replace; the second is a capability the backend cannot provide, and the
    /// turn stops instead of detouring around it.
    capability_gaps: capability::CapabilityGaps,
    /// The profile's completion criterion, when it named one. `completion`
    /// says why a run whose task is already done is ended rather than left to
    /// finish its step budget.
    completion: Option<agent_protocol::profile::CompletionSpec>,
    /// The latest observation node, so the answer can cite what it rests on.
    reason_last_observation: Option<String>,
    /// Whether the kernel's reading named a checkable criterion, so a completed
    /// check can satisfy it.
    criterion_expects_check: bool,
    /// Whether such a check has completed since the turn opened.
    criterion_observed: bool,
    /// The observation node of the check that satisfied the criterion. The
    /// verdict cites this rather than whichever call happened to be last: "the
    /// answer rests on the check" must point at the check.
    criterion_observation: Option<String>,
    /// The step ordinal within this turn, for the chain's own numbering.
    step_index: u32,
    /// Calls dispatched so far in this turn. The verdict reads it to tell an
    /// action that happened from an action the turn only talked about.
    dispatched: u32,
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
    /// The run's budget, from the caller's facts. A harness with no facts gets
    /// the default, where every limit but the step count is unset.
    budget: RunBudget,
    /// When the run's wall clock is spent. `None` when no limit is configured.
    /// Monotonic, because it times a limit and is not a timestamp.
    deadline: Option<std::time::Instant>,
    /// Tool calls this turn has dispatched. The run's `max_tool_calls` is
    /// counted here, where the call is; the Scheduler cannot see inside a step.
    tool_calls_made: u32,
    /// The tokens this run has spent, cumulative across its steps.
    ///
    /// The loop is what sees each response's usage, so the run's
    /// `max_total_tokens` is checked here, at the step boundary — before the
    /// next request is assembled. It rides every `StepOutcome` so the caller
    /// can write the terminal payload without re-deriving the log.
    spent: Spent,
    /// Where the facts that must precede a side effect become durable.
    ///
    /// A step's events travel back to the caller and are appended after the
    /// step, which is too late for the two facts a tool call needs: the step
    /// that opened and the call that is about to run. Those go to the log here,
    /// before the gateway is asked to do anything.
    journal: Journal,
}

impl LoopTurn {
    /// The provider budget every request this turn makes carries.
    ///
    /// `max_output_tokens` becomes the provider's `max_tokens`; a run that
    /// configured none sends none, and the provider's own default applies. It is
    /// deliberately not the window's reservation: that number says how much of
    /// the working set to leave for an answer, and using it as the completion
    /// cap is what ended real answers at 2,000 tokens on 2026-10-30 (CONTRACT
    /// §7 #131). The wall clock is what remains of the run's limit, so a request
    /// will not be given longer than the run has left — zero or less is sent as
    /// no limit: the step boundary already refuses to start a request past the
    /// deadline, and a round-trip value of 0 is a different instruction in some
    /// adapters.
    ///
    /// `reasoning_effort` is the depth assembly resolved — the profile's choice,
    /// the model's declared default, or `high`. It is a separate control from
    /// `max_tokens`: effort says how deeply to think, the cap only bounds how
    /// long the whole completion may run, reasoning included.
    fn model_budget(&self) -> ModelBudget {
        ModelBudget {
            max_tokens: self.budget.max_output_tokens,
            reasoning_effort: self.budget.reasoning_effort,
            wall_clock_ms: self.deadline.and_then(|deadline| {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                (left.as_millis() > 0).then_some(left.as_millis() as u64)
            }),
            ..ModelBudget::default()
        }
    }
}

/// The advice this turn's task layer carries, from its frozen tool sets.
fn tool_advice(turn: &LoopTurn) -> ToolAdvice {
    ToolAdvice::new(&turn.tool_sets)
}

/// Enter a posture that differs from the one the turn opened in.
///
/// The mode changes, the note the task layer carries is derived from the reason
/// for the switch, and the turn remembers that it switched so the verdict can
/// cite the new stance. The node itself is minted by the caller, which is the
/// only place that knows what evidence caused the switch.
fn enter_stance(turn: &mut LoopTurn, stance: Stance, because: &str) {
    turn.mode = reason_record::switched_mode(turn.mode, stance);
    let note =
        (stance == Stance::Repair).then(|| repair_note(turn.pending_failure.as_ref(), because));
    turn.stance_note = note;
    turn.stance_switched = true;
}

/// Rewrite the task layer this turn already placed, in place.
///
/// The layer describes the plan and the posture, and both can move while the
/// turn runs; the next step must read the state the log now holds, not the one
/// the turn opened with. The plan is `plan_view` rather than `ctx.plan` for the
/// same reason: a plan written mid-turn would otherwise be reverted by a later
/// stance switch.
fn refresh_task_layer(turn: &mut LoopTurn, ledger: bool) {
    let Some(facts) = turn.ctx.state.as_ref() else { return };
    let advice = tool_advice(turn);
    let text = task_text(
        &turn.ctx.user_input,
        facts,
        TaskLedger {
            plan: turn.plan_view.as_ref(),
            workers: &turn.ctx.workers,
            work: turn.work.as_ref(),
        },
        turn.judgment.as_ref(),
        turn.stale.as_ref(),
        ledger.then_some(&advice),
        turn.stance_note.as_deref(),
    );
    if let Some(layers) = turn.context_layers.as_mut() {
        layers.task = text.clone();
    }
    let uid = format!("msg/task/{}", turn.ctx.turn_id);
    if let Some(message) =
        turn.messages.iter_mut().find(|message| message.message_uid.as_str() == uid)
    {
        *message = ModelMessage::task_for_turn(&turn.ctx.turn_id, text);
    }
}
use agent_loop_impl::{redacted_proposal, step_failed};
mod agent_loop_impl;

#[cfg(test)]
mod tests;
