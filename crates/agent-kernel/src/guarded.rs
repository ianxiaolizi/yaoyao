//! GuardedToolProvider — the ToolGateway implementation that enforces policy,
//! approval, resource boundaries, and redaction around an inner tool provider.
//!
//! Flow for one invocation: resolve the tool's declared capabilities, evaluate
//! policy with the invocation's tool/path/command context, then
//!   * `deny`  -> refuse (no execution, no approval raised),
//!   * `ask`   -> raise a durable approval request and yield (the caller ends
//!     the run; the decision resumes it later — the resume loop is M5),
//!   * `allow` -> check the file boundary, run the tool, redact the result, and
//!     offload oversized content to the artifact store.

use std::sync::Arc;

use agent_ports::artifact_store::ArtifactStore;
use agent_ports::execution_environment::SandboxMode;
use agent_ports::permission_classifier::PermissionClassifier;
use agent_ports::secret_provider::SecretProvider;
use agent_ports::tool_gateway::{ToolGateway, ToolGatewayError};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolResult, ToolSideEffect,
};
use agent_ports::workspace_snapshot::WorkspaceSnapshotter;
use agent_protocol::ids::{ApprovalId, SessionId};
use agent_protocol::permission::{ActionFacts, AuthorizationSubject};
use agent_protocol::permissions::{Capability, PolicyDecision};
use agent_protocol::redactor::Redactor;
use agent_protocol::tool::ToolPolicyRecord;

use crate::approval::ApprovalService;

mod approved;
use crate::permission::{PermissionBaseline, PermissionDecisionRecord, PermissionService};
use crate::policy::{PolicyContext, PolicyEngine};
use crate::resource_guard::ResourceGuard;
use crate::scrub::{
    PREVIEW_HEAD_BYTES, PREVIEW_TAIL_BYTES, ResolvedSecret, exceeds_offload_limits,
    offload_preview, scrub_provider_error, scrub_text,
};
use crate::tool_schema::{SchemaCheck, SchemaGap};

/// A result past either limit is offloaded to the artifact store instead of
/// entering the model context whole. Two limits rather than one because they
/// catch different shapes: a minified bundle is enormous and barely long, a
/// generated table is long and not enormous.
const OFFLOAD_MAX_BYTES: usize = 50 * 1024;

/// How many registered tool names a "no such tool" message will list.
///
/// A server can register hundreds, and the message is a sentence and not a
/// manifest: enough names to show what the surface looks like, and a count that
/// says how many more there are.
const SURFACE_SAMPLE: usize = 12;

/// Policy- and approval-guarded tool executor.
pub struct GuardedToolProvider {
    inner: Arc<dyn ToolProvider>,
    policy: PolicyEngine,
    approvals: Arc<ApprovalService>,
    guard: ResourceGuard,
    redactor: Redactor,
    artifacts: Arc<dyn ArtifactStore>,
    /// Resolves `secret://` references in tool arguments at execution time.
    ///
    /// `None` is a run that resolves none: a reference is then refused rather
    /// than passed to the tool as if it were the value it names.
    secrets: Option<Arc<dyn SecretProvider>>,
    /// Where the one decision behind each call is written.
    ///
    /// `None` is a gateway assembled without a ledger — a fixture, or a unit
    /// test of some other layer. Such a gateway still makes the same decision
    /// (see [`crate::permission::rule`]); it just keeps no record of it. The
    /// assembled binary always hands one over.
    permissions: Option<Arc<PermissionService>>,
    /// Who is asked about a boundary action, when the policy does not refuse it.
    ///
    /// `None` is a build with no independent judge: the decision names no trace
    /// and the policy's own answer stands. A judge that *is* installed and then
    /// cannot answer is the case that stops for a person (CONTRACT §7 #177).
    classifier: Option<Arc<dyn PermissionClassifier>>,
    /// Where a recovery point is taken before a call that may change files
    /// (B8). `None` is a gateway assembled without one — a fixture, or a unit
    /// test of another layer. The binary always hands one over.
    snapshot: Option<Arc<dyn WorkspaceSnapshotter>>,
    /// The backend this gateway's calls would run under (B5).
    ///
    /// The gateway is the only layer holding the action *and* the backend at
    /// once, and the isolation floor is a question about both. It defaults to
    /// [`SandboxMode::LocalProcess`] — the documented backend of an unnamed run
    /// — because a gateway that was not told what confines it must not assume
    /// something does: the conservative answer refuses a high-risk command, and
    /// a caller that really is confined says so with [`Self::with_sandbox_mode`].
    sandbox_mode: SandboxMode,
    /// How many bytes of one result stay inline; larger results are offloaded.
    offload_max_bytes: usize,
    /// The compiled declarations calls are checked against, kept between calls
    /// because a tool's schema is the same object every time it is called.
    schema: SchemaCheck,
}

impl GuardedToolProvider {
    pub fn new(
        inner: Arc<dyn ToolProvider>,
        policy: PolicyEngine,
        approvals: Arc<ApprovalService>,
        guard: ResourceGuard,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        Self {
            inner,
            policy,
            approvals,
            guard,
            redactor: Redactor::new(),
            artifacts,
            secrets: None,
            permissions: None,
            classifier: None,
            snapshot: None,
            sandbox_mode: SandboxMode::LocalProcess,
            offload_max_bytes: OFFLOAD_MAX_BYTES,
            schema: SchemaCheck::new(),
        }
    }

    /// Write one `permission/decision` before each call this gateway makes.
    ///
    /// The service is handed over rather than built here because the event
    /// store is assembly's decision, and a gateway that opened a second writer
    /// to the same log could land a decision on a stream this run did not read.
    pub fn with_permission_service(mut self, permissions: Arc<PermissionService>) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// Ask an independent classifier about every boundary action (B3).
    pub fn with_permission_classifier(mut self, classifier: Arc<dyn PermissionClassifier>) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// Say what backend these calls would run under (B5).
    ///
    /// Assembly reads it from the environment it built
    /// ([`agent_ports::execution_environment::ExecutionEnvironment::mode`]), so
    /// the floor and the process the call reaches agree by construction. The
    /// default is the local process, which is what an unnamed run gets and the
    /// one mode that cannot confine a high-risk call.
    pub fn with_sandbox_mode(mut self, mode: SandboxMode) -> Self {
        self.sandbox_mode = mode;
        self
    }

    /// Resolve `secret://` references in arguments through `secrets`.
    ///
    /// The plaintext is injected immediately before the tool runs and kept
    /// only for the length of that call: the invocation the caller built, the
    /// log it was written to, and the approval summary all still hold the
    /// reference. What comes back is a *copy* whose argument strings have the
    /// values substituted, plus the values themselves so the result and any
    /// provider error can be scrubbed with them.
    pub fn with_secrets(mut self, secrets: Arc<dyn SecretProvider>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    /// Take a recovery point before every call that may change files.
    ///
    /// The snapshot is taken before the provider runs and compared against the
    /// workspace after it; a capture that fails stops the call (CONTRACT §7
    /// #182). `None` — the default — is a gateway with no snapshotter: the same
    /// decisions, no recovery points.
    pub fn with_snapshotter(mut self, snapshot: Arc<dyn WorkspaceSnapshotter>) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    /// Use `redactor` for results, arguments and approval summaries.
    ///
    /// The default is the same redactor the loop uses; this exists so the
    /// process-level setting (`HighEntropy` policy) reaches the boundary that
    /// writes tool results into the log, rather than being decided twice in
    /// two places.
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Bound one result to `max_bytes` inline.
    ///
    /// The caller derives this from the run's working set
    /// ([`agent_protocol::profile::RunBudget::tool_output_limit_bytes`]): one
    /// step's output that alone outgrows the window cannot be compacted away
    /// afterwards, so the bound has to match the window the run actually has.
    pub fn with_offload_limit(mut self, max_bytes: usize) -> Self {
        self.offload_max_bytes = max_bytes.max(PREVIEW_HEAD_BYTES + PREVIEW_TAIL_BYTES);
        self
    }

    fn capabilities_for(&self, tool_name: &str) -> Vec<Capability> {
        self.inner
            .definitions()
            .into_iter()
            .find(|d| d.name == tool_name)
            .map(|d| d.capability_requirements)
            .unwrap_or_default()
    }

    fn is_known(&self, tool_name: &str) -> bool {
        self.inner.definitions().into_iter().any(|d| d.name == tool_name)
    }

    /// Raise the approval a boundary call needs, and say it is pending.
    ///
    /// The question is bound to the action the gateway just judged, not to the
    /// capability: what a person is asked about is the exact fingerprint they
    /// will be answered about. (CONTRACT §7 #178)
    async fn request_approval(
        &self,
        session_id: &SessionId,
        invocation: &ToolInvocation,
        action: &ActionFacts,
    ) -> Result<ToolGatewayError, ToolGatewayError> {
        let summary = crate::action::summarize_arguments(&self.redactor, invocation);
        let req = self
            .approvals
            .request_for_action(session_id, action, &invocation.tool_name, &summary)
            .await
            .map_err(|e| ToolGatewayError::Approval(e.to_string()))?;
        Ok(ToolGatewayError::ApprovalPending {
            tool: invocation.tool_name.clone(),
            approval_id: req.approval_id.to_string(),
        })
    }

    /// Refuse a call whose arguments do not match the tool's declaration.
    ///
    /// Before policy, because the two answers differ in what they cost: a
    /// person is not asked to approve a call that could never run, and a policy
    /// review is not spent on one. The gateway is where this belongs rather
    /// than each provider, because the declaration is what the *model* was
    /// shown — the surface and the check against it should be the same list.
    ///
    /// A provider still owns its own semantic checks: a schema can say
    /// `fs.edit` takes two strings, and only the tool can say the anchor
    /// appears exactly once.
    fn check_arguments(&self, invocation: &ToolInvocation) -> Result<(), ToolGatewayError> {
        let Some(definition) =
            self.inner.definitions().into_iter().find(|d| d.name == invocation.tool_name)
        else {
            // `is_known` already answered; a provider that changed its surface
            // between the two calls is a race this cannot do better than.
            return Err(self.unknown_tool(&invocation.tool_name));
        };
        match self.schema.check(&definition.input_schema, &invocation.arguments) {
            Ok(()) => Ok(()),
            Err(SchemaGap::Arguments(problems)) => Err(ToolGatewayError::InvalidArguments {
                tool: invocation.tool_name.clone(),
                problems,
            }),
            Err(SchemaGap::Declaration(reason)) => {
                Err(ToolGatewayError::InvalidSchema { tool: invocation.tool_name.clone(), reason })
            }
        }
    }

    /// A refusal of a name nobody registered, carrying the surface to look at.
    ///
    /// The gateway is the only layer holding the whole surface, and a bare
    /// "unknown tool" leaves the caller the one question they cannot answer
    /// alone: whether the name is misspelled, or the family behind it was never
    /// registered at all. Saying what *is* registered answers both — the name is
    /// visibly outside the surface, or the neighbour it was meant to be is
    /// visibly inside it. It is what `plugin list` already does one layer up,
    /// for the same reason.
    fn unknown_tool(&self, tool_name: &str) -> ToolGatewayError {
        ToolGatewayError::UnknownTool {
            tool: tool_name.to_string(),
            reason: self.surface_note(tool_name),
        }
    }

    /// What the surface has to say to a caller who asked for something it does
    /// not have.
    fn surface_note(&self, wanted: &str) -> String {
        let registered: Vec<String> =
            self.inner.definitions().into_iter().map(|d| d.name).collect();
        let total = registered.len();
        if total == 0 {
            return "the run has no tools registered".to_string();
        }
        // Closest names first, so a long surface still shows what the caller was
        // most likely reaching for rather than an arbitrary dozen.
        let mut ordered = registered;
        ordered.sort_by_key(|name| std::cmp::Reverse(shared_prefix_len(name, wanted)));
        if total <= SURFACE_SAMPLE {
            return format!("the run registers {total} tool(s): {}", ordered.join(", "));
        }
        let shown = ordered[..SURFACE_SAMPLE].join(", ");
        format!("the run registers {total} tools; the nearest are: {shown}")
    }

    /// Run an invocation whose approval has already been decided.
    ///
    /// The policy question was asked and answered before this call. Asking it
    /// again would raise a *second* approval for a decision that already
    /// exists, and the session would wait forever on a queue nobody drains.
    /// What still runs here is everything that is not that question — the
    /// known-tool check, the path boundary, redaction, offload. An approved
    /// call is allowed to run; it is not exempt from the rules about running.
    ///
    /// Deliberately not a method on [`ToolGateway`]: its other implementers
    /// have no approval state to consult, and a trait method would make every
    /// one of them grow a knob that does nothing. `resume` is the only caller,
    /// and it holds the concrete type for exactly this reason.
    ///
    /// The approval is checked against the action *now*, not against the one
    /// the approval was raised about: a fingerprint the person answered is the
    /// only thing that lets this call through, and the check is what makes a
    /// stale grant — a different path, a moved task revision, an `L3` answer
    /// already spent — a refused call rather than a quiet reuse. (CONTRACT §7
    /// #178)
    pub async fn execute_approved(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError> {
        if !self.is_known(&invocation.tool_name) {
            return Err(self.unknown_tool(&invocation.tool_name));
        }
        self.check_arguments(invocation)?;
        let ctx = self.policy_context(invocation);
        self.check_paths(invocation, &ctx)?;
        let action = self.approved_action(session_id, approval_id, invocation).await?;

        // The isolation floor holds on the resume path too: a standing answer
        // about a call is an answer about the call, and it cannot confine it.
        if let Some(floor) = crate::permission::isolation_floor(&action, self.sandbox_mode) {
            let reason =
                floor.reason.clone().unwrap_or_else(|| "the sandbox refused this call".to_string());
            self.record(session_id, &action, floor.clone(), None).await?;
            return Err(ToolGatewayError::PolicyDenied {
                tool: invocation.tool_name.clone(),
                reason,
                policy: Box::new(
                    ToolPolicyRecord::denied(
                        self.capabilities_for(&invocation.tool_name),
                        floor
                            .refusals
                            .iter()
                            .map(|(cap, layer)| (cap.clone(), *layer))
                            .collect::<Vec<_>>(),
                    )
                    .about(action.digest.clone()),
                ),
            });
        }

        let covers = self
            .approvals
            .covers_action(session_id, approval_id, &action)
            .await
            .map_err(|e| ToolGatewayError::Approval(e.to_string()))?;
        if !covers {
            let reason = "the approval does not cover this call: the action changed since it was \
                          answered, or the answer has already been spent"
                .to_string();
            let capabilities = self.capabilities_for(&invocation.tool_name);
            self.record(session_id, &action, PermissionBaseline::refused(reason.clone()), None)
                .await?;
            return Err(ToolGatewayError::PolicyDenied {
                tool: invocation.tool_name.clone(),
                reason,
                policy: Box::new(
                    ToolPolicyRecord::denied(
                        capabilities.clone(),
                        self.policy.refusals(&capabilities, Some(&ctx), &[]),
                    )
                    .about(action.digest.clone()),
                ),
            });
        }
        // The decision this call runs under, written before it runs: a resume
        // is a dispatch like any other, and a reader joining the log should see
        // the answer that permitted it by name.
        let decision = self
            .record(
                session_id,
                &action,
                PermissionBaseline::new(PolicyDecision::Allow)
                    .approve(true)
                    .approval(approval_id.clone()),
                None,
            )
            .await?;
        let record = ToolPolicyRecord::allowed(self.capabilities_for(&invocation.tool_name))
            .about(&action.digest);
        Ok(self.run_resolved(session_id, &action, &decision, invocation).await?.with_policy(record))
    }

    /// Resolve references, run the tool, scrub what comes back.
    ///
    /// The one path both execution entry points share, so an approved call and
    /// an allowed one cannot disagree about whether a reference was resolved
    /// or whether its value was scrubbed afterwards. The decision is handed in
    /// rather than asked for here: this runs only because one already exists,
    /// and the access record names the digest it was made about.
    async fn run_resolved(
        &self,
        session_id: &SessionId,
        action: &ActionFacts,
        decision: &PermissionDecisionRecord,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError> {
        let (resolved_invocation, secrets) =
            crate::secret_refs::resolve_arguments(self.secrets.as_deref(), invocation)?;
        self.finish_resolved(session_id, action, decision, &resolved_invocation, &secrets).await
    }

    /// Run an invocation whose references are already resolved.
    ///
    /// The access is written down before the tool runs, and a ledger that
    /// cannot take the write refuses the call: a secret used with no record of
    /// the action that permitted it is the one outcome this gate exists to
    /// prevent.
    async fn finish_resolved(
        &self,
        session_id: &SessionId,
        action: &ActionFacts,
        decision: &PermissionDecisionRecord,
        resolved_invocation: &ToolInvocation,
        secrets: &[ResolvedSecret],
    ) -> Result<ToolResult, ToolGatewayError> {
        crate::secret_refs::record_access(
            self.permissions.as_deref(),
            session_id,
            action,
            decision,
            secrets,
        )
        .await?;
        let recovery = crate::snapshot::RecoveryContext {
            snapshotter: self.snapshot.as_deref(),
            permissions: self.permissions.as_deref(),
            guard: &self.guard,
            session_id,
        };
        let dispatch = self.inner.side_effect(&action.tool_name);
        let point = recovery.capture(action, resolved_invocation, dispatch).await?;
        let result = self.inner.invoke(resolved_invocation).await;
        recovery.settle(point, &action.tool_name).await?;
        let result =
            result.map_err(|error| ToolGatewayError::Tool(scrub_provider_error(error, secrets)))?;
        // The decision is the gate's to state. A provider that filled the field
        // is stating a decision it did not make, so it is cleared here and only
        // a caller that actually decided — `execute` — sets it afterwards.
        let mut finalized = self.finalize(result, secrets);
        finalized.policy = None;
        Ok(finalized)
    }

    /// The policy's view of the call.
    ///
    /// The path field is filled from the first path argument the provider
    /// declares and the call actually carries. A rule narrowed by path sees the
    /// same value the resource boundary is about to check (K-08).
    fn policy_context<'a>(&self, invocation: &'a ToolInvocation) -> PolicyContext<'a> {
        PolicyContext {
            tool_name: invocation.tool_name.as_str(),
            path: self
                .inner
                .path_arguments(&invocation.tool_name)
                .into_iter()
                .find_map(|key| invocation.arguments.get(&key).and_then(|v| v.as_str())),
            command: invocation.arguments.get("command").and_then(|v| v.as_str()),
        }
    }

    /// The action one call proposes, as a permission decision will see it.
    ///
    /// Built in [`crate::action`] so a lease binds the fingerprint a result is under.
    pub fn action_facts(
        &self,
        task_id: &str,
        task_revision: u64,
        invocation: &ToolInvocation,
    ) -> ActionFacts {
        crate::action::action_facts(
            self.inner.as_ref(),
            &self.capabilities_for(&invocation.tool_name),
            task_id,
            task_revision,
            invocation,
        )
    }

    /// Write the decision this call is made on, or refuse to make the call.
    ///
    /// Every door out of `execute` comes through here — allow, ask, deny, and
    /// the boundary refusal before the policy is even asked — which is what
    /// makes "exactly one decision per call" a property of the code rather than
    /// of each arm remembering to do it. A write that fails is
    /// [`ToolGatewayError::PermissionAudit`] and the call does not run.
    async fn record(
        &self,
        session_id: &SessionId,
        action: &ActionFacts,
        baseline: PermissionBaseline,
        classifier: Option<crate::permission::ClassifierTrace>,
    ) -> Result<PermissionDecisionRecord, ToolGatewayError> {
        let Some(service) = &self.permissions else {
            // A gateway assembled without a ledger still makes the same
            // decision; it just has nowhere to put it.
            return Ok(crate::permission::rule(action.clone(), baseline, classifier));
        };
        service.decide(session_id, action.clone(), baseline, classifier).await.map_err(|error| {
            ToolGatewayError::PermissionAudit {
                tool: action.tool_name.clone(),
                reason: error.to_string(),
            }
        })
    }

    /// Check every argument the provider declares as a path.
    ///
    /// The loop is over the declaration, not over a key named `path`: a tool
    /// that carries its path under `file` is invisible to a hard-coded lookup,
    /// and the boundary then never sees the write at all (K-08). An argument
    /// the tool did not send is skipped — a declared key is not a required one.
    fn check_paths(
        &self,
        invocation: &ToolInvocation,
        ctx: &PolicyContext<'_>,
    ) -> Result<(), ToolGatewayError> {
        // Whether this counts as a write comes from what the tool declares
        // about itself, not from its name. A name-matching rule silently stops
        // covering every write tool added after it was written.
        //
        // Only an affirmative claim is taken at face value: `Read` and `Pure`
        // are the two answers that say "this touches nothing", and a provider
        // that answers neither is treated as a write. Reading "no answer" as
        // "harmless" opens the boundary for exactly the tools nobody has
        // classified — which is what `ToolProvider::side_effect` warns callers
        // about in its own words ("treat an unknown tool as `Destructive`").
        let write = !matches!(
            self.inner.side_effect(ctx.tool_name),
            Some(ToolSideEffect::Read) | Some(ToolSideEffect::Pure)
        );
        for key in self.inner.path_arguments(&invocation.tool_name) {
            let Some(path) = invocation.arguments.get(&key).and_then(|v| v.as_str()) else {
                continue;
            };
            self.guard.check(path, write).map_err(|e| ToolGatewayError::ResourceDenied {
                tool: ctx.tool_name.to_string(),
                reason: e.to_string(),
            })?;
        }
        Ok(())
    }

    /// Redact a result, then decide how much of it the model may see.
    ///
    /// A value this call resolved is scrubbed before the pattern-based
    /// redactor: the redactor knows what a secret *looks* like, and this knows
    /// the exact bytes of the one that was just handed to the tool. Neither
    /// subsumes the other, and the result is the one place a tool can echo
    /// what it was given.
    fn finalize(&self, result: ToolResult, secrets: &[ResolvedSecret]) -> ToolResult {
        let (content, mut redaction) = self.redactor.redact(&result.content);
        let (content, patterns) = scrub_text(&content, secrets);
        redaction.patterns.extend(patterns);
        redaction.patterns.sort();
        redaction.patterns.dedup();
        redaction.redacted = redaction.redacted || !redaction.patterns.is_empty();
        let mut finalized = result;
        finalized.content = content;
        finalized.redaction_metadata = redaction;

        if !exceeds_offload_limits(&finalized.content, self.offload_max_bytes) {
            return finalized;
        }
        // The count describes the text before any preview replaced it: a
        // reader comparing results must not measure the head and tail of a
        // preview. A provider that already stated the count keeps its number.
        if finalized.facts.line_count.is_none() {
            finalized.facts.line_count = Some(finalized.content.lines().count() as u64);
        }
        // Best effort, and specifically not a tool failure: the tool ran, and
        // its output is right here. Turning a storage problem into a failed
        // call would throw away a result that is in hand, so on error the full
        // text simply stays inline. The cost is context, which is recoverable;
        // the alternative loses the answer, which is not.
        if let Ok(artifact) = self.artifacts.put(finalized.content.as_bytes(), "text/plain") {
            finalized.content = offload_preview(&finalized.content, &artifact);
            finalized.artifacts.push(artifact);
        }
        finalized
    }
}
#[async_trait::async_trait]
impl ToolGateway for GuardedToolProvider {
    async fn execute(
        &self,
        session_id: &SessionId,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError> {
        if !self.is_known(&invocation.tool_name) {
            return Err(self.unknown_tool(&invocation.tool_name));
        }
        self.check_arguments(invocation)?;
        let capabilities = self.capabilities_for(&invocation.tool_name);
        let ctx = self.policy_context(invocation);
        // The action, as every layer below this point will see it. Built once,
        // before anything decides: the digest a decision is recorded under is
        // then the one the result carries, by construction.
        let subject = invocation
            .authorization_subject
            .clone()
            .unwrap_or_else(|| AuthorizationSubject::manual(session_id.as_str()));
        let mut action = self.action_facts(&subject.task_id, subject.task_revision, invocation);

        // The workspace boundary is asked before the policy, and its refusal is
        // recorded before it is returned. A refusal with no decision beside it
        // is the one case an audit cannot tell from a call nobody ever made —
        // and no grant can lift this one, so asking it early changes nothing a
        // grant could have said.
        if let Err(refusal) = self.check_paths(invocation, &ctx) {
            let reason = refusal.to_string();
            self.record(session_id, &action, PermissionBaseline::refused(reason), None).await?;
            return Err(refusal);
        }

        // Ask the engine with nothing granted first. A grant can only turn an
        // `ask` into an `allow` — never a `deny` — so when the answer is not
        // `ask`, the session's log cannot change it, and reading that log would
        // be work done to learn nothing. It is not free either: resolving the
        // grants folds the session's whole stream, and this runs on every tool
        // call. The second evaluation is the one that costs, and it happens
        // only for the calls that were going to stop and ask anyway.
        let mut decision = self.policy.evaluate_with_ctx(&capabilities, Some(&ctx), &[]);
        let ungranted = decision;
        if decision == PolicyDecision::Ask {
            // Only leases that cover *this* action are offered to the engine.
            // A grant is an answer about one fingerprint, and the policy engine
            // sees a capability list — so the narrowing has to happen here.
            let grants = self
                .approvals
                .leases_in_effect(session_id)
                .await
                .map_err(|e| ToolGatewayError::Approval(e.to_string()))?
                .into_iter()
                .filter(|lease| lease.matches_action(&action))
                .map(|lease| lease.capability)
                .collect::<Vec<_>>();
            decision = self.policy.evaluate_with_ctx(&capabilities, Some(&ctx), &grants);
        }
        // Whether a person's grant is what allowed this call, as opposed to a
        // rule that allows the capability outright. The difference decides what
        // an absent classifier means: a grant is an answer, a rule is not.
        let approved = ungranted != PolicyDecision::Allow && decision == PolicyDecision::Allow;

        // The isolation floor (B5), before any estimate and after every grant:
        // an action that needs a confining backend and does not have one is
        // refused, and a rule, a person's answer or a classifier's opinion is an
        // answer to a different question. Lifting it is not expressible here.
        let isolation = crate::permission::isolation_floor(&action, self.sandbox_mode);
        let decision = if isolation.is_some() { PolicyDecision::Deny } else { decision };

        // The independent judgement of a boundary action, taken before anything
        // is written or run (B3). A policy deny never reaches it: a refusal is
        // final, and an estimate about a call nobody may make is work done to
        // learn nothing. The risk a classifier states can only raise the action
        // it is judging — and raising it re-computes the digest, so the record
        // names the action that was actually judged.
        let classifier = crate::permission::judge(
            self.classifier.as_ref(),
            session_id,
            &action,
            decision,
            &capabilities,
        )
        .await;
        if let Some(risk) =
            classifier.as_ref().and_then(|trace| trace.suggestion.as_ref()).and_then(|s| s.risk)
        {
            action = action.raised_to(risk);
        }
        // The door this call leaves by is the matrix's answer, not the
        // policy's: a classifier's `deny` stops a call the policy allowed, and
        // its `ask` holds one. The baseline each arm records is still the
        // policy's own answer, so the written decision re-derives to the same
        // verdict it was acted on.
        let policy_answer = decision;
        let decision =
            crate::permission::verdict(&action, policy_answer, approved, classifier.as_ref());

        match decision {
            PolicyDecision::Deny => {
                // A tool that declares nothing leaves `refusals` nothing to
                // name — there is no capability to report a layer for — so the
                // sentence is written here instead of assembled from layers.
                //
                // Asked with no grants, because a grant cannot turn anything
                // into a `deny`: every layer this can name refuses the same way
                // it did in the evaluation above, whichever that was.
                // A call the sandbox floor refused names that layer rather than
                // the policy's — nothing about it was forbidden by a rule, and
                // the sentence a person reads should say where the fix is.
                let record = match &isolation {
                    Some(floor) => ToolPolicyRecord::denied(
                        capabilities.clone(),
                        floor
                            .refusals
                            .iter()
                            .map(|(cap, layer)| (cap.clone(), *layer))
                            .collect::<Vec<_>>(),
                    ),
                    None => ToolPolicyRecord::denied(
                        capabilities.clone(),
                        self.policy.refusals(&capabilities, Some(&ctx), &[]),
                    ),
                };
                let denied_by_classifier = classifier.as_ref().is_some_and(|trace| {
                    trace.outcome == crate::permission::ClassifierOutcome::Deny
                });
                let reason = if let Some(floor) = &isolation {
                    floor
                        .reason
                        .clone()
                        .unwrap_or_else(|| "the sandbox refused this call".to_string())
                } else if denied_by_classifier {
                    // The policy allowed this call; the refusal is the
                    // classifier's, and the record says so with its own words.
                    classifier
                        .as_ref()
                        .and_then(|trace| trace.suggestion.as_ref())
                        .filter(|suggestion| !suggestion.reason.is_empty())
                        .map(|suggestion| {
                            format!("the permission classifier refused: {}", suggestion.reason)
                        })
                        .unwrap_or_else(|| "the permission classifier refused the call".to_string())
                } else if capabilities.is_empty() {
                    "declares no capabilities, and an undeclared surface is not an unlimited one"
                        .to_string()
                } else {
                    crate::policy::deny_reason(&record.refusals)
                };
                // A refusal the classifier made is recorded against the
                // policy's own answer plus the trace, so the written decision
                // re-derives to "the classifier refused" rather than to a
                // policy deny nobody wrote.
                let baseline = if let Some(floor) = &isolation {
                    floor.clone()
                } else if denied_by_classifier {
                    PermissionBaseline::new(policy_answer)
                } else {
                    PermissionBaseline::from(record.clone())
                };
                self.record(
                    session_id,
                    &action,
                    baseline.because(reason.clone()),
                    classifier.clone(),
                )
                .await?;
                Err(ToolGatewayError::PolicyDenied {
                    tool: invocation.tool_name.clone(),
                    reason,
                    policy: Box::new(record.about(action.digest.clone())),
                })
            }
            PolicyDecision::Ask => {
                // The held call's decision is written before the question is
                // asked: "we asked" is itself a decision about this action, and
                // a session that crashed between raising the approval and
                // recording it would otherwise hold a question with no reason
                // in the log.
                self.record(
                    session_id,
                    &action,
                    PermissionBaseline::new(PolicyDecision::Ask),
                    classifier.clone(),
                )
                .await?;
                Err(self.request_approval(session_id, invocation, &action).await?)
            }
            PolicyDecision::Allow => {
                // Stated once and stamped on whatever this arm returns: the
                // arms leave by three different doors (no secret, secret
                // already granted, secret approved), and a record that only
                // two of them carried would be worse than none.
                let record = ToolPolicyRecord::allowed(capabilities.clone()).about(&action.digest);
                if !crate::secret_refs::references_secret(&invocation.arguments) {
                    let decision = self
                        .record(
                            session_id,
                            &action,
                            PermissionBaseline::new(PolicyDecision::Allow).approve(approved),
                            classifier.clone(),
                        )
                        .await?;
                    return Ok(self
                        .finish_resolved(session_id, &action, &decision, invocation, &[])
                        .await?
                        .with_policy(record));
                }
                // Whether this run could resolve the reference at all, asked
                // without producing a value: a call that could never run is not
                // one a person should be asked about, and a value nobody has
                // approved yet is not one this process should hold.
                crate::secret_refs::check_arguments(self.secrets.as_deref(), invocation)?;
                // A call that names a secret is a call a person should see
                // before it runs. The reference is all the summary can show
                // them, and the tool's own capability says whether they
                // already said yes to it in this scope.
                let secret_granted =
                    crate::secret_refs::secret_use_granted(&self.approvals, session_id, &action)
                        .await?;
                let baseline = if secret_granted {
                    PermissionBaseline::new(PolicyDecision::Allow).approve(approved)
                } else {
                    // The policy allowed the capability; a person has not yet
                    // seen this call name a secret. The decision states the
                    // call as it stands: held, not allowed.
                    PermissionBaseline::new(PolicyDecision::Ask)
                        .because("the call names a secret; a person sees it before it runs")
                };
                let decision =
                    self.record(session_id, &action, baseline, classifier.clone()).await?;
                if !secret_granted {
                    return Err(self.request_approval(session_id, invocation, &action).await?);
                }
                // Only now is the value produced. The decision exists, the
                // access is about to be written down, and the value lives from
                // here to the end of the call (CONTRACT §7 #181).
                let (resolved_invocation, secrets) =
                    crate::secret_refs::resolve_arguments(self.secrets.as_deref(), invocation)?;
                Ok(self
                    .finish_resolved(session_id, &action, &decision, &resolved_invocation, &secrets)
                    .await?
                    .with_policy(record))
            }
        }
    }

    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    /// What the model is offered: the executable surface minus every tool the
    /// policy could refuse for every possible call.
    ///
    /// The executable surface above is deliberately whole: `agent tool` and a
    /// resumed call name their tool directly, and both are owed the refusal
    /// and the layer that made it. A model choosing among tools is owed the
    /// opposite — a surface in which every entry can run.
    fn model_definitions(&self) -> Vec<ToolDefinition> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|definition| self.policy.could_allow(&definition.capability_requirements))
            .collect()
    }
}

/// How many leading characters two names have in common.
fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}
#[cfg(test)]
mod tests;
