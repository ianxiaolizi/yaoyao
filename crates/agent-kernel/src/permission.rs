//! PermissionService — one durable decision before a tool call runs.
//!
//! Every other layer in this build can be read to *infer* why a call ran: the
//! policy events, the approval grants, the tool result's own record. Inference
//! is what a person does after the fact, and it is exactly what an audit cannot
//! rest on — two of those sources can be missing, and the one that is missing is
//! usually the one that mattered. This service writes the decision down instead:
//! one `permission/decision` event per call, produced before the call runs, and
//! the call does not run if it cannot be written.
//!
//! The record answers the questions an audit asks in one place: which action
//! (by digest, so it is the *same* action the result is recorded under), what
//! was decided, who authorised it, what was refused and by which layer, and what
//! a classifier said if one was consulted.
//!
//! Fail-closed is the whole design: [`PermissionService::decide`] writes before
//! it returns, and a caller that cannot write does not get a decision — it gets
//! an error, and the gateway turns that into a refusal. A permission chain whose
//! audit depends on a write that may have failed is not an audit.

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::permission_classifier::{PermissionClassifier, PermissionClassifierInput};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{ApprovalId, EventId, EventSeq, SchemaVersion, SessionId};
use agent_protocol::permission::{ActionFacts, RiskLevel};
pub use agent_protocol::permission_classifier::{ClassifierOutcome, ClassifierTrace};
use agent_protocol::permissions::{Capability, PolicyDecision, RefusalLayer};
use chrono::{DateTime, Utc};

/// Where the authority for a decision came from.
///
/// Three answers, and they are not degrees of the same thing: `Policy` is a rule
/// this build or its operator wrote, `PermissionClassifier` is an estimate made
/// at call time, `UserApproval` is a person's answer about this action. A
/// reader asking "what let this run" is owed the one that applied, not the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationSource {
    Policy,
    PermissionClassifier,
    UserApproval,
}

/// How sure a classifier has to be before an `allow` is acted on.
///
/// Below this the answer is not evidence: a backend that says "allow" at 0.3 is
/// guessing, and a guess that lets a boundary action run is the failure this
/// family exists to prevent. The number is policy, so it lives beside the
/// matrix rather than inside the adapter — a reader looking for "what can
/// tighten a call" finds one function, not one function and an adapter.
pub const MIN_CLASSIFIER_CONFIDENCE: f64 = 0.7;

/// What the deterministic layers said, before anything estimates.
///
/// A struct rather than a bare [`PolicyDecision`] because the record has to name
/// *why* as well as *what*, and a refusal is two facts: the capability that was
/// refused and the layer that refused it. A caller that has already refused the
/// call for a reason outside the policy (the workspace boundary, a sandbox that
/// is not there) says so in `reason` and leaves `refusals` empty — the record
/// still names the decision, and the sentence is still the one a person reads.
#[derive(Debug, Clone)]
pub struct PermissionBaseline {
    pub decision: PolicyDecision,
    pub refusals: BTreeMap<Capability, RefusalLayer>,
    pub reason: Option<String>,
    /// Whether a person's standing grant is what turned the policy's `ask`
    /// into this `allow`.
    ///
    /// The distinction the matrix turns on: a rule that allows a boundary action
    /// outright is the machine deciding by itself, while a grant is a person
    /// who already answered. The first needs an independent judgement before it
    /// runs; the second is one, and an absent classifier does not erase it.
    pub approved: bool,
}

impl PermissionBaseline {
    pub fn new(decision: PolicyDecision) -> Self {
        Self { decision, refusals: BTreeMap::new(), reason: None, approved: false }
    }

    /// A refusal that is not a policy refusal, with the sentence to record.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            decision: PolicyDecision::Deny,
            refusals: BTreeMap::new(),
            reason: Some(reason.into()),
            approved: false,
        }
    }

    /// Record why, when the caller already has the sentence.
    pub fn because(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Say whether a person's grant is what allowed this call.
    pub fn approve(mut self, approved: bool) -> Self {
        self.approved = approved;
        self
    }
}

impl From<agent_protocol::tool::ToolPolicyRecord> for PermissionBaseline {
    fn from(record: agent_protocol::tool::ToolPolicyRecord) -> Self {
        Self { decision: record.decision, refusals: record.refusals, reason: None, approved: false }
    }
}

/// The decision one call was granted, refused or held on.
///
/// This is what `permission/decision` stores, and it is deliberately the same
/// shape a caller gets back: a record built for the log and an answer built for
/// the caller are two things that can disagree, and this one thing cannot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionDecisionRecord {
    pub decision: PolicyDecision,
    /// The action by digest — the same fingerprint the result is recorded under.
    pub action: ActionFacts,
    pub authorization_source: AuthorizationSource,
    /// The approval this decision was made against, when one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<ApprovalId>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refusals: BTreeMap<Capability, RefusalLayer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier: Option<ClassifierTrace>,
    pub created_at: DateTime<Utc>,
}

impl PermissionDecisionRecord {
    pub fn action_digest(&self) -> &str {
        &self.action.digest
    }

    pub fn risk(&self) -> RiskLevel {
        self.action.risk
    }
}

/// Errors from the decision ledger.
#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("decision payload error: {0}")]
    Payload(#[from] serde_json::Error),
}

/// The event-sourced ledger of permission decisions.
pub struct PermissionService {
    store: Arc<dyn EventStore>,
}

impl PermissionService {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self { store }
    }

    /// Decide one call and write the decision down, or refuse to decide.
    ///
    /// The order is the policy's, and it is the reason a classifier cannot widen
    /// anything: a baseline `deny` is final before the classifier is consulted,
    /// so no answer it gives can turn the call into an allow. Everything after
    /// that can only tighten — a classifier that refuses refuses, one that asks
    /// asks, and one that allows leaves the baseline's own answer standing.
    ///
    /// The write happens here, before this returns. A caller that receives a
    /// record holds the guarantees of the log, not of a promise to write later.
    pub async fn decide(
        &self,
        session_id: &SessionId,
        action: ActionFacts,
        baseline: PermissionBaseline,
        classifier: Option<ClassifierTrace>,
    ) -> Result<PermissionDecisionRecord, PermissionError> {
        let record = rule(action, baseline, classifier);
        let payload = serde_json::to_value(&record)?;
        // The read and the append are one step, held under the store's lease:
        // a decision written onto a log this read did not see would be a
        // decision taken about a different session state (CONTRACT §4 C4.5).
        let _lease = self.store.lease(session_id).await?;
        let next = self.next_seq(session_id).await?;
        let draft = EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(EventType::PERMISSION_DECISION),
            schema_version: SchemaVersion::current(),
            session_id: session_id.clone(),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::System,
            created_at: record.created_at,
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: serde_json::json!({}),
            ignorable: false,
        };
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(record)
    }

    /// The record a session's log holds for one action digest, if any.
    ///
    /// A reader's entry point, and the one tests use: the claim this service
    /// makes is "the decision is in the log", so the check is a read of the log
    /// rather than a field on a struct the caller was handed.
    pub async fn recorded(
        &self,
        session_id: &SessionId,
        action_digest: &str,
    ) -> Result<Option<PermissionDecisionRecord>, PermissionError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        for stored in events.iter().rev() {
            if stored.event.event_type.as_str() != EventType::PERMISSION_DECISION {
                continue;
            }
            let Ok(record) =
                serde_json::from_value::<PermissionDecisionRecord>(stored.event.payload.clone())
            else {
                continue;
            };
            if record.action.digest == action_digest {
                return Ok(Some(record));
            }
        }
        Ok(None)
    }

    async fn next_seq(&self, session_id: &SessionId) -> Result<EventSeq, PermissionError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO))
    }
}

/// What one call's decision comes to, without writing it down.
///
/// [`PermissionService::decide`] is this plus the write, and that split is
/// deliberate: a gateway assembled without a ledger (a fixture, a unit test of
/// some other layer) still has to make the same decision as the shipped one, or
/// the thing being tested is not the thing that ships.
pub fn rule(
    action: ActionFacts,
    baseline: PermissionBaseline,
    classifier: Option<ClassifierTrace>,
) -> PermissionDecisionRecord {
    let decided = resolve(&action, &baseline, classifier.as_ref());
    PermissionDecisionRecord {
        decision: decided.decision,
        action,
        authorization_source: decided.source,
        approval_id: decided.approval_id,
        reason: decided.reason,
        refusals: baseline.refusals,
        classifier,
        created_at: Utc::now(),
    }
}

/// The matrix: what a baseline answer and a classifier's advice come to.
struct Decided {
    decision: PolicyDecision,
    source: AuthorizationSource,
    reason: String,
    approval_id: Option<ApprovalId>,
}

/// Apply the matrix.
///
/// A function rather than a method because the matrix is the rule a reader has
/// to be able to check against the contract — it is the same rule whether the
/// caller is the gateway, a test, or a future second entry point.
///
/// The order is the whole point, and each step can only tighten:
///
/// 1. A deterministic refusal is final. No classifier answer, however sure,
///    turns a denied call into a running one.
/// 2. With no classifier configured, the policy's answer stands, and the
///    record names no judgement: a build that never had a judge is not a build
///    whose judge abstained. (An *announced* judge that then fails does stop
///    the call — see the `unavailable` arm below.)
/// 3. `deny` refuses, `ask` asks, and an unreachable or unreadable backend asks.
/// 4. An `allow` is acted on only above [`MIN_CLASSIFIER_CONFIDENCE`], and only
///    as far as the baseline already went: the classifier never raises an
///    `ask` into an `allow`.
fn resolve(
    action: &ActionFacts,
    baseline: &PermissionBaseline,
    classifier: Option<&ClassifierTrace>,
) -> Decided {
    // The floor first, and before any estimate: a managed deny, a rule that
    // refuses this invocation, or a capability this build's sandbox does not
    // have is a fact, and no classifier's opinion about a call changes a fact.
    if baseline.decision == PolicyDecision::Deny {
        return Decided {
            decision: PolicyDecision::Deny,
            source: AuthorizationSource::Policy,
            reason: baseline
                .reason
                .clone()
                .unwrap_or_else(|| refusals_sentence(&baseline.refusals)),
            approval_id: None,
        };
    }

    let Some(trace) = classifier else {
        return Decided {
            decision: baseline.decision,
            source: if baseline.approved {
                AuthorizationSource::UserApproval
            } else {
                AuthorizationSource::Policy
            },
            reason: baseline.reason.clone().unwrap_or_else(|| {
                if baseline.approved && action.requires_high_risk_isolation() {
                    "a person's standing grant covers this action".to_string()
                } else {
                    "the policy allows this call".to_string()
                }
            }),
            approval_id: None,
        };
    };

    match trace.outcome {
        ClassifierOutcome::Deny => Decided {
            decision: PolicyDecision::Deny,
            source: AuthorizationSource::PermissionClassifier,
            reason: trace
                .suggestion
                .as_ref()
                .filter(|suggestion| !suggestion.reason.is_empty())
                .map(|suggestion| {
                    format!("the permission classifier refused: {}", suggestion.reason)
                })
                .unwrap_or_else(|| "the permission classifier refused the call".to_string()),
            approval_id: None,
        },
        ClassifierOutcome::Ask => Decided {
            decision: PolicyDecision::Ask,
            source: AuthorizationSource::PermissionClassifier,
            reason: "the permission classifier asked for a person".to_string(),
            approval_id: None,
        },
        // Abstention, a timeout, a protocol error and an unreachable backend are
        // one case: none of them is evidence that the call is fine, so the call
        // stops and asks. A classifier that cannot answer does not inherit the
        // baseline's allow.
        //
        // The one exception is a call a person already granted: nothing was
        // learned by not asking the classifier, so the standing answer stands.
        // The call is not running *automatically* — a person decided about this
        // capability — and B4 ties that grant to the exact action it covered.
        ClassifierOutcome::Unclear | ClassifierOutcome::Unavailable
            if baseline.approved && baseline.decision == PolicyDecision::Allow =>
        {
            Decided {
                decision: PolicyDecision::Allow,
                source: AuthorizationSource::UserApproval,
                reason: "a person's standing grant covers this action; no independent classifier \
                         answered"
                    .to_string(),
                approval_id: None,
            }
        }
        ClassifierOutcome::Unclear | ClassifierOutcome::Unavailable => Decided {
            decision: PolicyDecision::Ask,
            source: AuthorizationSource::PermissionClassifier,
            reason: match trace.outcome {
                ClassifierOutcome::Unavailable => {
                    "no independent classifier was reachable for this call".to_string()
                }
                _ => "the permission classifier's answer could not be read".to_string(),
            },
            approval_id: None,
        },
        ClassifierOutcome::Allow => {
            let suggestion = trace.suggestion.as_ref();
            let confidence = suggestion.map(|s| s.confidence).unwrap_or(0.0);
            if confidence < MIN_CLASSIFIER_CONFIDENCE {
                return Decided {
                    decision: PolicyDecision::Ask,
                    source: AuthorizationSource::PermissionClassifier,
                    reason: format!(
                        "the permission classifier was not sure ({confidence:.2} < \
                         {MIN_CLASSIFIER_CONFIDENCE}); a person sees it"
                    ),
                    approval_id: None,
                };
            }
            if suggestion.is_some_and(|s| s.requires_confirmation) {
                return Decided {
                    decision: PolicyDecision::Ask,
                    source: AuthorizationSource::PermissionClassifier,
                    reason: "the permission classifier asked that a person see this action"
                        .to_string(),
                    approval_id: None,
                };
            }
            Decided {
                decision: baseline.decision,
                source: AuthorizationSource::PermissionClassifier,
                reason: "the permission classifier allowed the call".to_string(),
                approval_id: None,
            }
        }
    }
}

/// What the policy's answer and a classifier's advice come to, without a record.
///
/// The gateway branches on this rather than on the policy's own answer, so the
/// door a call leaves by is the one the matrix chose: a classifier's `deny`
/// stops an otherwise-allowed call, and its `ask` holds one.
pub fn verdict(
    action: &ActionFacts,
    decision: PolicyDecision,
    approved: bool,
    classifier: Option<&ClassifierTrace>,
) -> PolicyDecision {
    resolve(action, &PermissionBaseline::new(decision).approve(approved), classifier).decision
}

/// Ask the classifier about one call, when there is anything to ask.
///
/// `None` means "this question does not apply": the policy already refused (a
/// deny is final before any estimate), the action is below the boundary the
/// classifier exists for, or **this build has no judge at all**. The last is a
/// configuration fact, not an abstention: a build without an independent backend
/// was never promised one, and the record says so by naming no trace rather than
/// by inventing an `unavailable` one. An abstention is what a *configured*
/// classifier produces when it cannot answer, and that does read as "ask".
///
/// The input is built from the action facts and nothing else: no conversation,
/// no file contents, no environment, and no resolved secret — a `secret://`
/// reference stays the reference it was written as.
pub async fn judge(
    classifier: Option<&Arc<dyn PermissionClassifier>>,
    session_id: &SessionId,
    action: &ActionFacts,
    decision: PolicyDecision,
    capabilities: &[Capability],
) -> Option<ClassifierTrace> {
    if decision == PolicyDecision::Deny || !action.requires_high_risk_isolation() {
        return None;
    }
    let classifier = classifier?;
    let input = PermissionClassifierInput {
        // The id the action itself was built under, so the record and the
        // judgement name the same work.
        session_id: session_id.clone(),
        task_id: action.task_id.clone(),
        task_revision: action.task_revision,
        // The gateway holds no view of the work in progress: these three are
        // the caller's to fill in when one exists, and their absence is stated
        // rather than papered over with an empty string.
        task_summary: None,
        action: action.clone(),
        policy_baseline: decision,
        allowed_capabilities: capabilities.to_vec(),
        recent_actions: Vec::new(),
        untrusted_context_summary: None,
    };
    Some(classifier.classify(&input).await)
}

fn refusals_sentence(refusals: &BTreeMap<Capability, RefusalLayer>) -> String {
    if refusals.is_empty() {
        return "the call was refused before it ran".to_string();
    }
    refusals
        .iter()
        .map(|(capability, layer)| {
            format!("capability {} {}", capability.as_str(), layer.described())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::permission::{ActionSpec, Reversibility, RiskLevel};
    use agent_protocol::permission_classifier::{PermissionSuggestion, SuggestionDecision};
    use agent_testkit::FakeEventStore;

    fn sid() -> SessionId {
        SessionId::from_string("permission-session")
    }

    fn facts(tool: &str) -> ActionFacts {
        ActionFacts::from_spec(ActionSpec {
            task_id: "intent-1".to_string(),
            task_revision: 0,
            tool_name: tool.to_string(),
            capabilities: vec![Capability::new(Capability::FS_READ)],
            risk: RiskLevel::L0,
            reversibility: Reversibility::Reversible,
            targets: Vec::new(),
            arguments: serde_json::json!({}),
            summary: format!("{tool} nothing"),
        })
    }

    fn service() -> (PermissionService, Arc<FakeEventStore>) {
        let store = Arc::new(FakeEventStore::new());
        (PermissionService::new(store.clone()), store)
    }

    /// A store that cannot write: the ledger has nowhere to put a decision.
    ///
    /// Only `append` fails — leasing and reading work — so the failure a test
    /// sees is the one under test rather than a lock that could not be taken.
    struct RefusingAppends(FakeEventStore);

    #[async_trait::async_trait]
    impl EventStore for RefusingAppends {
        async fn append(
            &self,
            _session_id: &SessionId,
            _expected_next_seq: EventSeq,
            _drafts: Vec<EventDraft>,
        ) -> Result<agent_ports::event_store::AppendReceipt, EventStoreError> {
            Err(EventStoreError::Io(std::io::Error::other("the decision ledger is not writable")))
        }

        async fn subscribe(
            &self,
            session_id: &SessionId,
            from: EventSeq,
        ) -> Result<Box<dyn agent_ports::event_store::EventSubscription>, EventStoreError> {
            self.0.subscribe(session_id, from).await
        }

        async fn lease(
            &self,
            session_id: &SessionId,
        ) -> Result<Box<dyn agent_ports::event_store::SessionLease>, EventStoreError> {
            self.0.lease(session_id).await
        }

        async fn driver_lease(
            &self,
            session_id: &SessionId,
        ) -> Result<Box<dyn agent_ports::event_store::SessionLease>, EventStoreError> {
            self.0.driver_lease(session_id).await
        }

        async fn flush(
            &self,
            session_id: &SessionId,
        ) -> Result<agent_ports::event_store::FlushReceipt, EventStoreError> {
            self.0.flush(session_id).await
        }

        async fn read_stream(
            &self,
            session_id: &SessionId,
            from_seq: EventSeq,
            limit: usize,
        ) -> Result<Vec<agent_protocol::events::StoredEvent>, EventStoreError> {
            self.0.read_stream(session_id, from_seq, limit).await
        }
    }

    #[tokio::test]
    async fn a_policy_allowed_call_records_policy_as_its_source() {
        let (service, _) = service();
        let action = facts("fs.read");
        let record = service
            .decide(&sid(), action.clone(), PermissionBaseline::new(PolicyDecision::Allow), None)
            .await
            .expect("the decision is written");

        assert_eq!(record.decision, PolicyDecision::Allow);
        assert_eq!(record.authorization_source, AuthorizationSource::Policy);
        assert_eq!(record.action_digest(), action.digest);
        assert!(record.classifier.is_none());
    }

    #[tokio::test]
    async fn a_decision_is_in_the_log_before_decide_returns() {
        // The one property the whole chain rests on: a caller holding this
        // record is holding something a restarted process can read back.
        let (service, _) = service();
        let action = facts("fs.read");
        let record = service
            .decide(&sid(), action.clone(), PermissionBaseline::new(PolicyDecision::Allow), None)
            .await
            .expect("written");

        let stored = service.recorded(&sid(), &action.digest).await.expect("readable");
        let stored = stored.expect("the decision is in the log");
        assert_eq!(stored.action.digest, record.action.digest);
        assert_eq!(stored.decision, record.decision);
    }

    #[tokio::test]
    async fn a_classifier_cannot_lift_a_hard_deny() {
        let (service, _) = service();
        let record = service
            .decide(
                &sid(),
                facts("fs.read"),
                PermissionBaseline::refused("the workspace boundary refused this path"),
                Some(ClassifierTrace {
                    outcome: ClassifierOutcome::Allow,
                    route_id: "pack".to_string(),
                    revision: "1".to_string(),
                    pack_version: "permission/1".to_string(),
                    prompt_hash: String::new(),
                    input_hash: String::new(),
                    latency_ms: 3,
                    suggestion: Some(PermissionSuggestion {
                        decision: SuggestionDecision::Allow,
                        confidence: 1.0,
                        risk: None,
                        reversible: None,
                        requires_confirmation: false,
                        reason: "test".to_string(),
                    }),
                }),
            )
            .await
            .expect("written");

        assert_eq!(record.decision, PolicyDecision::Deny);
        assert_eq!(record.authorization_source, AuthorizationSource::Policy);
        assert_eq!(record.reason, "the workspace boundary refused this path");
    }

    #[tokio::test]
    async fn an_unavailable_classifier_asks_rather_than_allowing() {
        let (service, _) = service();
        let record = service
            .decide(
                &sid(),
                facts("web.request"),
                PermissionBaseline::new(PolicyDecision::Allow),
                Some(ClassifierTrace::unavailable("permission")),
            )
            .await
            .expect("written");

        assert_eq!(record.decision, PolicyDecision::Ask, "an absent answer is not consent");
        assert_eq!(record.authorization_source, AuthorizationSource::PermissionClassifier);
    }

    #[tokio::test]
    async fn a_failed_write_yields_no_decision() {
        // The store refuses every append; the caller gets an error, never a
        // record. Fail-closed means the absence of a decision is an answer.
        let service = PermissionService::new(Arc::new(RefusingAppends(FakeEventStore::new())));
        let error = service
            .decide(&sid(), facts("fs.read"), PermissionBaseline::new(PolicyDecision::Allow), None)
            .await
            .expect_err("no decision without a write");
        assert!(matches!(error, PermissionError::Store(_)), "{error:?}");
    }

    // ---- The gateway's use of the ledger ----
    //
    // The tests above prove the record is written. These prove the *gateway*
    // writes it: before the call runs, always, and under the same digest the
    // result is stamped with.

    use crate::approval::ApprovalService;
    use crate::guarded::GuardedToolProvider;
    use crate::policy::{PolicyEngine, PolicyRule};
    use crate::resource_guard::ResourceGuard;
    use agent_ports::tool_gateway::{ToolGateway, ToolGatewayError};
    use agent_ports::tool_provider::{ToolProvider, ToolProviderError, ToolSideEffect, ToolSource};
    use agent_protocol::permissions::{ApprovalScope, Workspace};
    use agent_protocol::tool::{ToolDefinition, ToolInvocation, ToolResult};
    use agent_testkit::InMemoryArtifactStore;
    use std::sync::Mutex;

    fn call(name: &str) -> ToolInvocation {
        ToolInvocation::proposed(name, serde_json::json!({}))
    }

    fn def(name: &str, caps: &[&str], side_effect: ToolSideEffect) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("test tool {name}"),
            source: ToolSource::Builtin,
            side_effect,
            capability_requirements: caps.iter().map(|c| Capability::new(*c)).collect(),
            idempotent: true,
            timeout_ms: None,
            input_schema: serde_json::json!({}),
        }
    }

    fn workspace() -> Workspace {
        Workspace {
            workspace_root: std::env::current_dir().unwrap().display().to_string(),
            writable_roots: vec![],
            external_directories: vec![],
        }
    }

    fn allowing(capability: &str) -> PolicyEngine {
        PolicyEngine::new()
            .with_rules([PolicyRule::allow(Capability::new(capability))])
            .with_sandbox([Capability::new(capability)])
    }

    /// A provider that answers, and says whether the ledger already held this
    /// call's decision at the moment it was asked to run.
    ///
    /// The check happens inside `invoke`, so it is the tool's own view of the
    /// order rather than a test's reading of two later timestamps.
    struct Witnessed {
        defs: Vec<ToolDefinition>,
        store: Arc<FakeEventStore>,
        session: SessionId,
        seen: Mutex<Vec<bool>>,
    }

    impl Witnessed {
        fn new(defs: Vec<ToolDefinition>, store: Arc<FakeEventStore>) -> Arc<Self> {
            Arc::new(Self { defs, store, session: sid(), seen: Mutex::new(Vec::new()) })
        }

        fn seen(&self) -> Vec<bool> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for Witnessed {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }

        fn side_effect(&self, name: &str) -> Option<ToolSideEffect> {
            self.defs.iter().find(|d| d.name == name).map(|d| d.side_effect)
        }

        async fn invoke(&self, _: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
            let events = self.store.read_stream(&self.session, EventSeq::ZERO, 100).await.unwrap();
            let written = events
                .iter()
                .any(|event| event.event.event_type.as_str() == EventType::PERMISSION_DECISION);
            self.seen.lock().unwrap().push(written);
            Ok(ToolResult::completed("ran"))
        }
    }

    fn gateway_over(
        inner: Arc<dyn ToolProvider>,
        policy: PolicyEngine,
        store: Arc<FakeEventStore>,
    ) -> (GuardedToolProvider, Arc<ApprovalService>, Arc<PermissionService>) {
        let approvals = Arc::new(ApprovalService::new(store.clone()));
        let permissions = Arc::new(PermissionService::new(store));
        let gateway = GuardedToolProvider::new(
            inner,
            policy,
            approvals.clone(),
            ResourceGuard::new(workspace()),
            Arc::new(InMemoryArtifactStore::default()),
        )
        .with_permission_service(permissions.clone());
        (gateway, approvals, permissions)
    }

    /// The decision events in a session's log, oldest first.
    async fn decisions(store: &FakeEventStore) -> Vec<PermissionDecisionRecord> {
        store
            .read_stream(&sid(), EventSeq::ZERO, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.event.event_type.as_str() == EventType::PERMISSION_DECISION)
            .map(|event| serde_json::from_value(event.event.payload).expect("a decision reads"))
            .collect()
    }

    #[tokio::test]
    async fn a_tool_call_writes_its_decision_before_it_runs() {
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
            store.clone(),
        );
        let (gateway, _, _) =
            gateway_over(tools.clone(), allowing(Capability::FS_READ), store.clone());

        gateway.execute(&sid(), &call("fs.read")).await.unwrap();

        assert_eq!(tools.seen(), vec![true], "the decision was in the log before the tool ran");
        assert_eq!(decisions(&store).await.len(), 1, "exactly one decision per call");
    }

    #[tokio::test]
    async fn a_failed_decision_write_prevents_tool_execution() {
        // The tool would run fine; the audit would not. The call does not run —
        // an audited chain whose audit can be skipped by breaking the store is
        // not audited at all.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
            store.clone(),
        );
        let approvals = Arc::new(ApprovalService::new(store));
        let gateway = GuardedToolProvider::new(
            tools.clone(),
            allowing(Capability::FS_READ),
            approvals,
            ResourceGuard::new(workspace()),
            Arc::new(InMemoryArtifactStore::default()),
        )
        .with_permission_service(Arc::new(PermissionService::new(Arc::new(
            RefusingAppends(FakeEventStore::new()),
        ))));

        let error = gateway.execute(&sid(), &call("fs.read")).await.unwrap_err();

        assert!(matches!(error, ToolGatewayError::PermissionAudit { .. }), "{error:?}");
        assert!(tools.seen().is_empty(), "the tool never saw the call");
    }

    #[tokio::test]
    async fn a_hard_deny_cannot_be_overridden_by_a_grant() {
        // A person really has said yes and the grant is in effect — and the
        // call is still refused, because a managed deny is not a question a
        // grant can answer.
        let store = Arc::new(FakeEventStore::new());
        let approvals = Arc::new(ApprovalService::new(store.clone()).with_session_scope(true));
        let gateway = GuardedToolProvider::new(
            Witnessed::new(
                vec![def("shell.run", &[Capability::PROCESS_SPAWN], ToolSideEffect::Destructive)],
                store.clone(),
            ),
            PolicyEngine::new()
                .with_managed_deny([Capability::new(Capability::PROCESS_SPAWN)])
                .with_rules([PolicyRule::ask(Capability::new(Capability::PROCESS_SPAWN))])
                .with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]),
            approvals.clone(),
            ResourceGuard::new(workspace()),
            Arc::new(InMemoryArtifactStore::default()),
        )
        .with_permission_service(Arc::new(PermissionService::new(store.clone())));
        let request = approvals
            .request(&sid(), Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
            .await
            .unwrap();
        approvals
            .grant(&sid(), &request.approval_id, ApprovalScope::AllowForSession, "user")
            .await
            .unwrap();
        assert_eq!(approvals.leases_in_effect(&sid()).await.unwrap().len(), 1);

        let error = gateway.execute(&sid(), &call("shell.run")).await.unwrap_err();

        assert!(matches!(error, ToolGatewayError::PolicyDenied { .. }), "{error:?}");
        let recorded = decisions(&store).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].decision, PolicyDecision::Deny);
        assert_eq!(
            recorded[0].refusals.get(&Capability::new(Capability::PROCESS_SPAWN)),
            Some(&RefusalLayer::ManagedPolicy),
            "the record names the layer that refused, not merely that one did"
        );
    }

    #[tokio::test]
    async fn a_decision_record_names_the_subject_and_authorization_source() {
        let store = Arc::new(FakeEventStore::new());
        let (gateway, _, _) = gateway_over(
            Witnessed::new(
                vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
                store.clone(),
            ),
            allowing(Capability::FS_READ),
            store.clone(),
        );

        gateway.execute(&sid(), &call("fs.read")).await.unwrap();

        let recorded = decisions(&store).await;
        let record = recorded.first().expect("a decision is recorded");
        assert_eq!(record.action.tool_name, "fs.read");
        assert_eq!(record.action.task_id, format!("manual:{}", sid().as_str()));
        assert_eq!(record.authorization_source, AuthorizationSource::Policy);
        assert_eq!(record.decision, PolicyDecision::Allow);
        assert_eq!(record.risk(), RiskLevel::L0);
    }

    #[tokio::test]
    async fn a_decision_record_and_its_tool_result_carry_the_same_action_digest() {
        // The join key between "the gate said yes" and "the tool ran" is the
        // digest. Two records that cannot be joined are two records a reader has
        // to guess between.
        let store = Arc::new(FakeEventStore::new());
        let (gateway, _, permissions) = gateway_over(
            Witnessed::new(
                vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
                store.clone(),
            ),
            allowing(Capability::FS_READ),
            store.clone(),
        );

        let result = gateway.execute(&sid(), &call("fs.read")).await.unwrap();

        let policy = result.policy.expect("the result carries its decision");
        let digest = policy.action_digest.expect("the result names the action it was about");
        let recorded = permissions.recorded(&sid(), &digest).await.unwrap();
        let recorded = recorded.expect("the very digest the result names is in the log");
        assert_eq!(recorded.action.digest, digest);
        assert_eq!(recorded.decision, policy.decision);
    }

    #[tokio::test]
    async fn an_old_log_without_permission_events_still_reads() {
        // A session written before this ledger existed has no decision to find.
        // "None" is the correct answer; an error would make every old session
        // unreadable, which is the opposite of what the log is for.
        let (service, _) = service();
        assert!(service.recorded(&sid(), "sha256:absent").await.unwrap().is_none());
    }

    // ---- The classifier (B3) ----
    //
    // The matrix above is checked here against a gateway: the answer the
    // classifier gives is advice the gate acts on, it can only tighten, and it
    // is taken before anything is written or run.

    use agent_ports::permission_classifier::{PermissionClassifier, PermissionClassifierInput};

    /// A classifier that answers one scripted verdict and remembers what it was
    /// asked.
    struct Scripted {
        outcome: ClassifierOutcome,
        suggestion: Option<(SuggestionDecision, f64, Option<RiskLevel>)>,
        seen: Mutex<Vec<PermissionClassifierInput>>,
    }

    impl Scripted {
        fn allowing(confidence: f64) -> Arc<Self> {
            Arc::new(Self {
                outcome: ClassifierOutcome::Allow,
                suggestion: Some((SuggestionDecision::Allow, confidence, None)),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn denying() -> Arc<Self> {
            Arc::new(Self {
                outcome: ClassifierOutcome::Deny,
                suggestion: Some((SuggestionDecision::Deny, 0.95, None)),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn judging(risk: RiskLevel) -> Arc<Self> {
            Arc::new(Self {
                outcome: ClassifierOutcome::Allow,
                suggestion: Some((SuggestionDecision::Allow, 0.99, Some(risk))),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn seen(&self) -> Vec<PermissionClassifierInput> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl PermissionClassifier for Scripted {
        async fn classify(&self, input: &PermissionClassifierInput) -> ClassifierTrace {
            self.seen.lock().unwrap().push(input.clone());
            let suggestion =
                self.suggestion.as_ref().map(|(decision, confidence, risk)| PermissionSuggestion {
                    decision: *decision,
                    confidence: *confidence,
                    risk: *risk,
                    reversible: None,
                    requires_confirmation: false,
                    reason: "scripted".to_string(),
                });
            ClassifierTrace {
                route_id: "scripted".to_string(),
                revision: "rev-1".to_string(),
                pack_version: "permission/1".to_string(),
                prompt_hash: "prompt".to_string(),
                input_hash: "input".to_string(),
                latency_ms: 4,
                outcome: self.outcome,
                suggestion,
            }
        }
    }

    /// A gateway over `policy` with `classifier` installed and its ledger kept.
    fn judged_gateway(
        tools: Arc<Witnessed>,
        policy: PolicyEngine,
        store: Arc<FakeEventStore>,
        classifier: Arc<Scripted>,
    ) -> GuardedToolProvider {
        GuardedToolProvider::new(
            tools,
            policy,
            Arc::new(ApprovalService::new(store.clone())),
            ResourceGuard::new(workspace()),
            Arc::new(InMemoryArtifactStore::default()),
        )
        .with_permission_service(Arc::new(PermissionService::new(store)))
        .with_permission_classifier(classifier)
    }

    fn network_call() -> ToolInvocation {
        ToolInvocation::proposed(
            "web.request",
            serde_json::json!({ "url": "https://docs.rs/index.html" }),
        )
    }

    #[tokio::test]
    async fn a_classifier_allow_still_obeys_the_hard_deny_floor() {
        // The policy refuses `process.spawn` outright and a classifier is sure
        // the call is fine. The call is refused, and the classifier is not even
        // asked: an estimate about an impossible call is work done to learn
        // nothing, and a record that asked anyway invites "it agreed" to be read
        // as the reason the call ran.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("shell.run", &[Capability::PROCESS_SPAWN], ToolSideEffect::Destructive)],
            store.clone(),
        );
        let classifier = Scripted::allowing(1.0);
        let gateway = judged_gateway(
            tools.clone(),
            PolicyEngine::new()
                .with_managed_deny([Capability::new(Capability::PROCESS_SPAWN)])
                .with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]),
            store.clone(),
            classifier.clone(),
        );

        let error = gateway.execute(&sid(), &call("shell.run")).await.unwrap_err();

        assert!(matches!(error, ToolGatewayError::PolicyDenied { .. }), "{error:?}");
        assert!(tools.seen().is_empty(), "the tool never ran");
        assert!(classifier.seen().is_empty(), "the classifier was never asked");
        let recorded = decisions(&store).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].decision, PolicyDecision::Deny);
        assert_eq!(recorded[0].authorization_source, AuthorizationSource::Policy);
    }

    #[tokio::test]
    async fn a_classifier_deny_stops_an_otherwise_allowed_l2_call() {
        // The policy allows `network.request` and nothing else objects. The
        // classifier does — and a call the policy permitted does not run.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("web.request", &[Capability::NETWORK_REQUEST], ToolSideEffect::Network)],
            store.clone(),
        );
        let classifier = Scripted::denying();
        let gateway = judged_gateway(
            tools.clone(),
            allowing(Capability::NETWORK_REQUEST),
            store.clone(),
            classifier.clone(),
        );

        let error = gateway.execute(&sid(), &network_call()).await.unwrap_err();

        assert!(matches!(error, ToolGatewayError::PolicyDenied { .. }), "denied: {error:?}");
        assert!(tools.seen().is_empty(), "the tool never ran");
        assert_eq!(classifier.seen().len(), 1, "the classifier was asked exactly once");
        let recorded = decisions(&store).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].decision, PolicyDecision::Deny);
        assert_eq!(recorded[0].authorization_source, AuthorizationSource::PermissionClassifier);
        assert!(
            recorded[0].reason.contains("permission classifier"),
            "the record names who refused: {}",
            recorded[0].reason
        );
    }

    #[tokio::test]
    async fn a_low_confidence_answer_asks_instead_of_allowing() {
        // The backend said allow — at 0.2. That is not evidence, and the call
        // stops for a person instead of running on a guess.
        let action = facts("web.request");
        let trace = ClassifierTrace {
            outcome: ClassifierOutcome::Allow,
            suggestion: Some(PermissionSuggestion {
                decision: SuggestionDecision::Allow,
                confidence: 0.2,
                risk: None,
                reversible: None,
                requires_confirmation: false,
                reason: "maybe".to_string(),
            }),
            ..ClassifierTrace::unavailable("scripted")
        };

        let record = rule(action, PermissionBaseline::new(PolicyDecision::Allow), Some(trace));

        assert_eq!(record.decision, PolicyDecision::Ask);
        assert!(record.reason.contains("not sure"), "{}", record.reason);
    }

    #[tokio::test]
    async fn a_timeout_abstention_or_protocol_error_never_allows() {
        for outcome in [ClassifierOutcome::Unclear, ClassifierOutcome::Unavailable] {
            let trace = ClassifierTrace { outcome, ..ClassifierTrace::unavailable("scripted") };
            let record = rule(
                facts("web.request"),
                PermissionBaseline::new(PolicyDecision::Allow),
                Some(trace),
            );
            assert_eq!(record.decision, PolicyDecision::Ask, "{outcome:?} must ask");
            assert_ne!(record.decision, PolicyDecision::Allow);
        }
    }

    #[tokio::test]
    async fn a_classifier_cannot_lower_the_computed_risk() {
        // An `L3` action judged `L0`: it stays `L3`, and its digest — the
        // identity a lease is bound to — does not move.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("shell.run", &[Capability::PROCESS_SPAWN], ToolSideEffect::Destructive)],
            store.clone(),
        );
        let gateway = judged_gateway(
            tools,
            allowing(Capability::PROCESS_SPAWN),
            store.clone(),
            Scripted::judging(RiskLevel::L0),
        );
        let before = gateway.action_facts("manual:permission-session", 0, &call("shell.run"));

        let result = gateway.execute(&sid(), &call("shell.run")).await.unwrap();

        let recorded = decisions(&store).await;
        let record = recorded.first().expect("a decision is recorded");
        assert_eq!(record.risk(), RiskLevel::L3, "an estimate can only raise");
        assert_eq!(record.action.digest, before.digest);
        assert_eq!(result.status, agent_protocol::tool::ToolStatus::Completed);
    }

    #[tokio::test]
    async fn a_classifier_that_raises_the_risk_changes_the_action_it_names() {
        // An `L2` request judged `L3`: the record says `L3`, and its digest is
        // the digest of the raised action — the one that was actually judged.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("web.request", &[Capability::NETWORK_REQUEST], ToolSideEffect::Network)],
            store.clone(),
        );
        let gateway = judged_gateway(
            tools,
            allowing(Capability::NETWORK_REQUEST),
            store.clone(),
            Scripted::judging(RiskLevel::L3),
        );
        let before = gateway.action_facts("manual:permission-session", 0, &network_call());

        gateway.execute(&sid(), &network_call()).await.unwrap();

        let recorded = decisions(&store).await;
        let record = recorded.first().expect("a decision is recorded");
        assert_eq!(record.risk(), RiskLevel::L3);
        assert_ne!(record.action.digest, before.digest);
    }

    #[tokio::test]
    async fn a_classifier_sees_no_secret_plaintext_or_whole_conversation() {
        // What the classifier is handed is the action, not the session: no
        // argument object, no conversation, and the secret only as the
        // reference the caller wrote.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("shell.run", &[Capability::PROCESS_SPAWN], ToolSideEffect::Destructive)],
            store.clone(),
        );
        let classifier = Scripted::allowing(1.0);
        let gateway = judged_gateway(
            tools,
            allowing(Capability::PROCESS_SPAWN),
            store.clone(),
            classifier.clone(),
        );

        gateway
            .execute(
                &sid(),
                &ToolInvocation::proposed(
                    "shell.run",
                    serde_json::json!({
                        "command": "curl -H 'Authorization: $TOKEN' https://x",
                        "note": "s3cr3t-plaintext",
                    }),
                ),
            )
            .await
            .unwrap();

        let seen = classifier.seen();
        assert_eq!(seen.len(), 1);
        let input = &seen[0];
        assert!(input.task_summary.is_none(), "no conversation is handed over");
        assert!(input.recent_actions.is_empty());
        assert!(input.untrusted_context_summary.is_none());
        let rendered = serde_json::to_string(&input.action).unwrap();
        assert!(!rendered.contains("s3cr3t-plaintext"), "{rendered}");
        assert!(!rendered.contains("\"arguments\""), "only the action facts are handed over");
    }

    #[tokio::test]
    async fn a_boundary_call_without_a_classifier_keeps_the_policy_answer() {
        // A build with no judge is not a build whose judge abstained. The
        // matrix leaves the policy's answer alone and the record says, by
        // naming no trace, that nothing independent was asked — which is the
        // truth a reader needs and the one an empty `unavailable` would hide.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("web.request", &[Capability::NETWORK_REQUEST], ToolSideEffect::Network)],
            store.clone(),
        );
        let (gateway, _, _) =
            gateway_over(tools.clone(), allowing(Capability::NETWORK_REQUEST), store.clone());

        let result = gateway.execute(&sid(), &network_call()).await.expect("the policy allows it");

        assert_eq!(result.status, agent_protocol::tool::ToolStatus::Completed);
        assert_eq!(tools.seen(), vec![true]);
        let recorded = decisions(&store).await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].decision, PolicyDecision::Allow);
        assert_eq!(recorded[0].authorization_source, AuthorizationSource::Policy);
        assert!(recorded[0].classifier.is_none(), "no judge was configured, and none is named");
    }

    #[tokio::test]
    async fn a_persons_grant_stands_when_no_classifier_answers() {
        // A grant is a person's answer, and the record has to say so: the
        // policy's `ask`, lifted by the session's lease, is not the same
        // authorization as a rule that never asked. With no judge in the build
        // this is the only source left, and it names the person.
        let store = Arc::new(FakeEventStore::new());
        let tools = Witnessed::new(
            vec![def("shell.run", &[Capability::PROCESS_SPAWN], ToolSideEffect::Destructive)],
            store.clone(),
        );
        let approvals = Arc::new(ApprovalService::new(store.clone()).with_session_scope(true));
        let gateway = GuardedToolProvider::new(
            tools.clone(),
            PolicyEngine::new()
                .with_rules([PolicyRule::ask(Capability::new(Capability::PROCESS_SPAWN))])
                .with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]),
            approvals.clone(),
            ResourceGuard::new(workspace()),
            Arc::new(InMemoryArtifactStore::default()),
        )
        .with_permission_service(Arc::new(PermissionService::new(store.clone())));
        let request = approvals
            .request(&sid(), Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
            .await
            .unwrap();
        approvals
            .grant(&sid(), &request.approval_id, ApprovalScope::AllowForSession, "user")
            .await
            .unwrap();

        let result = gateway.execute(&sid(), &call("shell.run")).await.expect("a person said yes");

        assert_eq!(result.status, agent_protocol::tool::ToolStatus::Completed);
        assert_eq!(tools.seen(), vec![true]);
        let recorded = decisions(&store).await;
        assert_eq!(recorded[0].decision, PolicyDecision::Allow);
        assert_eq!(recorded[0].authorization_source, AuthorizationSource::UserApproval);
        assert!(
            recorded[0].classifier.is_none(),
            "no judge was configured, so the record names none"
        );
    }

    #[test]
    fn an_old_policy_record_without_an_action_digest_still_reads() {
        let old = serde_json::json!({
            "decision": "allow",
            "capabilities": ["fs.read"],
            "refusals": {}
        });
        let record: agent_protocol::tool::ToolPolicyRecord =
            serde_json::from_value(old).expect("an old log entry reads");
        assert!(record.action_digest.is_none());
    }
}
