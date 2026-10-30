//! SubagentProvider trait — delegate a subtask to another agent.
//!
//! The port answers "create, observe, cancel, recycle" and nothing else. It
//! knows no sessions, no event log, and no policy: a backend starts a child and
//! says how it ended; who asked, where the child's history lives, and what gets
//! recorded about the delegation are the kernel's business
//! ([`agent_kernel`]'s `SubagentDelegation`, built on this port).
//!
//! ## Two kinds of "cannot", kept apart
//!
//! A backend that is asked for something it does not do must **say so before
//! anything starts**. `output_schema` is the example the design names: a
//! backend that dropped the schema and ran the child anyway would hand back an
//! answer that looks like a schema-conformant result and is not one — a silent
//! downgrade is worse than a refusal, because the caller cannot tell it happened.
//! So a request declares what it is asking for — derived from its own fields by
//! [`SubagentRequest::requested_features`], not declared by the caller, who could
//! forget — and a backend publishes what it can honour through
//! [`SubagentProvider::supported`]. The difference between the two is a refusal.
//!
//! A child that *was* started and then failed is a different fact: that is
//! [`SubagentOutcome::Failed`], not an error, because something happened and it
//! has to be said.
//!
//! ## What a handle is, and what it is not
//!
//! A handle names a child — the backend, the session the child's history lives
//! in, and the parent turn that asked for it. It carries no status: a status
//! copied at `spawn` could only ever say `Running`, and a field that cannot be
//! anything else invites a reader to believe it is being kept up to date. The
//! status is what [`SubagentProvider::wait`] and [`SubagentProvider::cancel`]
//! return, and what the kernel records.

use std::fmt;

use agent_protocol::ids::{SessionId, TurnId};
use agent_protocol::model::ModelRoute;
use agent_protocol::profile::RunBudget;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A part of a [`SubagentRequest`] a backend may or may not be able to honour.
///
/// The list is exactly the request fields whose *absence of support would be
/// invisible from the result*: a child that was never told which tools it may
/// use still answers, and the answer looks like one that was. `objective` and
/// `context_refs` are not here because every backend carries them: they are what
/// the request is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentFeature {
    /// The parent can restrict which tools the child may use.
    ToolAllowlist,
    /// The parent can pin the child to a model route.
    ModelRoute,
    /// The parent can require the child's answer to match a schema.
    OutputSchema,
    /// The child can be held to a budget the parent states.
    Budget,
    /// The child can be stopped when the parent's deadline passes.
    Timeout,
}

impl SubagentFeature {
    /// The name this feature is written and reported under.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolAllowlist => "tool_allowlist",
            Self::ModelRoute => "model_route",
            Self::OutputSchema => "output_schema",
            Self::Budget => "budget",
            Self::Timeout => "timeout",
        }
    }
}

impl fmt::Display for SubagentFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An ordered set of [`SubagentFeature`]s.
///
/// Ordered rather than hashed for the reason a capability set is: it is compared,
/// reported, and (in the tests) named, and all three read better as the same
/// sequence every time rather than as whatever order a hash seed produces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentFeatures(Vec<SubagentFeature>);

impl SubagentFeatures {
    /// An empty set: nothing is claimed.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Add a feature. Adding one already present changes nothing.
    pub fn with(mut self, feature: SubagentFeature) -> Self {
        if !self.0.contains(&feature) {
            self.0.push(feature);
            self.0.sort();
        }
        self
    }

    /// Whether `feature` is in the set.
    pub fn contains(&self, feature: SubagentFeature) -> bool {
        self.0.contains(&feature)
    }

    /// Whether nothing is claimed.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The features, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = SubagentFeature> + '_ {
        self.0.iter().copied()
    }

    /// Which of `requested` this set does not contain.
    ///
    /// All of them, not the first: a request that asks for three unsupported
    /// things should be answered once with three names, not three times with one.
    pub fn missing_from(&self, requested: &SubagentFeatures) -> Vec<SubagentFeature> {
        requested.iter().filter(|feature| !self.contains(*feature)).collect()
    }
}

impl FromIterator<SubagentFeature> for SubagentFeatures {
    fn from_iter<T: IntoIterator<Item = SubagentFeature>>(iter: T) -> Self {
        iter.into_iter().fold(Self::new(), Self::with)
    }
}

impl fmt::Display for SubagentFeatures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.iter().map(SubagentFeature::as_str).collect();
        if names.is_empty() {
            return f.write_str("nothing");
        }
        f.write_str(&names.join(", "))
    }
}

/// What a parent asks a child to do.
///
/// `None` and empty mean *not asked for*, not "asked for and left at its
/// default": a caller that does not need the child on a particular model route
/// does not want a backend that cannot pin one to refuse the request. That is
/// why [`SubagentRequest::requested_features`] derives the asked-for set from
/// which fields are set rather than taking it from the caller.
///
/// The design's `cancellation_policy` is deliberately absent. This port's
/// cancellation has one behaviour — put the child down — and a field with a
/// single value would pretend a choice exists that nothing implements.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentRequest {
    /// What the child is for, in the parent's words. The one field every backend
    /// carries: it is the request.
    pub objective: String,
    /// References to context the child should be given (artifact locators, event
    /// ids). Sent verbatim; a backend may pass them through without resolving
    /// them, which is why they are not a negotiated feature.
    #[serde(default)]
    pub context_refs: Vec<String>,
    /// Which tools the child may use, if the parent wants to restrict them.
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    /// Which model the child should run on, if the parent wants to pin one.
    #[serde(default)]
    pub model_route: Option<ModelRoute>,
    /// What the child may spend, if the parent wants to bound it.
    #[serde(default)]
    pub budget: Option<RunBudget>,
    /// The shape the child's answer must have, if the parent requires one.
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// How long the child may run, in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// The plan step this child carries, if the parent named one.
    ///
    /// The parent's own bookkeeping rather than something asked of the child:
    /// it rides the request because the request is what the parent's log
    /// records at the moment the child starts, and a reader asking "which step
    /// is this child doing?" reads it from there. It is not a negotiated
    /// feature — a backend that ignores it still runs the child the same way —
    /// and it is skipped when unset so a request that names no step keeps the
    /// bytes it always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
}

impl SubagentRequest {
    /// A request for `objective`, asking for nothing beyond a child that runs.
    pub fn new(objective: impl Into<String>) -> Self {
        Self { objective: objective.into(), ..Self::default() }
    }

    /// The features this request actually asks a backend for.
    ///
    /// Derived, never declared: "I require an output schema" is not something a
    /// caller should be able to forget to say, and a caller that could forget
    /// would get the silent downgrade this module exists to prevent.
    pub fn requested_features(&self) -> SubagentFeatures {
        let mut features = SubagentFeatures::new();
        if !self.tool_allowlist.is_empty() {
            features = features.with(SubagentFeature::ToolAllowlist);
        }
        if self.model_route.is_some() {
            features = features.with(SubagentFeature::ModelRoute);
        }
        if self.output_schema.is_some() {
            features = features.with(SubagentFeature::OutputSchema);
        }
        if self.budget.is_some() {
            features = features.with(SubagentFeature::Budget);
        }
        if self.timeout_ms.is_some() {
            features = features.with(SubagentFeature::Timeout);
        }
        features
    }
}

/// The state a child ended in.
///
/// `Running` is not one of the terminal states and cannot be produced by an
/// outcome; it exists because a status is also what a reader asks about a child
/// that has not ended, and a vocabulary with a hole in it invites a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Completed,
    /// The child answered with less than it was asked for, and said so.
    Partial,
    Failed,
    Cancelled,
    /// The parent asked, and this backend cannot say whether the child stopped.
    CancelledUncertain,
    TimedOut,
}

/// How a child ended.
///
/// Every variant is a fact about the child, not a judgement about the
/// delegation: `Failed` says the child could not do the job, and the parent's
/// own decision to cancel is `Cancelled`. The two are kept apart because a
/// reader looks for them differently — one is a question about the task, the
/// other about who stopped it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum SubagentOutcome {
    /// The child finished with an answer.
    Completed {
        summary: String,
        /// The child's structured answer, if it gave one.
        #[serde(default)]
        result: Option<Value>,
    },
    /// The child says it did part of the job. Only ever produced because a child
    /// said so: this build never synthesizes it, because "how much got done" is
    /// not something the parent can see from outside.
    Partial { summary: String },
    /// The child could not do the job. `side_effects` is what the child says it
    /// did before giving up — the design asks for it because a failed
    /// child may still have changed the world.
    Failed {
        message: String,
        #[serde(default)]
        side_effects: Vec<String>,
    },
    /// The parent asked the child to stop and it did.
    Cancelled,
    /// The parent asked the child to stop and this backend cannot say whether it
    /// did. Distinct from `Cancelled` because the difference is exactly what a
    /// caller needs to know before reusing anything the child touched.
    CancelledUncertain { message: String },
    /// The child's deadline passed and the backend asked it to stop. Whether it
    /// *stopped* is the backend's to say, and `message` says it: a stop it could
    /// not confirm is stated there rather than passed over, the same way a
    /// cancellation is `CancelledUncertain` and not `Cancelled`.
    TimedOut { message: String },
}

impl SubagentOutcome {
    /// The terminal status this outcome is.
    pub fn status(&self) -> SubagentStatus {
        match self {
            Self::Completed { .. } => SubagentStatus::Completed,
            Self::Partial { .. } => SubagentStatus::Partial,
            Self::Failed { .. } => SubagentStatus::Failed,
            Self::Cancelled => SubagentStatus::Cancelled,
            Self::CancelledUncertain { .. } => SubagentStatus::CancelledUncertain,
            Self::TimedOut { .. } => SubagentStatus::TimedOut,
        }
    }

    /// One line a person can read: what happened, in the outcome's own terms.
    ///
    /// A summary of the summary, used where the full outcome would be noise —
    /// a refusal message, a log line, a test's assertion output. It is never
    /// used in place of the outcome where the outcome is the answer.
    pub fn describe(&self) -> String {
        match self {
            Self::Completed { summary, .. } => format!("completed: {summary}"),
            Self::Partial { summary } => format!("partial: {summary}"),
            Self::Failed { message, .. } => format!("failed: {message}"),
            Self::Cancelled => "cancelled".to_string(),
            Self::CancelledUncertain { message } => format!("cancelled (unconfirmed): {message}"),
            Self::TimedOut { message } => format!("timed out: {message}"),
        }
    }
}

/// A handle to one child — DSH's `{id, result, dispose}`, with the id being the
/// child's session.
///
/// There is no separate subagent id: the session the child's history lives in
/// *is* the child's identity, and a second name for the same thing is a second
/// name for a reader to reconcile. `parent_turn_id` attributes the child to a
/// turn of the parent's, so a settlement can be attributed to the turn it
/// belongs to rather than to whichever turn happens to be open when it arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentHandle {
    /// The backend that owns the child.
    pub provider: String,
    /// The session the child's history lives in. Assigned by the parent — the
    /// parent is the only party that can say what the child inherited.
    pub child_session_id: SessionId,
    /// The parent turn this child belongs to: for a forked child, the closed
    /// turn whose history it inherited. `None` when the backend was given no
    /// parent turn to attribute it to.
    #[serde(default)]
    pub parent_turn_id: Option<TurnId>,
}

impl SubagentHandle {
    pub fn new(
        provider: impl Into<String>,
        child_session_id: SessionId,
        parent_turn_id: Option<TurnId>,
    ) -> Self {
        Self { provider: provider.into(), child_session_id, parent_turn_id }
    }
}

/// Errors from [`SubagentProvider`] operations.
#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error("subagent backend '{provider}' cannot honour what the request asked for: {features}")]
    Unsupported { provider: String, features: SubagentFeatures },
    #[error("subagent '{child_session_id}' could not be started: {message}")]
    Spawn { child_session_id: SessionId, message: String },
    #[error("no child '{child_session_id}' is running under backend '{provider}'")]
    Unknown { provider: String, child_session_id: SessionId },
    #[error("the child's wire could not be spoken: {message}")]
    Wire { message: String },
    /// The backend's own bookkeeping was left inconsistent by a panic somewhere
    /// else in the process, so it can no longer say which children it holds.
    ///
    /// Every other variant here is about a child; this one is about the backend
    /// that would have held it. A caller that gets this learns the backend is
    /// damaged, not that the child is gone — the two must not read alike, which
    /// is why it is not folded into [`Unknown`](Self::Unknown).
    #[error("subagent backend '{provider}' lost track of its children: {message}")]
    Lost { provider: String, message: String },
}

/// Port for delegating a subtask to another agent.
///
/// Backends differ in what they are (a child process, an in-process session, a
/// remote worker); they do not differ in what a parent can ask of them, and the
/// two questions a caller always has — "what is happening" and "make it stop" —
/// are methods rather than backend-specific inventions.
///
/// The lifecycle is an explicit sequence, so that a caller which skips a step
/// skips a fact rather than losing one silently: `spawn` → (`wait` | `cancel`) →
/// `dispose`.
#[async_trait::async_trait]
pub trait SubagentProvider: Send + Sync {
    /// What this backend is called in records and refusals.
    fn name(&self) -> &str;

    /// What this backend can honour. Fail closed by default: a backend that has
    /// not said it supports something is taken not to.
    fn supported(&self) -> SubagentFeatures {
        SubagentFeatures::new()
    }

    /// Start a child for `request`.
    ///
    /// `child_session_id` is the parent's name for the child and
    /// `parent_turn_id` the parent turn to attribute it to; both are echoed back
    /// in the handle. A backend that cannot honour `request.requested_features()`
    /// returns [`SubagentError::Unsupported`] **without starting anything**.
    async fn spawn(
        &self,
        child_session_id: &SessionId,
        parent_turn_id: Option<&TurnId>,
        request: SubagentRequest,
    ) -> Result<SubagentHandle, SubagentError>;

    /// Wait for the child to end, and say how it ended.
    ///
    /// Waiting is not the same as being woken: this returns when the child ends,
    /// and a parent whose run has already ended learns of it through its Inbox
    /// instead — no paused process waits.
    async fn wait(&self, handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError>;

    /// Ask the child to stop, and wait until this backend can say what happened.
    ///
    /// The outcome — not the request — is the answer: a backend that put the
    /// child down returns `Cancelled`, and one that cannot confirm it returns
    /// `CancelledUncertain` rather than claiming a stop it did not see.
    async fn cancel(&self, handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError>;

    /// Release everything this backend holds for the child.
    ///
    /// Idempotent: disposing an already-disposed child is a no-op rather than an
    /// error, because the only reason a caller calls it twice is that it cannot
    /// tell whether the first call landed.
    ///
    /// A child still running is stopped first. A backend that released its
    /// bookkeeping and let the child keep running would have released the
    /// handle, not the child — and a caller that disposes without waiting is
    /// asking for the child to be gone.
    async fn dispose(&self, handle: &SubagentHandle) -> Result<(), SubagentError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::model::ModelTier;

    #[test]
    fn a_request_asks_for_what_its_fields_actually_set() {
        // The premise of the whole negotiation: "I require an output schema" is
        // read off the request rather than told to us, so a caller cannot ask
        // for one and have it dropped without the request itself saying so.
        let bare = SubagentRequest::new("look at the thing");
        assert!(bare.requested_features().is_empty());
        assert!(bare.requested_features().missing_from(&bare.requested_features()).is_empty());

        let demanding = SubagentRequest {
            output_schema: Some(serde_json::json!({ "type": "object" })),
            timeout_ms: Some(1_000),
            ..SubagentRequest::new("look at the thing")
        };
        let asked = demanding.requested_features();
        assert_eq!(
            asked.iter().collect::<Vec<_>>(),
            vec![SubagentFeature::OutputSchema, SubagentFeature::Timeout]
        );
    }

    #[test]
    fn an_empty_allowlist_is_not_a_request_for_one() {
        // The distinction the `Option`/empty convention exists for: not asking
        // for a restriction must not refuse a backend that cannot restrict.
        let request =
            SubagentRequest { tool_allowlist: Vec::new(), ..SubagentRequest::new("anything") };
        assert!(!request.requested_features().contains(SubagentFeature::ToolAllowlist));

        let restricted = SubagentRequest {
            tool_allowlist: vec!["fs.read".to_string()],
            ..SubagentRequest::new("anything")
        };
        assert!(restricted.requested_features().contains(SubagentFeature::ToolAllowlist));
    }

    #[test]
    fn a_backend_answers_with_every_feature_it_cannot_honour() {
        // All of them, named: a caller that asked for three unsupported things
        // should be answered once with three, not three times with one.
        let supported: SubagentFeatures = [SubagentFeature::Timeout].into_iter().collect();
        let requested: SubagentFeatures =
            [SubagentFeature::ModelRoute, SubagentFeature::OutputSchema, SubagentFeature::Timeout]
                .into_iter()
                .collect();

        assert_eq!(
            supported.missing_from(&requested),
            vec![SubagentFeature::ModelRoute, SubagentFeature::OutputSchema]
        );
        assert_eq!(supported.missing_from(&supported), Vec::new());
    }

    #[test]
    fn an_outcome_is_the_terminal_status_it_says_it_is() {
        let completed = SubagentOutcome::Completed { summary: "done".into(), result: None };
        assert_eq!(completed.status(), SubagentStatus::Completed);
        assert_eq!(
            SubagentOutcome::CancelledUncertain { message: "not confirmed".into() }.status(),
            SubagentStatus::CancelledUncertain
        );
        assert_eq!(
            SubagentOutcome::TimedOut { message: "too slow".into() }.status(),
            SubagentStatus::TimedOut
        );

        // `Running` is reachable as a status word and unreachable as an outcome:
        // a child that ended did not end still running.
        for outcome in [
            completed,
            SubagentOutcome::Partial { summary: "half".into() },
            SubagentOutcome::Failed {
                message: "broke".into(),
                side_effects: vec!["wrote x".into()],
            },
            SubagentOutcome::Cancelled,
        ] {
            assert_ne!(outcome.status(), SubagentStatus::Running);
        }
    }

    #[test]
    fn an_outcome_round_trips_through_serde() {
        // These are recorded as durable facts, so the shape the log holds is
        // this shape; a field that does not survive the trip would be a fact
        // that changes on the way to the reader.
        let outcome = SubagentOutcome::Failed {
            message: "the child exited without a result".into(),
            side_effects: vec!["created build/out.txt".into()],
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"status\":\"failed\""), "{json}");
        let decoded: SubagentOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, outcome);

        let completed = SubagentOutcome::Completed {
            summary: "done".into(),
            result: Some(serde_json::json!({ "answer": 42 })),
        };
        let decoded: SubagentOutcome =
            serde_json::from_str(&serde_json::to_string(&completed).unwrap()).unwrap();
        assert_eq!(decoded, completed);
    }

    #[test]
    fn a_handle_names_the_child_the_backend_and_the_turn_that_asked() {
        let child = SessionId::from_string("child-1");
        let turn = TurnId::from_string("turn-9");
        let handle = SubagentHandle::new("stdio", child.clone(), Some(turn.clone()));

        assert_eq!(handle.provider, "stdio");
        assert_eq!(handle.child_session_id, child);
        assert_eq!(handle.parent_turn_id, Some(turn));

        let decoded: SubagentHandle =
            serde_json::from_str(&serde_json::to_string(&handle).unwrap()).unwrap();
        assert_eq!(decoded, handle);
    }

    #[test]
    fn a_model_route_is_enough_to_ask_for_one() {
        // The feature derivation has to cover every field a backend can fail to
        // honour, not just the one the design names as an example.
        let request = SubagentRequest {
            model_route: Some(ModelRoute::new(ModelTier::Main, "openai", "gpt")),
            ..SubagentRequest::new("anything")
        };
        assert_eq!(
            request.requested_features().iter().collect::<Vec<_>>(),
            vec![SubagentFeature::ModelRoute]
        );
    }

    #[test]
    fn an_unsupported_refusal_names_every_feature() {
        let error = SubagentError::Unsupported {
            provider: "stdio".to_string(),
            features: [SubagentFeature::OutputSchema, SubagentFeature::Budget]
                .into_iter()
                .collect(),
        };
        let message = error.to_string();
        assert!(message.contains("stdio"), "{message}");
        assert!(message.contains("output_schema"), "{message}");
        assert!(message.contains("budget"), "{message}");
    }

    #[test]
    fn a_feature_set_reports_nothing_rather_than_an_empty_sentence() {
        // A refusal that said "cannot honour: " would read as a truncation.
        assert_eq!(SubagentFeatures::new().to_string(), "nothing");
        assert_eq!(SubagentFeature::OutputSchema.to_string(), "output_schema");
    }
}
