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
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{ApprovalId, EventId, EventSeq, SchemaVersion, SessionId};
use agent_protocol::permission::{ActionFacts, RiskLevel};
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

/// What a classifier concluded, as the decision that used it records it.
///
/// Defined here rather than beside the classifier because the record is the
/// thing that has to be storable: a classifier is replaced, rescheduled or
/// absent, and the log outlives every one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierOutcome {
    Allow,
    Ask,
    Deny,
    /// No classifier was reachable for this call.
    Unavailable,
    /// The classifier answered something this build cannot act on — a timeout,
    /// a protocol error, a low-confidence answer.
    Unclear,
}

/// One classifier call, as it is remembered.
///
/// The hashes are what make a classifier's advice reviewable without keeping
/// its input: the pack, the prompt and the input are reproducible from these
/// three values plus the classifier's own version.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClassifierTrace {
    pub route_id: String,
    pub revision: String,
    pub pack_version: String,
    pub prompt_hash: String,
    pub input_hash: String,
    pub latency_ms: u64,
    pub outcome: ClassifierOutcome,
}

impl ClassifierTrace {
    /// The trace for a call no classifier answered.
    pub fn unavailable(route_id: &str) -> Self {
        Self {
            route_id: route_id.to_string(),
            revision: String::new(),
            pack_version: String::new(),
            prompt_hash: String::new(),
            input_hash: String::new(),
            latency_ms: 0,
            outcome: ClassifierOutcome::Unavailable,
        }
    }
}

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
}

impl PermissionBaseline {
    pub fn new(decision: PolicyDecision) -> Self {
        Self { decision, refusals: BTreeMap::new(), reason: None }
    }

    /// A refusal that is not a policy refusal, with the sentence to record.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            decision: PolicyDecision::Deny,
            refusals: BTreeMap::new(),
            reason: Some(reason.into()),
        }
    }

    /// Record why, when the caller already has the sentence.
    pub fn because(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

impl From<agent_protocol::tool::ToolPolicyRecord> for PermissionBaseline {
    fn from(record: agent_protocol::tool::ToolPolicyRecord) -> Self {
        Self { decision: record.decision, refusals: record.refusals, reason: None }
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
    let decided = resolve(&baseline, classifier.as_ref());
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
fn resolve(baseline: &PermissionBaseline, classifier: Option<&ClassifierTrace>) -> Decided {
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
            source: AuthorizationSource::Policy,
            reason: baseline
                .reason
                .clone()
                .unwrap_or_else(|| "the policy allows this call".to_string()),
            approval_id: None,
        };
    };
    match trace.outcome {
        ClassifierOutcome::Deny => Decided {
            decision: PolicyDecision::Deny,
            source: AuthorizationSource::PermissionClassifier,
            reason: "the permission classifier refused the call".to_string(),
            approval_id: None,
        },
        // Abstention, a timeout, a protocol error and a low-confidence answer
        // are one case: none of them is evidence that the call is fine, so the
        // call stops and asks. A classifier that cannot answer does not inherit
        // the baseline's allow.
        ClassifierOutcome::Ask | ClassifierOutcome::Unclear | ClassifierOutcome::Unavailable => {
            Decided {
                decision: PolicyDecision::Ask,
                source: AuthorizationSource::PermissionClassifier,
                reason: format!("the permission classifier asked: {:?}", trace.outcome),
                approval_id: None,
            }
        }
        ClassifierOutcome::Allow => Decided {
            decision: baseline.decision,
            source: AuthorizationSource::PermissionClassifier,
            reason: "the permission classifier allowed the call".to_string(),
            approval_id: None,
        },
    }
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
