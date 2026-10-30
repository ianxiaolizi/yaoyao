//! The reasoning chain: why each step of a turn happened.
//!
//! A node is a typed decision with a claim, a named rule, and the bases it was
//! derived from — earlier nodes, durable events, or stated facts. The chain is
//! written by the kernel around the model's choices, not by the model: a turn's
//! situation, the kernel's reading of the input, the selected stance, every
//! dispatched call, every observation, every refusal and the final answer are
//! nodes. The model's own choices are marked `source: model` and carry the
//! response event they came from, so the record says which part was derived and
//! which part was decided.
//!
//! Two rules make the chain auditable rather than decorative:
//!
//! - **No node without a rule.** Every node names an entry in [`RULES`], whose
//!   text states the derivation. A node with no rule is a claim with no reason.
//! - **No node without a basis.** At least one earlier node, durable event, or
//!   stated fact. `why` walks those edges backwards; a root that is not the
//!   turn's own input is a bug the graph check exposes.
//!
//! The chain never enters a model request. It is recorded for people and for
//! projections, so it costs no context and cannot be talked around by the
//! model.
//!
//! What this module deliberately does not do: it does not read the model's
//! prose, it does not decide what the model should say, and it does not claim
//! that the kernel understood the request. It records the derivation the kernel
//! can prove and marks the rest as the model's.

use serde::{Deserialize, Serialize};

/// The event that carries one reason node.
pub const EVENT_TYPE: &str = "reason/recorded";

/// Longest claim kept for one node.
pub const MAX_CLAIM_CHARS: usize = 240;

/// Most bases one node may carry.
pub const MAX_BASIS: usize = 6;

/// Longest single basis text kept.
pub const MAX_BASIS_CHARS: usize = 240;

/// What kind of decision a node records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonKind {
    /// The facts around the turn when it opened.
    Situation,
    /// The kernel's reading of what the input is asking for.
    Reading,
    /// The posture selected for this turn.
    Stance,
    /// A call the model proposed and the kernel dispatched.
    Action,
    /// What the world returned.
    Observation,
    /// A refusal the kernel made, and the rule that fired.
    Gate,
    /// The final answer and the stop rule.
    Verdict,
}

impl ReasonKind {
    /// The wire word, matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Situation => "situation",
            Self::Reading => "reading",
            Self::Stance => "stance",
            Self::Action => "action",
            Self::Observation => "observation",
            Self::Gate => "gate",
            Self::Verdict => "verdict",
        }
    }
}

/// Who decided the node.
///
/// The distinction is the honesty of the chain: `Kernel` nodes are derived by a
/// named rule from facts, `Model` nodes are choices the kernel recorded but did
/// not make, `World` nodes are tool results, and `User` nodes would be the
/// person's own words if a node ever needed to carry them directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonSource {
    Kernel,
    Model,
    World,
    User,
}

impl ReasonSource {
    /// The wire word, matching the serde representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kernel => "kernel",
            Self::Model => "model",
            Self::World => "world",
            Self::User => "user",
        }
    }
}

/// One edge of the derivation: an earlier node, a durable event, or a fact.
///
/// Exactly one field is set. A node that cites two different events in one edge
/// would make "which of these caused it" unanswerable, so the shape forbids it
/// rather than trusting every producer to mean one thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Basis {
    /// The id of an earlier reason node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The id of a durable event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// A stated fact, in one bounded line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact: Option<String>,
}

impl Basis {
    /// An edge to an earlier node.
    pub fn node(id: impl Into<String>) -> Self {
        Self { node: Some(id.into()), event: None, fact: None }
    }

    /// An edge to a durable event.
    pub fn event(id: impl Into<String>) -> Self {
        Self { node: None, event: Some(id.into()), fact: None }
    }

    /// An edge to a stated fact.
    pub fn fact(text: impl Into<String>) -> Self {
        Self { node: None, event: None, fact: Some(text.into()) }
    }

    /// The one reference this edge carries, in the order node, event, fact.
    fn reference(&self) -> Option<&str> {
        self.node
            .as_deref()
            .or(self.event.as_deref())
            .or(self.fact.as_deref())
            .filter(|text| !text.trim().is_empty())
    }

    /// Whether exactly one non-empty reference is set.
    fn is_one(&self) -> bool {
        let set = [&self.node, &self.event, &self.fact]
            .into_iter()
            .filter(|value| value.as_deref().is_some_and(|text| !text.trim().is_empty()))
            .count();
        set == 1
    }
}

/// One decision in a turn's reasoning chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasonNode {
    /// Stable within the session, e.g. `r/<turn>/3`.
    pub id: String,
    /// The turn this node belongs to.
    pub turn_id: String,
    /// The step this node belongs to; `0` is turn-level.
    pub step: u32,
    pub kind: ReasonKind,
    /// One line: what was decided or observed.
    pub claim: String,
    /// What it was derived from.
    pub basis: Vec<Basis>,
    /// The id of the rule in [`RULES`] that produced it.
    pub rule: String,
    pub source: ReasonSource,
}

impl ReasonNode {
    /// The rule this node names, if the build knows it.
    pub fn known_rule(&self) -> Option<&'static Rule> {
        rule(&self.rule)
    }

    /// Check the shape every node must have.
    ///
    /// This is the invariant behind "every step can be traced": a node with no
    /// basis or no known rule cannot be explained and must not enter the log.
    pub fn validate(&self) -> Result<(), ReasonError> {
        if self.id.trim().is_empty() {
            return Err(ReasonError::Invalid(String::from("a reason node needs an id")));
        }
        if self.turn_id.trim().is_empty() {
            return Err(ReasonError::Invalid(String::from(
                "a reason node needs the turn it belongs to",
            )));
        }
        if self.claim.trim().is_empty() {
            return Err(ReasonError::Invalid(format!("{} has no claim", self.id)));
        }
        if bounded_chars(&self.claim, MAX_CLAIM_CHARS) {
            return Err(ReasonError::Invalid(format!(
                "{} claim is longer than {MAX_CLAIM_CHARS} characters",
                self.id
            )));
        }
        if self.basis.is_empty() {
            return Err(ReasonError::Invalid(format!(
                "{} has no basis; a step with no reason is not traceable",
                self.id
            )));
        }
        if self.basis.len() > MAX_BASIS {
            return Err(ReasonError::Invalid(format!(
                "{} has {} bases; the limit is {MAX_BASIS}",
                self.id,
                self.basis.len()
            )));
        }
        for basis in &self.basis {
            if !basis.is_one() {
                return Err(ReasonError::Invalid(format!(
                    "{} has a basis that names none or more than one thing",
                    self.id
                )));
            }
            let reference = basis.reference().unwrap_or_default();
            if bounded_chars(reference, MAX_BASIS_CHARS) {
                return Err(ReasonError::Invalid(format!(
                    "{} has a basis longer than {MAX_BASIS_CHARS} characters",
                    self.id
                )));
            }
        }
        if rule(&self.rule).is_none() {
            return Err(ReasonError::Invalid(format!(
                "{} names rule `{}`, which this build does not know",
                self.id, self.rule
            )));
        }
        Ok(())
    }
}

/// A named derivation rule with the sentence that explains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub id: &'static str,
    /// Version of the rule's meaning; a semantic change bumps it.
    pub version: u32,
    pub text: &'static str,
}

/// Every rule this build can fire.
///
/// The table is closed on purpose. A producer cannot write a free-form
/// "because": it names one of these, so the list of possible reasons is finite
/// and reviewable rather than as broad as the model's vocabulary.
pub const RULES: &[Rule] = &[
    Rule {
        id: "situation/derive",
        version: 1,
        text: "derived from the turn context, the frozen tool set and the settled history; no field is model-authored",
    },
    Rule {
        id: "reading/action-cue",
        version: 1,
        text: "the input contains an action cue (a verb that changes something, or a named target), so the kernel reads it as a request to act",
    },
    Rule {
        id: "reading/question-cue",
        version: 1,
        text: "the input asks a question and carries no action cue, so the kernel reads it as a request for an answer",
    },
    Rule {
        id: "reading/correction-cue",
        version: 1,
        text: "the input says something was wrong or must be redone, so the kernel reads it as a correction",
    },
    Rule {
        id: "reading/greeting-cue",
        version: 1,
        text: "a short input with only greeting words and no action cue, so the kernel reads it as an opening",
    },
    Rule {
        id: "reading/report-cue",
        version: 1,
        text: "the input reports a state rather than asking for a change, so the kernel reads it as a report",
    },
    Rule {
        id: "reading/no-cue",
        version: 1,
        text: "no cue in the input is decisive, so the kernel records an unclear reading instead of guessing",
    },
    Rule {
        id: "stance/repair-failure",
        version: 1,
        text: "an earlier failure is still unreconciled, so the turn opens in the repair posture",
    },
    Rule {
        id: "stance/act-guarded",
        version: 1,
        text: "the request is an action and the turn offers a call that cannot be undone, so the act posture is marked guarded; the kernel's own gates still apply",
    },
    Rule {
        id: "stance/act-request",
        version: 1,
        text: "the request is an action and nothing irreversible is offered, so the turn runs in the act posture",
    },
    Rule {
        id: "stance/answer-question",
        version: 1,
        text: "the request is a question, so the turn answers instead of acting",
    },
    Rule {
        id: "stance/answer-report",
        version: 1,
        text: "the request reports a state, so the turn answers it without opening a work loop",
    },
    Rule {
        id: "stance/chat-greeting",
        version: 1,
        text: "the input is a greeting, so the turn stays conversational and arms no work gate",
    },
    Rule {
        id: "stance/probe-unclear",
        version: 1,
        text: "the kernel could not read a request, so the turn probes for one instead of acting on a guess",
    },
    Rule {
        id: "stance/record-scheduled",
        version: 1,
        text: "a scheduled turn with no action cue reports back instead of starting work",
    },
    Rule {
        id: "stance/continuation-granted",
        version: 1,
        text: "the turn continues a call whose approval was granted, so the posture is act: what is left is the work the request asked for, not a new reading of the message",
    },
    Rule {
        id: "switch/failure-repair",
        version: 1,
        text: "a real tool failure arrived while the turn was not already repairing, so the posture changes to repair: the next move must change one variable instead of repeating the failed call",
    },
    Rule {
        id: "switch/steer-correction",
        version: 1,
        text: "the user steered a correction into the running turn, so the posture changes to repair and the next move answers the correction",
    },
    Rule {
        id: "switch/steer-request",
        version: 1,
        text: "the user steered a new request into a turn that was not acting, so the posture changes to act",
    },
    Rule {
        id: "action/dispatch",
        version: 1,
        text: "the model proposed this call under the turn's stance; the kernel dispatched it without rewriting the choice",
    },
    Rule {
        id: "observation/result",
        version: 1,
        text: "the world returned this result to the call that preceded it",
    },
    Rule {
        id: "observation/refused",
        version: 1,
        text: "the gateway refused this call before the world saw it, so the call has an outcome but no result; the refusal is a decision, not a failure the turn repairs",
    },
    Rule {
        id: "observation/awaiting-approval",
        version: 1,
        text: "the call was held for approval before it ran, so this turn ends with a decision pending; the result, if granted, belongs to the resumed turn",
    },
    Rule {
        id: "gate/stale-read",
        version: 1,
        text: "an earlier turn read a target this turn names again, and nothing has been looked at since the turn opened",
    },
    Rule {
        id: "gate/repeat-call",
        version: 1,
        text: "the same call with the same arguments has already run twice in this turn, so a third run cannot tell anything new",
    },
    Rule {
        id: "gate/failure-check",
        version: 1,
        text: "a real tool failure has not been reconciled and nothing has looked at the world since, so the turn may not answer yet",
    },
    Rule {
        id: "verdict/answer",
        version: 1,
        text: "the turn ended with the model's answer and no completed check in this turn satisfies a criterion the reading named, so the stop rests on the answer rather than on an observation",
    },
    Rule {
        id: "verdict/criterion-observed",
        version: 2,
        text: "a call that runs a command naming a check completed after the work and no real failure has happened since, so the answer rests on that observation rather than on prose",
    },
    Rule {
        id: "verdict/after-failure",
        version: 1,
        text: "the turn answered in the repair posture after a real failure, so the answer rests on the failed observation and on the posture that failure caused",
    },
    Rule {
        id: "verdict/action-unmet",
        version: 1,
        text: "the input read as an action and the turn stayed in the act posture, but no call was dispatched: the turn answered where it was expected to act",
    },
    Rule {
        id: "verdict/output-truncated",
        version: 1,
        text: "the provider stopped the response at the run's output-token limit, so what arrived is the beginning of an answer rather than the answer; the turn ends on that limit, not on the model's choice to stop",
    },
];

/// Look up a rule by id.
pub fn rule(id: &str) -> Option<&'static Rule> {
    RULES.iter().find(|rule| rule.id == id)
}

/// What the input looks like to the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechAct {
    Question,
    Action,
    Correction,
    Report,
    Greeting,
    Unclear,
}

impl SpeechAct {
    /// The word used in a claim.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Action => "action request",
            Self::Correction => "correction",
            Self::Report => "report",
            Self::Greeting => "greeting",
            Self::Unclear => "unclear",
        }
    }
}

/// The kernel's reading of one input.
///
/// This is a cue-based reading, not an understanding: it says which words moved
/// it and which rule fired, so a wrong reading is visible at the point where it
/// was made. It never claims more than the cues support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub act: SpeechAct,
    /// The request in the person's own words, bounded.
    pub demand: String,
    /// What would count as done, when the input names a check.
    pub criterion: Option<String>,
    /// The cues that decided the reading.
    pub cues: Vec<&'static str>,
}

impl Reading {
    /// The rule id this reading fired.
    pub fn rule(&self) -> &'static str {
        match self.act {
            SpeechAct::Action => "reading/action-cue",
            SpeechAct::Question => "reading/question-cue",
            SpeechAct::Correction => "reading/correction-cue",
            SpeechAct::Greeting => "reading/greeting-cue",
            SpeechAct::Report => "reading/report-cue",
            SpeechAct::Unclear => "reading/no-cue",
        }
    }

    /// The reading as the chain shows it.
    pub fn render(&self) -> String {
        let mut text = format!("kernel reading: {}.", self.act.as_str());
        if !self.demand.is_empty() {
            text.push_str(&format!(" Input: \"{}\".", truncate(&self.demand, 120)));
        }
        if let Some(criterion) = &self.criterion {
            text.push_str(&format!(" Criterion: {criterion}."));
        }
        if !self.cues.is_empty() {
            text.push_str(&format!(" Cues: {}.", self.cues.join(", ")));
        }
        truncate(&text, MAX_CLAIM_CHARS)
    }
}

/// The facts around a turn when it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Situation {
    pub input_kind: String,
    pub previous_turn: Option<String>,
    /// The decision word of the approval this turn continues, when the turn
    /// opened on a resume (`granted`, `denied`, ...). The continuation's text
    /// is often the person's own "go ahead", which reads as no cue at all; the
    /// decision is what the posture has to come from.
    pub resumed: Option<String>,
    pub queued_inputs: usize,
    pub max_steps: u32,
    pub plan_done: usize,
    pub plan_total: usize,
    pub pending_failure: Option<String>,
    pub stale_read: Option<String>,
    pub read_tools: usize,
    pub write_tools: usize,
    pub irreversible: Vec<String>,
    pub sandbox: Option<String>,
    pub network_allowed: Option<bool>,
    pub judgment_stage: Option<String>,
}

impl Situation {
    /// The situation as the chain shows it.
    pub fn render(&self) -> String {
        let mut parts = vec![format!("{} turn", self.input_kind)];
        parts.push(format!("previous turn: {}", self.previous_turn.as_deref().unwrap_or("none")));
        if let Some(resumed) = &self.resumed {
            parts.push(format!("resumed: {resumed}"));
        }
        if self.queued_inputs > 0 {
            parts.push(format!("{} queued", self.queued_inputs));
        }
        if self.plan_total > 0 {
            parts.push(format!("plan {}/{}", self.plan_done, self.plan_total));
        } else {
            parts.push(String::from("no plan"));
        }
        parts.push(format!("{} read + {} write tool(s)", self.read_tools, self.write_tools));
        if self.irreversible.is_empty() {
            parts.push(String::from("no irreversible tool"));
        } else {
            parts.push(format!("irreversible: {}", truncate(&self.irreversible.join(", "), 80)));
        }
        parts.push(match &self.pending_failure {
            Some(tool) => format!("unreconciled failure: {tool}"),
            None => String::from("no pending failure"),
        });
        if let Some(stale) = &self.stale_read {
            parts.push(format!("stale read: {stale}"));
        }
        if let Some(sandbox) = &self.sandbox {
            parts.push(format!("sandbox: {sandbox}"));
        }
        if self.network_allowed == Some(false) {
            parts.push(String::from("network denied"));
        }
        if let Some(stage) = &self.judgment_stage {
            parts.push(format!("judgment: {stage}"));
        }
        truncate(&format!("situation: {}.", parts.join("; ")), MAX_CLAIM_CHARS)
    }
}

/// The posture a turn runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stance {
    /// Answer what was asked; no work loop.
    Answer,
    /// Change the world, then check.
    Act,
    /// Ask or look before acting; the request is not readable yet.
    Probe,
    /// Recover from a failure: one changed move, then reconcile.
    Repair,
    /// Conversational; no gates, no advice.
    Chat,
    /// Scheduled report; no work unless the input asks for it.
    Record,
}

impl Stance {
    /// The word used in a claim.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Act => "act",
            Self::Probe => "probe",
            Self::Repair => "repair",
            Self::Chat => "chat",
            Self::Record => "record",
        }
    }
}

/// A check the turn arms is the kernel's floor, not the stance's choice: the
/// deterministic gates stay in force whatever the posture. The mode therefore
/// records only what the posture *is*, not a permission to disarm anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub stance: Stance,
    /// Whether the turn offers a call that cannot be undone, so the write-back
    /// advice applies. Kept beside the stance because "act" alone does not say
    /// whether the branch advice is relevant.
    pub guarded: bool,
}

impl Mode {
    fn new(stance: Stance, guarded: bool) -> Self {
        Self { stance, guarded }
    }
}

/// The mode a turn carries after a mid-turn switch.
///
/// Switching changes the posture, not the boundary: whether an irreversible call
/// is offered is a fact of the turn's tool set, so `guarded` is carried over
/// from the mode the turn opened in.
pub fn switched_mode(previous: Mode, stance: Stance) -> Mode {
    Mode::new(stance, previous.guarded)
}

/// The stance selected for a turn and the rule that selected it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub stance: Stance,
    pub rule: &'static str,
    pub mode: Mode,
}

impl Selection {
    fn new(stance: Stance, rule: &'static str, guarded: bool) -> Self {
        Self { stance, rule, mode: Mode::new(stance, guarded) }
    }

    /// The stance as the chain shows it.
    pub fn render(&self) -> String {
        let mut text = format!("stance: {}", self.stance.as_str());
        if self.mode.guarded {
            text.push_str(" (guarded: an irreversible call is offered)");
        }
        truncate(&text, MAX_CLAIM_CHARS)
    }
}

/// Read one input from cues.
///
/// The precedence is deliberate: a correction outranks a new request (the
/// person is telling you the last reading was wrong), a greeting only counts
/// when the input is short and carries no action, and an unclear input stays
/// unclear rather than being rounded to "action".
pub fn read(text: &str) -> Reading {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    let mut cues = Vec::new();

    // "不是" alone is not a correction: "这不是 bug" corrects the world, not the
    // assistant. Only the forms that point at previous work are cues.
    let corrects = has_any(
        &lower,
        &["不对", "错了", "不是这样", "不是这个", "不是那样", "重新", "又错", "没成功", "还是不行"],
    ) || has_any(&lower, &["wrong", "incorrect", "redo", "not what"]);
    // A clause that opens with a negation takes its action cues with it:
    // "不要改任何文件" is a prohibition, not an instruction to change a file.
    // Without this, a request that forbids an action reads as one.
    let scan = scan_actions(&lower);
    let acts = scan.active;
    // "test" names a check as often as it asks for one. When the input also
    // asks a question and no verb accompanies it, the question is what it is:
    // "why does tests/test_calc.py expect this?" names a file, not an action.
    let verbs = scan.verb;
    if scan.negated {
        cues.push("negation");
    }
    let asks = has_any(&lower, QUESTION_WORDS) || trimmed.ends_with('?') || trimmed.ends_with('？');
    let greets = has_any(&lower, GREETING_WORDS);
    let reports = reports_state(&lower, acts, asks);
    let has_path = has_any(&lower, PATH_WORDS);
    let has_test = has_any(&lower, TEST_WORDS);

    let act = if corrects {
        cues.push("correction");
        if acts {
            cues.push("action");
        }
        SpeechAct::Correction
    } else if greets && !acts && trimmed.chars().count() <= 24 {
        cues.push("greeting");
        SpeechAct::Greeting
    } else if acts && !(asks && !verbs) {
        cues.push("action");
        if has_path {
            cues.push("path");
        }
        if has_test {
            cues.push("test");
        }
        SpeechAct::Action
    } else if asks {
        cues.push("question");
        SpeechAct::Question
    } else if reports {
        cues.push("report");
        SpeechAct::Report
    } else {
        SpeechAct::Unclear
    };

    // A criterion is something the turn could satisfy by acting. An input that
    // only asks about a test names the test without asking for it to run, so it
    // gets no criterion: otherwise reading `tests/…` to answer a question would
    // look like the check the request named.
    let criterion = if has_test && matches!(act, SpeechAct::Action | SpeechAct::Correction) {
        Some(String::from("the check or test named in the input passes"))
    } else if act == SpeechAct::Action && has_path {
        Some(String::from("the answer rests on the named target"))
    } else {
        None
    };

    Reading { act, demand: truncate(trimmed, 160), criterion, cues }
}

/// Select the posture for a turn.
pub fn select(situation: &Situation, reading: &Reading) -> Selection {
    let failure = situation.pending_failure.is_some()
        || (situation.previous_turn.as_deref() == Some("failed")
            && matches!(reading.act, SpeechAct::Action | SpeechAct::Unclear));
    if failure {
        return Selection::new(
            Stance::Repair,
            "stance/repair-failure",
            !situation.irreversible.is_empty(),
        );
    }
    // A granted approval is context the text does not carry: the continuation
    // is usually the person's own "go ahead", which names no action and no
    // question. What is left after the call ran is the work the request asked
    // for, so the posture is act whatever the words look like.
    if situation.resumed.as_deref() == Some("granted") {
        return Selection::new(
            Stance::Act,
            "stance/continuation-granted",
            !situation.irreversible.is_empty(),
        );
    }
    match reading.act {
        SpeechAct::Greeting => Selection::new(Stance::Chat, "stance/chat-greeting", false),
        SpeechAct::Action | SpeechAct::Correction => {
            let guarded = !situation.irreversible.is_empty();
            let rule = if guarded { "stance/act-guarded" } else { "stance/act-request" };
            Selection::new(Stance::Act, rule, guarded)
        }
        SpeechAct::Question => Selection::new(Stance::Answer, "stance/answer-question", false),
        SpeechAct::Report => Selection::new(Stance::Answer, "stance/answer-report", false),
        SpeechAct::Unclear => {
            if situation.input_kind == "scheduled" {
                Selection::new(Stance::Record, "stance/record-scheduled", false)
            } else {
                Selection::new(Stance::Probe, "stance/probe-unclear", false)
            }
        }
    }
}

/// Mints node ids for one turn.
///
/// The counter lives with the turn, so ids are stable (`r/<turn>/0`, `1`, ...)
/// and two turns can never mint the same one.
#[derive(Debug, Clone)]
pub struct Recorder {
    turn_id: String,
    next: u32,
}

impl Recorder {
    /// A recorder for one turn.
    pub fn new(turn_id: &str) -> Self {
        Self { turn_id: turn_id.to_string(), next: 0 }
    }

    /// Record one node.
    pub fn node(
        &mut self,
        kind: ReasonKind,
        step: u32,
        claim: impl AsRef<str>,
        basis: Vec<Basis>,
        rule: &str,
        source: ReasonSource,
    ) -> ReasonNode {
        let id = format!("r/{}/{}", self.turn_id, self.next);
        self.next += 1;
        ReasonNode {
            id,
            turn_id: self.turn_id.clone(),
            step,
            kind,
            claim: truncate(claim.as_ref(), MAX_CLAIM_CHARS),
            basis: basis.into_iter().take(MAX_BASIS).collect(),
            rule: rule.to_string(),
            source,
        }
    }
}

const ACTION_WORDS: &[&str] = &[
    "改",
    "写",
    "删",
    "跑",
    "执行",
    "创建",
    "新建",
    "加",
    "换",
    "修",
    "移",
    "复制",
    "提交",
    "运行",
    "安装",
    "生成",
    "读",
    // "查" alone is also inside 检查, which is how questions about a check
    // turned into action cues. The verb needs a complement to count.
    "查看",
    "查一下",
    "查查",
    "搜",
    "打开",
    "弄",
    "重命名",
    // The verbs real Chinese requests used and the first table missed: an
    // overwrite, a value adjustment. A bare "调" is not listed — it is also a
    // noun in 调查/调用 — so the direction is required.
    "覆盖",
    "调整",
    "调长",
    "调大",
    "调小",
    "替换",
    "test",
    "fix",
    "add",
    "remove",
    "run",
    "write",
    "delete",
    "update",
    "rename",
    "create",
    "install",
    "edit",
    "build",
    "read ",
    "show ",
    "list ",
    "find ",
    "search ",
    "open ",
];

const QUESTION_WORDS: &[&str] = &[
    "吗",
    "呢",
    "为什么",
    "怎么",
    "什么",
    "哪里",
    "哪个",
    "多少",
    "是否",
    "how",
    "what",
    "why",
    "which",
    "where",
    "when",
    "?",
];

const GREETING_WORDS: &[&str] = &["你好", "在吗", "在么", "嗨", "早上好", "hi", "hello", "hey"];

const PATH_WORDS: &[&str] = &[
    ".rs", ".toml", ".md", ".txt", ".json", ".yaml", ".yml", "/", "\\", "src", "crates", "apps",
    "文件",
];

const TEST_WORDS: &[&str] = &["测试", "test", "cargo test", "检查", "验证", "verify", "check"];

fn has_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// The punctuation that ends a clause. Chinese negation scopes to its clause,
/// so this is the window, not a character count: a fixed window would either
/// miss "不要改任何文件" (seven characters in) or reach into the next request.
const CLAUSE_BREAKS: &[char] = &['，', '。', '；', '！', '？', ',', ';', '!', '?', '\n', '：', ':'];

const NEGATION_WORDS: &[&str] = &[
    "不要", "不用", "不必", "无需", "不需", "请勿", "不准", "禁止", "不能", "不该", "don't",
    "do not", "without",
];

/// What one input's action cues amount to, clause by clause.
///
/// Chinese negation scopes to the clause it opens, so "别老断，帮我调长一些"
/// still carries a request: the prohibition governs the first half and the
/// action lives in the second. A clause is counted only when it names an
/// action; a negation elsewhere in the sentence does not reach it.
struct ActionScan {
    /// A clause named an action and did not negate it.
    active: bool,
    /// A clause named an action and negated it — the cue did not count.
    negated: bool,
    /// The active cues include a verb, not only a check's name. "test" is a
    /// noun in "tests/test_calc.py"; a verb is what makes a request an action
    /// even when it also asks something.
    verb: bool,
}

fn scan_actions(text: &str) -> ActionScan {
    let mut scan = ActionScan { active: false, negated: false, verb: false };
    for clause in text.split(CLAUSE_BREAKS) {
        if !has_any(clause, ACTION_WORDS) {
            continue;
        }
        if clause_negates(clause) {
            scan.negated = true;
            continue;
        }
        scan.active = true;
        if clause_has_verb(clause) {
            scan.verb = true;
        }
    }
    scan
}

fn clause_has_verb(clause: &str) -> bool {
    ACTION_WORDS.iter().any(|word| *word != "test" && clause.contains(word))
}

fn clause_negates(clause: &str) -> bool {
    has_any(clause, NEGATION_WORDS)
        || clause.trim_start().starts_with('别')
        || clause.split_whitespace().skip(1).any(|word| word.starts_with('别'))
}

fn reports_state(text: &str, acts: bool, asks: bool) -> bool {
    !acts && !asks && (text.starts_with('我') && has_any(text, &["完成", "做了", "已经", "好了"]))
}

fn truncate(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let mut kept: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        kept.pop();
        kept.push('…');
    }
    kept
}

fn bounded_chars(text: &str, max: usize) -> bool {
    text.chars().count() > max
}

/// A node that cannot enter the log, with the reason a producer can act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReasonError {
    #[error("{0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_node() -> ReasonNode {
        Recorder::new("t1").node(
            ReasonKind::Situation,
            0,
            "situation: user turn",
            vec![Basis::fact("the turn opened")],
            "situation/derive",
            ReasonSource::Kernel,
        )
    }

    #[test]
    fn an_action_request_is_read_from_its_cues() {
        let reading = read("把 report.txt 里的旧数据换成新数据，然后跑一下测试");
        assert_eq!(reading.act, SpeechAct::Action);
        assert!(reading.cues.contains(&"path"));
        assert!(reading.cues.contains(&"test"));
        assert!(reading.criterion.is_some());
        assert_eq!(reading.rule(), "reading/action-cue");
    }

    #[test]
    fn a_question_is_read_as_a_question_when_no_action_cue_is_present() {
        let reading = read("report.txt 里是什么？");
        assert_eq!(reading.act, SpeechAct::Question);
        assert_eq!(reading.rule(), "reading/question-cue");
    }

    /// The three inputs the 2026-10-24 real-use round misread, kept verbatim.
    /// The kernel has to read them the way they were asked, or the chain
    /// records an understanding nobody had.
    #[test]
    fn real_chinese_requests_are_read_the_way_they_were_asked() {
        let overwrite = read("用一条 shell 命令把 app.log 的内容整体覆盖为一行 `rotated`。");
        assert_eq!(overwrite.act, SpeechAct::Action, "{overwrite:?}");

        let adjust = read("config.yaml 里的超时总是断，帮我调长一些，别老断。");
        assert_eq!(adjust.act, SpeechAct::Action, "{adjust:?}");

        let question = read(
            "这不是 bug，是预期行为。请解释一下 tests/test_calc.py 里 test_rounds_half_up \
             为什么期望 2.68，不要改任何文件。",
        );
        assert_eq!(question.act, SpeechAct::Question, "{question:?}");
        assert!(question.cues.contains(&"negation"), "{question:?}");
        assert!(question.criterion.is_none(), "a question about a test does not ask for it to run");
    }

    /// A negation governs its clause, not the sentence: the second half is a
    /// request even though the first half forbids something.
    #[test]
    fn a_negated_clause_does_not_swallow_the_request_beside_it() {
        assert_eq!(read("别老断，帮我调长一些。").act, SpeechAct::Action);
        assert_eq!(read("不要改 tests/ 下的文件。").act, SpeechAct::Unclear);
    }

    /// "不是" corrects the world in "这不是 bug"; only the forms that point at
    /// previous work are corrections. A bare "不是" used to outrank a question.
    #[test]
    fn a_statement_about_the_world_is_not_a_correction() {
        assert_ne!(read("这不是 bug，是预期行为。").act, SpeechAct::Correction);
        let reading = read("不对，你改错了，重新写一遍");
        assert_eq!(reading.act, SpeechAct::Correction);
    }

    #[test]
    fn a_correction_outranks_the_request_it_corrects() {
        let reading = read("不对，你改错了，重新写一遍");
        assert_eq!(reading.act, SpeechAct::Correction);
    }

    #[test]
    fn an_english_read_request_is_an_action_not_an_unclear_input() {
        let reading = read("read notes.txt and tell me the word");
        assert_eq!(reading.act, SpeechAct::Action);
        assert!(reading.cues.contains(&"path"));
    }

    #[test]
    fn a_short_greeting_is_chat_and_a_long_one_is_not() {
        assert_eq!(read("你好").act, SpeechAct::Greeting);
        assert_eq!(read("你好，帮我把 report.txt 里的数据换成新的").act, SpeechAct::Action);
    }

    #[test]
    fn an_input_with_no_cue_stays_unclear() {
        let reading = read("嗯");
        assert_eq!(reading.act, SpeechAct::Unclear);
        assert_eq!(reading.rule(), "reading/no-cue");
    }

    /// One line per way a person asks for something: a request, a prohibition,
    /// a question that names a check, a correction, a greeting. The table is
    /// the contract the cue lists keep — each case used to be either a real
    /// misreading, a risk beside one, or the sentence that keeps a fix honest.
    /// A criterion is only claimed for an action or a correction.
    #[test]
    fn chinese_readings_hold_across_request_negation_and_question() {
        struct Case {
            text: &'static str,
            act: SpeechAct,
            criterion: bool,
            cue: Option<&'static str>,
        }
        let cases = [
            Case {
                text: "把 app.log 的内容整体覆盖为一行 `rotated`。",
                act: SpeechAct::Action,
                criterion: false,
                cue: Some("action"),
            },
            Case {
                text: "跑一下 cargo test，把结果贴给我。",
                act: SpeechAct::Action,
                criterion: true,
                cue: Some("test"),
            },
            Case {
                text: "删掉 report.txt。",
                act: SpeechAct::Action,
                criterion: true,
                cue: Some("path"),
            },
            Case {
                text: "读 notes.txt 并告诉我里面的单词。",
                act: SpeechAct::Action,
                criterion: true,
                cue: Some("path"),
            },
            Case {
                text: "调长 config.yaml 里的超时。",
                act: SpeechAct::Action,
                criterion: true,
                cue: Some("path"),
            },
            Case {
                text: "update notes.txt with the new count",
                act: SpeechAct::Action,
                criterion: true,
                cue: Some("path"),
            },
            Case {
                text: "不要改 tests/ 下的文件。",
                act: SpeechAct::Unclear,
                criterion: false,
                cue: Some("negation"),
            },
            Case {
                text: "别删掉 report.txt，先备份一份再删。",
                act: SpeechAct::Action,
                criterion: true,
                cue: Some("negation"),
            },
            Case {
                text: "report.txt 里是什么？不要改它。",
                act: SpeechAct::Question,
                criterion: false,
                cue: Some("negation"),
            },
            Case {
                text: "tests/test_calc.py 为什么期望 2.68？",
                act: SpeechAct::Question,
                criterion: false,
                cue: Some("question"),
            },
            Case {
                text: "不对，刚才那次改错了，重新跑一遍测试。",
                act: SpeechAct::Correction,
                criterion: true,
                cue: Some("correction"),
            },
            Case {
                text: "你好",
                act: SpeechAct::Greeting,
                criterion: false,
                cue: Some("greeting"),
            },
            Case {
                text: "嗯，再看看。", act: SpeechAct::Unclear, criterion: false, cue: None
            },
        ];
        for case in cases {
            let reading = read(case.text);
            assert_eq!(reading.act, case.act, "{:?}", case.text);
            assert_eq!(
                reading.criterion.is_some(),
                case.criterion,
                "criterion for {:?}: {reading:?}",
                case.text
            );
            if let Some(cue) = case.cue {
                assert!(reading.cues.contains(&cue), "{:?}: {:?}", case.text, reading.cues);
            }
        }
    }

    fn a_situation() -> Situation {
        Situation {
            input_kind: String::from("user"),
            previous_turn: None,
            resumed: None,
            queued_inputs: 0,
            max_steps: 12,
            plan_done: 0,
            plan_total: 0,
            pending_failure: None,
            stale_read: None,
            read_tools: 2,
            write_tools: 1,
            irreversible: vec![String::from("fs.write")],
            sandbox: None,
            network_allowed: None,
            judgment_stage: None,
        }
    }

    #[test]
    fn an_unreconciled_failure_opens_in_repair() {
        let mut situation = a_situation();
        situation.pending_failure = Some(String::from("shell.run"));
        let reading = read("再试一次");
        let selection = select(&situation, &reading);
        assert_eq!(selection.stance, Stance::Repair);
        assert_eq!(selection.rule, "stance/repair-failure");
    }

    /// A granted approval is the one case where "nothing readable in the words"
    /// still has a posture: the call ran, and what is left is the work.
    #[test]
    fn a_granted_continuation_acts_even_when_the_words_say_nothing() {
        let mut situation = a_situation();
        situation.resumed = Some(String::from("granted"));
        let reading = read("批准了，继续。");
        assert_eq!(reading.act, SpeechAct::Unclear, "the words carry no cue");
        let selection = select(&situation, &reading);
        assert_eq!(selection.stance, Stance::Act);
        assert_eq!(selection.rule, "stance/continuation-granted");
    }

    /// A failure on the resumed call outranks the continuation: the turn is
    /// repairing, not continuing.
    #[test]
    fn a_failed_continuation_repairs_instead_of_acting() {
        let mut situation = a_situation();
        situation.resumed = Some(String::from("granted"));
        situation.pending_failure = Some(String::from("shell.run"));
        let selection = select(&situation, &read("批准了，继续。"));
        assert_eq!(selection.stance, Stance::Repair);
        assert_eq!(selection.rule, "stance/repair-failure");
    }

    #[test]
    fn a_write_request_is_guarded() {
        let selection = select(&a_situation(), &read("改一下 report.txt"));
        assert_eq!(selection.stance, Stance::Act);
        assert!(selection.mode.guarded);
        assert_eq!(selection.rule, "stance/act-guarded");
    }

    #[test]
    fn a_question_answers_and_a_greeting_chats() {
        assert_eq!(select(&a_situation(), &read("report.txt 里是什么？")).stance, Stance::Answer);
        let chat = select(&a_situation(), &read("你好"));
        assert_eq!(chat.stance, Stance::Chat);
        assert!(!chat.mode.guarded);
    }

    #[test]
    fn a_scheduled_turn_with_no_cue_records() {
        let mut situation = a_situation();
        situation.input_kind = String::from("scheduled");
        assert_eq!(select(&situation, &read("嗯")).stance, Stance::Record);
    }

    #[test]
    fn ids_are_stable_and_bounded() {
        let mut recorder = Recorder::new("t1");
        let first = recorder.node(
            ReasonKind::Action,
            1,
            "x".repeat(MAX_CLAIM_CHARS + 20),
            vec![Basis::fact("f")],
            "action/dispatch",
            ReasonSource::Model,
        );
        let second = recorder.node(
            ReasonKind::Observation,
            1,
            "ok",
            vec![Basis::node(first.id.clone())],
            "observation/result",
            ReasonSource::World,
        );
        assert_eq!(first.id, "r/t1/0");
        assert_eq!(second.id, "r/t1/1");
        assert_eq!(first.claim.chars().count(), MAX_CLAIM_CHARS);
    }

    #[test]
    fn every_rule_a_reading_or_stance_names_exists_in_the_table() {
        let situations = [a_situation()];
        let readings = [
            read("改 report.txt"),
            read("这是什么？"),
            read("不对，重来"),
            read("你好"),
            read("嗯"),
            read("我完成了"),
        ];
        for situation in &situations {
            for reading in &readings {
                assert!(rule(reading.rule()).is_some(), "{}", reading.rule());
                assert!(rule(select(situation, reading).rule).is_some());
            }
        }
        assert!(rule("situation/derive").is_some());
        assert!(rule("action/dispatch").is_some());
        assert!(rule("observation/result").is_some());
        assert!(rule("verdict/answer").is_some());
        assert!(rule("verdict/criterion-observed").is_some());
        assert!(rule("verdict/after-failure").is_some());
        assert!(rule("verdict/output-truncated").is_some());
        assert!(rule("gate/stale-read").is_some());
        assert!(rule("gate/repeat-call").is_some());
        assert!(rule("switch/failure-repair").is_some());
        assert!(rule("switch/steer-correction").is_some());
        assert!(rule("switch/steer-request").is_some());
    }

    #[test]
    fn a_mid_turn_switch_changes_the_posture_and_keeps_the_boundary() {
        let opened = select(&a_situation(), &read("改 report.txt"));
        assert!(opened.mode.guarded);
        let switched = switched_mode(opened.mode, Stance::Repair);
        assert_eq!(switched.stance, Stance::Repair);
        assert!(switched.guarded, "whether a call is irreversible is a fact of the turn");
    }

    #[test]
    fn a_node_without_a_basis_or_a_known_rule_is_refused() {
        let mut node = a_node();
        node.basis.clear();
        assert!(node.validate().unwrap_err().to_string().contains("no basis"));

        let mut node = a_node();
        node.rule = String::from("because/i/said/so");
        assert!(node.validate().unwrap_err().to_string().contains("does not know"));

        let mut node = a_node();
        node.basis.push(Basis::fact("second"));
        assert!(node.validate().is_ok());
    }

    #[test]
    fn a_basis_names_exactly_one_thing() {
        let mut node = a_node();
        node.basis = vec![Basis { node: None, event: None, fact: Some(String::new()) }];
        assert!(node.validate().is_err());

        node.basis = vec![Basis {
            node: Some(String::from("r/t/0")),
            event: Some(String::from("e1")),
            fact: None,
        }];
        assert!(node.validate().is_err());

        node.basis = vec![Basis::node("r/t/0")];
        assert!(node.validate().is_ok());
    }

    #[test]
    fn the_situation_and_stance_render_their_decision() {
        let situation = a_situation();
        assert!(situation.render().contains("1 write tool"));
        let selection = select(&situation, &read("改 report.txt"));
        assert!(selection.render().contains("stance: act"));
        assert!(selection.render().contains("guarded"));
    }
}
