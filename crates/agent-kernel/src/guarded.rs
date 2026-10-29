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
use agent_ports::secret_provider::{SecretProvider, SecretProviderError};
use agent_ports::tool_gateway::{ToolGateway, ToolGatewayError};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
};
use agent_protocol::ids::SessionId;
use agent_protocol::permissions::{ArtifactRef, Capability, PolicyDecision, SecretRef};
use agent_protocol::redactor::Redactor;
use agent_protocol::tool::ToolErrorKind;

use crate::approval::ApprovalService;
use crate::policy::{PolicyContext, PolicyEngine, Refusal};
use crate::resource_guard::ResourceGuard;
use crate::tool_schema::{SchemaCheck, SchemaGap};

/// A result past either limit is offloaded to the artifact store instead of
/// entering the model context whole. Two limits rather than one because they
/// catch different shapes: a minified bundle is enormous and barely long, a
/// generated table is long and not enormous.
const OFFLOAD_MAX_BYTES: usize = 50 * 1024;
const OFFLOAD_MAX_LINES: usize = 2_000;

/// How many registered tool names a "no such tool" message will list.
///
/// A server can register hundreds, and the message is a sentence and not a
/// manifest: enough names to show what the surface looks like, and a count that
/// says how many more there are.
const SURFACE_SAMPLE: usize = 12;

/// How much of an offloaded result still reaches the model: enough to recognise
/// what it is and where it went wrong, not enough to be the thing itself.
const PREVIEW_HEAD_BYTES: usize = 2_048;
const PREVIEW_TAIL_BYTES: usize = 512;

/// The scheme a tool argument uses to name a secret instead of carrying one.
const SECRET_SCHEME: &str = "secret://";

/// The shortest resolved value the result scrubber replaces.
///
/// A backstop, not the boundary: the arguments never carried the value, and a
/// tool that echoes it is the case this covers. Below this length a value is
/// indistinguishable from an ordinary word, and replacing every occurrence
/// would corrupt perfectly good output while protecting nothing.
const MIN_SECRET_SCRUB_CHARS: usize = 8;

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
            offload_max_bytes: OFFLOAD_MAX_BYTES,
            schema: SchemaCheck::new(),
        }
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

    /// Whether any string in `value` names a secret.
    fn references_secret(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(text) => text.contains(SECRET_SCHEME),
            serde_json::Value::Array(items) => items.iter().any(Self::references_secret),
            serde_json::Value::Object(map) => map.values().any(Self::references_secret),
            _ => false,
        }
    }

    /// Resolve every reference in the arguments, refusing the whole call if one
    /// cannot be resolved.
    ///
    /// All-or-nothing: a call that half-resolved would hand the tool a mix of
    /// values and reference strings, and the tool would act on the mix. The
    /// returned invocation is a copy — the caller's own value, and the event
    /// already written from it, keep the references.
    fn resolve_arguments(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<(ToolInvocation, Vec<ResolvedSecret>), ToolGatewayError> {
        if !Self::references_secret(&invocation.arguments) {
            return Ok((invocation.clone(), Vec::new()));
        }
        let mut resolved = Vec::new();
        let arguments =
            self.resolve_value(&invocation.tool_name, &invocation.arguments, &mut resolved)?;
        let mut resolved_invocation = invocation.clone();
        resolved_invocation.arguments = arguments;
        Ok((resolved_invocation, resolved))
    }

    fn resolve_value(
        &self,
        tool: &str,
        value: &serde_json::Value,
        resolved: &mut Vec<ResolvedSecret>,
    ) -> Result<serde_json::Value, ToolGatewayError> {
        match value {
            serde_json::Value::String(text) => {
                self.resolve_text(tool, text, resolved).map(serde_json::Value::String)
            }
            serde_json::Value::Array(items) => items
                .iter()
                .map(|item| self.resolve_value(tool, item, resolved))
                .collect::<Result<Vec<_>, _>>()
                .map(serde_json::Value::Array),
            serde_json::Value::Object(map) => map
                .iter()
                .map(|(key, item)| {
                    self.resolve_value(tool, item, resolved).map(|value| (key.clone(), value))
                })
                .collect::<Result<serde_json::Map<_, _>, _>>()
                .map(serde_json::Value::Object),
            other => Ok(other.clone()),
        }
    }

    /// Substitute every `secret://<source>/<name>` in one string.
    ///
    /// A reference is parsed only at the scheme's start and ends at the first
    /// character that cannot appear in a name, so `Bearer secret://env/TOKEN`
    /// inside a shell command substitutes exactly the token and leaves the
    /// rest of the command alone. A malformed reference is refused rather than
    /// passed through as text: a tool handed the literal reference would do
    /// something with it, and the caller would have meant a value.
    fn resolve_text(
        &self,
        tool: &str,
        text: &str,
        resolved: &mut Vec<ResolvedSecret>,
    ) -> Result<String, ToolGatewayError> {
        let Some(first) = text.find(SECRET_SCHEME) else {
            return Ok(text.to_string());
        };
        let mut out = String::with_capacity(text.len());
        out.push_str(&text[..first]);
        let mut rest = &text[first..];
        while let Some(start) = rest.find(SECRET_SCHEME) {
            out.push_str(&rest[..start]);
            let after = &rest[start + SECRET_SCHEME.len()..];
            let end = after
                .find(|ch: char| {
                    !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/'))
                })
                .unwrap_or(after.len());
            let body = &after[..end];
            let reference = format!("{SECRET_SCHEME}{body}");
            let malformed = || ToolGatewayError::SecretUnavailable {
                tool: tool.to_string(),
                reference: reference.clone(),
                reason: "a secret reference is written secret://<source>/<name>".to_string(),
                kind: ToolErrorKind::InvalidArguments,
            };
            let Some((source, name)) = body.split_once('/') else {
                return Err(malformed());
            };
            if source.is_empty() || name.is_empty() || name.contains('/') {
                return Err(malformed());
            }
            let secret = SecretRef { source: source.to_string(), name: name.to_string() };
            let Some(provider) = self.secrets.as_deref() else {
                return Err(ToolGatewayError::SecretUnavailable {
                    tool: tool.to_string(),
                    reference: secret.reference(),
                    reason: "this run has no secret provider configured".to_string(),
                    kind: ToolErrorKind::PermissionDenied,
                });
            };
            let value = provider.resolve(&secret).map_err(|error| match error {
                SecretProviderError::NotFound { .. } => ToolGatewayError::SecretUnavailable {
                    tool: tool.to_string(),
                    reference: secret.reference(),
                    reason: "is not available to this run".to_string(),
                    kind: ToolErrorKind::PermissionDenied,
                },
                SecretProviderError::Failed(message) => ToolGatewayError::SecretUnavailable {
                    tool: tool.to_string(),
                    reference: secret.reference(),
                    reason: format!("the secret provider failed: {message}"),
                    kind: ToolErrorKind::Unavailable,
                },
            })?;
            resolved.push(ResolvedSecret { reference: secret.reference(), value: value.clone() });
            out.push_str(&value);
            rest = &after[end..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Whether an approval already covers the capabilities a secret-using call
    /// declares.
    ///
    /// Any intersecting grant is enough: the grant was given for this tool's
    /// capability, and a secret names a value the person approving the call
    /// could not see either way.
    async fn secret_use_granted(
        &self,
        session_id: &SessionId,
        capabilities: &[Capability],
    ) -> Result<bool, ToolGatewayError> {
        let leases = self
            .approvals
            .leases_in_effect(session_id)
            .await
            .map_err(|e| ToolGatewayError::Approval(e.to_string()))?;
        Ok(leases.iter().any(|lease| capabilities.contains(&lease.capability)))
    }

    /// Raise the approval a secret-using call needs, and say it is pending.
    async fn request_approval(
        &self,
        session_id: &SessionId,
        invocation: &ToolInvocation,
        capability: Capability,
    ) -> Result<ToolGatewayError, ToolGatewayError> {
        let summary = self.summarize(invocation);
        let req = self
            .approvals
            .request(session_id, capability, &invocation.tool_name, &summary)
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

    fn summarize(&self, invocation: &ToolInvocation) -> String {
        let raw = serde_json::to_string(&invocation.arguments).unwrap_or_default();
        let (scrubbed, _) = self.redactor.redact(&raw);
        if scrubbed.len() > 200 {
            // 200 is a *size* budget, so the cut is a byte index — but the
            // arguments are whatever the caller sent, and in this repo that is
            // routinely not ASCII. The index has to be moved back to a
            // character boundary or the slice panics, which would happen while
            // asking a person to decide. `chars().take(200)` would also be safe
            // and is the wrong fix: it counts characters, which for Chinese
            // text is a bound three times larger.
            format!("{}…", &scrubbed[..scrubbed.floor_char_boundary(200)])
        } else {
            scrubbed
        }
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
    pub async fn execute_approved(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError> {
        if !self.is_known(&invocation.tool_name) {
            return Err(self.unknown_tool(&invocation.tool_name));
        }
        self.check_arguments(invocation)?;
        self.check_paths(invocation, &self.policy_context(invocation))?;
        self.run_resolved(invocation).await
    }

    /// Resolve references, run the tool, scrub what comes back.
    ///
    /// The one path both execution entry points share, so an approved call and
    /// an allowed one cannot disagree about whether a reference was resolved
    /// or whether its value was scrubbed afterwards.
    async fn run_resolved(
        &self,
        invocation: &ToolInvocation,
    ) -> Result<ToolResult, ToolGatewayError> {
        let (resolved_invocation, secrets) = self.resolve_arguments(invocation)?;
        self.finish_resolved(&resolved_invocation, &secrets).await
    }

    /// Run an invocation whose references are already resolved.
    async fn finish_resolved(
        &self,
        resolved_invocation: &ToolInvocation,
        secrets: &[ResolvedSecret],
    ) -> Result<ToolResult, ToolGatewayError> {
        let result = self
            .inner
            .invoke(resolved_invocation)
            .await
            .map_err(|error| ToolGatewayError::Tool(scrub_provider_error(error, secrets)))?;
        Ok(self.finalize(result, secrets))
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

        if !self.exceeds_offload_limits(&finalized.content) {
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

impl GuardedToolProvider {
    /// Whether content is past either offload limit.
    fn exceeds_offload_limits(&self, content: &str) -> bool {
        content.len() > self.offload_max_bytes || content.lines().count() > OFFLOAD_MAX_LINES
    }
}

/// A reference resolved for one call, kept only long enough to scrub the
/// answer and any error the provider returns.
struct ResolvedSecret {
    /// The reference as written, which is what the redaction metadata names.
    reference: String,
    /// The plaintext that was handed to the tool. Never persisted.
    value: String,
}

/// Replace every occurrence of a resolved value with `***`.
///
/// Returns the scrubbed text and the references whose values were found, so
/// the redaction metadata can say what happened without carrying the value.
/// Longest first: when one secret contains another, replacing the shorter one
/// first would leave a fragment of the longer one behind.
fn scrub_text(text: &str, secrets: &[ResolvedSecret]) -> (String, Vec<String>) {
    let mut out = text.to_string();
    let mut patterns = Vec::new();
    let mut ordered: Vec<&ResolvedSecret> =
        secrets.iter().filter(|secret| secret.value.len() >= MIN_SECRET_SCRUB_CHARS).collect();
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.value.len()));
    for secret in ordered {
        if out.contains(&secret.value) {
            out = out.replace(&secret.value, "***");
            patterns.push(secret.reference.clone());
        }
    }
    (out, patterns)
}

/// Scrub resolved values out of a provider's error message.
///
/// A shell that echoes its command line, or a server that quotes the header it
/// rejected, would otherwise put the value into `tool/failed` — durable, and
/// exported with the session.
fn scrub_provider_error(error: ToolProviderError, secrets: &[ResolvedSecret]) -> ToolProviderError {
    match error {
        ToolProviderError::Failed { tool, message } => {
            let (message, _) = scrub_text(&message, secrets);
            ToolProviderError::Failed { tool, message }
        }
        ToolProviderError::Classified { tool, kind, message } => {
            let (message, _) = scrub_text(&message, secrets);
            ToolProviderError::Classified { tool, kind, message }
        }
        other => other,
    }
}

/// The head, a marker naming the full content's locator, and the tail.
///
/// The marker is a sentence the model can act on rather than a bare hash: it
/// says what happened, how much there was, and what to ask for to see it.
fn offload_preview(content: &str, artifact: &ArtifactRef) -> String {
    // Short-but-long-by-lines content would otherwise have its head and tail
    // overlap, printing the same text twice.
    let (head_end, tail_start) = if content.len() <= PREVIEW_HEAD_BYTES + PREVIEW_TAIL_BYTES {
        (content.len(), content.len())
    } else {
        (
            char_boundary_at_or_before(content, PREVIEW_HEAD_BYTES),
            char_boundary_at_or_after(content, PREVIEW_TAIL_BYTES),
        )
    };
    let marker = format!(
        "... output truncated; full content saved to {} ({} bytes, {} lines) ...",
        artifact.locator(),
        artifact.size,
        content.lines().count()
    );
    let head = &content[..head_end];
    let tail = &content[tail_start..];
    if tail.is_empty() { format!("{head}\n{marker}") } else { format!("{head}\n{marker}\n{tail}") }
}

/// The largest index at or below `max` that starts a character.
fn char_boundary_at_or_before(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// The smallest index at or above `len - from_end` that starts a character.
fn char_boundary_at_or_after(text: &str, from_end: usize) -> usize {
    if text.len() <= from_end {
        return 0;
    }
    let mut start = text.len() - from_end;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
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

        // Ask the engine with nothing granted first. A grant can only turn an
        // `ask` into an `allow` — never a `deny` — so when the answer is not
        // `ask`, the session's log cannot change it, and reading that log would
        // be work done to learn nothing. It is not free either: resolving the
        // grants folds the session's whole stream, and this runs on every tool
        // call. The second evaluation is the one that costs, and it happens
        // only for the calls that were going to stop and ask anyway.
        let mut decision = self.policy.evaluate_with_ctx(&capabilities, Some(&ctx), &[]);
        if decision == PolicyDecision::Ask {
            let grants = self
                .approvals
                .leases_in_effect(session_id)
                .await
                .map_err(|e| ToolGatewayError::Approval(e.to_string()))?
                .into_iter()
                .map(|lease| lease.capability)
                .collect::<Vec<_>>();
            decision = self.policy.evaluate_with_ctx(&capabilities, Some(&ctx), &grants);
        }

        match decision {
            PolicyDecision::Deny => {
                // A tool that declares nothing leaves `refusals` nothing to
                // name — there is no capability to report a layer for — so the
                // sentence is written here instead of assembled from layers.
                //
                // Asked with no grants, because a grant cannot turn anything
                // into a `deny`: every layer this can name refuses the same way
                // it did in the evaluation above, whichever that was.
                let reason = if capabilities.is_empty() {
                    "declares no capabilities, and an undeclared surface is not an unlimited one"
                        .to_string()
                } else {
                    deny_reason(&self.policy.refusals(&capabilities, Some(&ctx), &[]))
                };
                Err(ToolGatewayError::PolicyDenied { tool: invocation.tool_name.clone(), reason })
            }
            PolicyDecision::Ask => {
                let capability =
                    capabilities.first().cloned().unwrap_or_else(|| Capability::new("unknown"));
                Err(self.request_approval(session_id, invocation, capability).await?)
            }
            PolicyDecision::Allow => {
                self.check_paths(invocation, &ctx)?;
                if !Self::references_secret(&invocation.arguments) {
                    return self.finish_resolved(invocation, &[]).await;
                }
                // Resolve before asking: a reference this run cannot resolve is
                // a call that could never run, and a person should not be asked
                // to approve one. The values stay in this frame until the call
                // actually runs (or the approval is raised and they are
                // dropped with the frame).
                let (resolved_invocation, secrets) = self.resolve_arguments(invocation)?;
                // A call that names a secret is a call a person should see
                // before it runs. The reference is all the summary can show
                // them, and the tool's own capability says whether they
                // already said yes to it in this scope.
                if !self.secret_use_granted(session_id, &capabilities).await? {
                    let capability =
                        capabilities.first().cloned().unwrap_or_else(|| Capability::new("unknown"));
                    return Err(self.request_approval(session_id, invocation, capability).await?);
                }
                self.finish_resolved(&resolved_invocation, &secrets).await
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

/// Why each requested capability was refused, and by which layer.
///
/// The capability alone was the whole of the old message, and it said "denied by
/// policy" for every layer there is — including the two that are not policy at
/// all. A capability the sandbox does not provide is not a decision anyone made;
/// it is a fact about this build, and the two are fixed in different places.
///
/// The list is empty exactly when the decision was not `Deny`, so this is only
/// ever asked about a capability that really was refused.
fn deny_reason(refusals: &[(Capability, Refusal)]) -> String {
    refusals
        .iter()
        .map(|(capability, layer)| {
            format!("capability {} {}", capability.as_str(), layer.described())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// How many leading characters two names have in common.
fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::artifact_store::ArtifactStoreError;
    use agent_ports::tool_provider::{ToolProviderError, ToolSideEffect, ToolSource, ToolStatus};
    use agent_protocol::ids::ApprovalId;
    use agent_protocol::permissions::{ApprovalScope, Workspace};
    use agent_testkit::FakeEventStore;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A tool provider that serves pre-scripted results and declares a fixed
    /// definition, so the gateway can resolve capabilities.
    struct ScriptedTools {
        defs: Vec<ToolDefinition>,
        results: Mutex<HashMap<String, ToolResult>>,
    }

    impl ScriptedTools {
        fn echo(defs: Vec<ToolDefinition>) -> Self {
            Self { defs, results: Mutex::new(HashMap::new()) }
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for ScriptedTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }

        fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
            self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
        }

        async fn invoke(
            &self,
            invocation: &ToolInvocation,
        ) -> Result<ToolResult, ToolProviderError> {
            let results = self.results.lock().unwrap();
            results.get(&invocation.tool_name).cloned().ok_or_else(|| {
                ToolProviderError::UnknownTool {
                    tool: invocation.tool_name.clone(),
                    available: results.keys().cloned().collect(),
                }
            })
        }
    }

    /// A provider whose path argument is not called `path`, and which says so.
    ///
    /// The resource boundary is supposed to follow the declaration rather than
    /// a conventional key name (K-08); this is the declaration it follows.
    struct RenamedPathTools {
        defs: Vec<ToolDefinition>,
        argument: String,
    }

    #[async_trait::async_trait]
    impl ToolProvider for RenamedPathTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }

        fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
            self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
        }

        fn path_arguments(&self, _: &str) -> Vec<String> {
            vec![self.argument.clone()]
        }

        async fn invoke(&self, _: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
            Ok(ToolResult::completed("ran"))
        }
    }

    /// A provider that declares definitions and answers for its side effects
    /// whatever it was told to — including `None`, which is the shape
    /// `ToolProvider::side_effect` lets a provider take: a described surface
    /// with the classification left to the adapter.
    struct SpeakingTools {
        defs: Vec<ToolDefinition>,
        answer: Option<ToolSideEffect>,
    }

    #[async_trait::async_trait]
    impl ToolProvider for SpeakingTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }

        fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
            if self.defs.iter().any(|d| d.name == tool_name) { self.answer } else { None }
        }

        async fn invoke(&self, _: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
            Ok(ToolResult::completed("ran"))
        }
    }

    /// A provider whose tool runs and fails, which is the shape
    /// [`ToolProviderError::Failed`] exists for — as distinct from a name this
    /// provider does not implement.
    struct FailingTools {
        defs: Vec<ToolDefinition>,
        message: String,
    }

    #[async_trait::async_trait]
    impl ToolProvider for FailingTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }

        fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
            self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
        }

        async fn invoke(
            &self,
            invocation: &ToolInvocation,
        ) -> Result<ToolResult, ToolProviderError> {
            Err(ToolProviderError::Failed {
                tool: invocation.tool_name.clone(),
                message: self.message.clone(),
            })
        }
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

    fn gateway(
        tools: Arc<ScriptedTools>,
        policy: PolicyEngine,
    ) -> (GuardedToolProvider, Arc<ApprovalService>, Arc<ScriptedTools>) {
        gateway_over(ResourceGuard::new(workspace()), tools, policy)
    }

    fn gateway_over(
        guard: ResourceGuard,
        tools: Arc<ScriptedTools>,
        policy: PolicyEngine,
    ) -> (GuardedToolProvider, Arc<ApprovalService>, Arc<ScriptedTools>) {
        let store = Arc::new(FakeEventStore::new());
        let approvals = Arc::new(ApprovalService::new(store));
        let artifacts = Arc::new(agent_testkit::InMemoryArtifactStore::default());
        let gateway =
            GuardedToolProvider::new(tools.clone(), policy, approvals.clone(), guard, artifacts);
        (gateway, approvals, tools)
    }

    /// A gateway over any provider, for the cases where the interesting
    /// provider is not a [`ScriptedTools`].
    fn gateway_any(
        guard: ResourceGuard,
        tools: Arc<dyn ToolProvider>,
        policy: PolicyEngine,
    ) -> GuardedToolProvider {
        let store = Arc::new(FakeEventStore::new());
        GuardedToolProvider::new(
            tools,
            policy,
            Arc::new(ApprovalService::new(store)),
            guard,
            Arc::new(agent_testkit::InMemoryArtifactStore::default()),
        )
    }

    /// An artifact store that is simply not there, to see what the gateway does
    /// when offloading cannot happen.
    struct UnavailableArtifacts;

    impl ArtifactStore for UnavailableArtifacts {
        fn put(&self, _: &[u8], _: &str) -> Result<ArtifactRef, ArtifactStoreError> {
            Err(ArtifactStoreError::NotFound("no store".to_string()))
        }

        fn get(&self, _: &ArtifactRef) -> Result<Vec<u8>, ArtifactStoreError> {
            Err(ArtifactStoreError::NotFound("no store".to_string()))
        }
    }

    fn allowing(capability: &str) -> PolicyEngine {
        PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::allow(Capability::new(capability))])
            .with_sandbox([Capability::new(capability)])
    }

    fn sid() -> SessionId {
        SessionId::from_string("guarded-session")
    }

    fn invocation(name: &str) -> ToolInvocation {
        ToolInvocation::proposed(name, serde_json::json!({}))
    }

    #[tokio::test]
    async fn allow_runs_the_tool() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let policy = PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::allow(Capability::new(Capability::FS_READ))])
            .with_sandbox([Capability::new(Capability::FS_READ)]);
        let (gateway, _, tools) = gateway(tools, policy);
        *tools.results.lock().unwrap() =
            HashMap::from([("fs.read".to_string(), ToolResult::completed("hello"))]);

        let result = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap();
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.content, "hello");
    }

    #[tokio::test]
    async fn a_provider_failure_crosses_the_gateway_with_its_classification() {
        // Ruling #11: a tool that ran and failed and a tool that never ran are
        // exit 1 and exit 2 (C3.1). Flattening the inner error to a string would
        // make those indistinguishable at the one seam that sees both.
        let tools = Arc::new(FailingTools {
            defs: vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
            message: "the disk said no".to_string(),
        });
        let gateway =
            gateway_any(ResourceGuard::new(workspace()), tools, allowing(Capability::FS_READ));

        let error = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap_err();

        let ToolGatewayError::Tool(inner) = error else {
            panic!("a provider failure is the provider's own fact, not the gate's");
        };
        let ToolProviderError::Failed { tool, message } = inner else {
            panic!("the classification must survive the gateway");
        };
        assert_eq!(tool, "fs.read");
        assert_eq!(message, "the disk said no");
    }

    #[tokio::test]
    async fn a_provider_that_lacks_a_registered_name_keeps_that_shape_too() {
        // The gateway admitted the name; the provider is the layer that fell
        // short. That is a different fact from a name nothing recognised, and
        // both have to stay readable through the same variant.
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let gateway =
            gateway_any(ResourceGuard::new(workspace()), tools, allowing(Capability::FS_READ));

        let error = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap_err();

        let ToolGatewayError::Tool(ToolProviderError::UnknownTool { tool, available }) = error
        else {
            panic!("expected the provider's own not-implemented error");
        };
        assert_eq!(tool, "fs.read");
        assert!(available.is_empty(), "nothing was scripted: {available:?}");
    }

    #[tokio::test]
    async fn deny_refuses_without_running() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "shell.run",
            &[Capability::PROCESS_SPAWN],
            ToolSideEffect::Destructive,
        )]));
        let policy = PolicyEngine::new().with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]);
        let (gateway, _, _) = gateway(tools, policy);

        let err = gateway.execute(&sid(), &invocation("shell.run")).await.unwrap_err();
        assert!(matches!(err, ToolGatewayError::PolicyDenied { .. }), "unexpected: {err:?}");
    }

    #[tokio::test]
    async fn ask_raises_a_durable_approval() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "shell.run",
            &[Capability::PROCESS_SPAWN],
            ToolSideEffect::Destructive,
        )]));
        let policy = PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::ask(Capability::new(
                Capability::PROCESS_SPAWN,
            ))])
            .with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]);
        let (gateway, approvals, _) = gateway(tools, policy);

        let err = gateway.execute(&sid(), &invocation("shell.run")).await.unwrap_err();
        let id = match err {
            ToolGatewayError::ApprovalPending { approval_id, .. } => approval_id,
            other => panic!("expected approval pending, got {other:?}"),
        };

        let pending = approvals.pending(&sid()).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].approval_id.to_string(), id);
        assert_eq!(pending[0].capability, Capability::new(Capability::PROCESS_SPAWN));
    }

    /// As [`gateway`], but the approval service permits session-scoped grants.
    /// Those are off by default — a session-wide grant must be opted into — so
    /// a test that grants one has to say so here.
    fn gateway_with_session_scope(
        tools: Arc<ScriptedTools>,
        policy: PolicyEngine,
    ) -> (GuardedToolProvider, Arc<ApprovalService>, Arc<ScriptedTools>) {
        let store = Arc::new(FakeEventStore::new());
        let approvals = Arc::new(ApprovalService::new(store).with_session_scope(true));
        let artifacts = Arc::new(agent_testkit::InMemoryArtifactStore::default());
        let gateway = GuardedToolProvider::new(
            tools.clone(),
            policy,
            approvals.clone(),
            ResourceGuard::new(workspace()),
            artifacts,
        );
        (gateway, approvals, tools)
    }

    /// A gateway whose one tool asks: `process.spawn` is admitted by the
    /// sandbox and asked about by the rules, and nothing has granted it yet.
    fn asking_gateway() -> (GuardedToolProvider, Arc<ApprovalService>, Arc<ScriptedTools>) {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "shell.run",
            &[Capability::PROCESS_SPAWN],
            ToolSideEffect::Destructive,
        )]));
        let policy = PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::ask(Capability::new(
                Capability::PROCESS_SPAWN,
            ))])
            .with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]);
        gateway_with_session_scope(tools, policy)
    }

    /// Call the tool once, so that it stops and asks, and return the id of the
    /// request it raised.
    async fn ask_once(gateway: &GuardedToolProvider) -> ApprovalId {
        match gateway.execute(&sid(), &invocation("shell.run")).await {
            Err(ToolGatewayError::ApprovalPending { approval_id, .. }) => {
                ApprovalId::from_string(approval_id)
            }
            other => panic!("expected an approval to be raised, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_session_grant_answers_the_next_call_without_asking() {
        // YA-16. The one place production asks policy handed it "no grants"
        // unconditionally, so `approve --scope session` meant exactly what
        // `once` meant: a person who said "yes, and stop asking" was asked
        // again on the next call.
        let (gateway, approvals, tools) = asking_gateway();
        *tools.results.lock().unwrap() =
            HashMap::from([("shell.run".to_string(), ToolResult::completed("done"))]);

        let id = ask_once(&gateway).await;
        approvals.grant(&sid(), &id, ApprovalScope::AllowForSession, "user").await.unwrap();

        let result = gateway.execute(&sid(), &invocation("shell.run")).await.unwrap();
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(approvals.pending(&sid()).await.unwrap().is_empty(), "no second question");
    }

    #[tokio::test]
    async fn a_once_grant_does_not_answer_the_next_call() {
        // The other half of the same wiring, and the half that fails open if it
        // is wrong: reading `once` as a standing permission turns every
        // single-use approval into a blank cheque for that capability.
        let (gateway, approvals, tools) = asking_gateway();
        *tools.results.lock().unwrap() =
            HashMap::from([("shell.run".to_string(), ToolResult::completed("done"))]);

        let id = ask_once(&gateway).await;
        approvals.grant(&sid(), &id, ApprovalScope::AllowOnce, "user").await.unwrap();

        let again = gateway.execute(&sid(), &invocation("shell.run")).await;
        assert!(
            matches!(again, Err(ToolGatewayError::ApprovalPending { .. })),
            "a second call is a second question: {again:?}"
        );
    }

    #[tokio::test]
    async fn a_turn_grant_answers_the_next_call_while_its_turn_has_not_ended() {
        // A `turn` grant is standing here because nothing in this session's log
        // says any turn ended — it is the *next* turn it is for, and that turn
        // has not run. What ends it is a turn event; the approval service's own
        // tests drive that.
        let (gateway, approvals, tools) = asking_gateway();
        *tools.results.lock().unwrap() =
            HashMap::from([("shell.run".to_string(), ToolResult::completed("done"))]);

        let id = ask_once(&gateway).await;
        approvals.grant(&sid(), &id, ApprovalScope::AllowForTurn, "user").await.unwrap();

        let result = gateway.execute(&sid(), &invocation("shell.run")).await.unwrap();
        assert_eq!(result.status, ToolStatus::Completed);
    }

    #[tokio::test]
    async fn a_summary_whose_byte_cut_lands_mid_character_still_asks() {
        // The summary is what a person reads when asked to decide, and it is
        // built from the call's arguments — which here are routinely not ASCII.
        // Cutting at byte 200 assumed every argument was, and the assumption
        // cost more than a bad preview: the panic happens *before* the approval
        // is recorded, so the run dies on the one path where somebody is being
        // asked something.
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "shell.run",
            &[Capability::PROCESS_SPAWN],
            ToolSideEffect::Destructive,
        )]));
        let policy = PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::ask(Capability::new(
                Capability::PROCESS_SPAWN,
            ))])
            .with_sandbox([Capability::new(Capability::PROCESS_SPAWN)]);
        let (gateway, approvals, _) = gateway(tools, policy);

        // 70 three-byte characters: 210 bytes, so byte 200 lands inside one of
        // them and `&raw[..200]` would panic.
        let argument = "爻".repeat(70);
        let call =
            ToolInvocation::proposed("shell.run", serde_json::json!({ "command": argument }));

        let err = gateway.execute(&sid(), &call).await.expect_err("an approval is pending");
        assert!(matches!(err, ToolGatewayError::ApprovalPending { .. }), "{err:?}");

        let pending = approvals.pending(&sid()).await.unwrap();
        assert_eq!(pending.len(), 1, "the approval was raised, not lost before it was written");
        let summary = &pending[0].args_summary;
        let (prefix, ellipsis) = summary.split_at(summary.len() - '…'.len_utf8());
        assert_eq!(ellipsis, "…", "the preview still says it was cut: {summary}");
        assert!(prefix.len() <= 200, "the budget is bytes, and it is still a budget");
        assert!(prefix.len() >= 197, "the cut is at the boundary, not somewhere safe: {summary}");
    }

    #[tokio::test]
    async fn unknown_tool_is_rejected() {
        let tools = Arc::new(ScriptedTools::echo(vec![]));
        let policy = PolicyEngine::new();
        let (gateway, _, _) = gateway(tools, policy);

        let err = gateway.execute(&sid(), &invocation("nope")).await.unwrap_err();
        assert!(matches!(err, ToolGatewayError::UnknownTool { .. }));
    }

    #[tokio::test]
    async fn an_unknown_tool_names_the_caller_and_the_surface() {
        // "unknown tool: nope" is the same message for a typo and for a name
        // that was never registered, and those are fixed in different places. The
        // gateway is the only layer holding the surface, so it is the only layer
        // that can tell the two apart for the caller.
        let tools = Arc::new(ScriptedTools::echo(vec![
            def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read),
            def("git.commit", &[Capability::GIT_COMMIT], ToolSideEffect::Write),
        ]));
        let policy = PolicyEngine::new();
        let (gateway, _, _) = gateway(tools, policy);

        let err = gateway.execute(&sid(), &invocation("fs.reed")).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown tool 'fs.reed'"), "{message}");
        // The typo's neighbour, so the caller can see what they meant to call.
        assert!(message.contains("fs.read"), "{message}");
        assert!(message.contains("git.commit"), "{message}");
    }

    #[tokio::test]
    async fn an_empty_surface_says_so_instead_of_listing_nothing() {
        // A run with no tools at all is a configuration fact, and "the run
        // registers 0 tool(s): " would read like a formatting bug rather than the
        // reason. The count is what the sentence exists to carry.
        let tools = Arc::new(ScriptedTools::echo(vec![]));
        let policy = PolicyEngine::new();
        let (gateway, _, _) = gateway(tools, policy);

        let err = gateway.execute(&sid(), &invocation("nope")).await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no tools registered"), "{message}");
    }

    #[tokio::test]
    async fn a_long_surface_shows_the_nearest_names_and_not_the_first_dozen() {
        // Past the sample size the list has to stop somewhere, and where it stops
        // is the whole question: an arbitrary dozen need not contain the name the
        // caller was reaching for, and then the note tells them nothing. The
        // closest name is the one that has to survive the cut.
        let mut defs: Vec<ToolDefinition> = (0..SURFACE_SAMPLE * 2)
            .map(|i| def(&format!("tool.{i:02}"), &[Capability::FS_READ], ToolSideEffect::Read))
            .collect();
        // Registered last, so a list that simply keeps the first dozen drops it —
        // and a near miss for the name that is about to be asked for.
        defs.push(def("tool.report", &[Capability::FS_READ], ToolSideEffect::Read));

        let tools = Arc::new(ScriptedTools::echo(defs));
        let (gateway, _, _) = gateway(tools, PolicyEngine::new());

        let err = gateway.execute(&sid(), &invocation("tool.reporter")).await.unwrap_err();
        let message = err.to_string();
        // First in the list, because it shares the most with what was asked for
        // — not merely present somewhere in the sentence, which it would be
        // anyway as a prefix of the name being refused.
        assert!(message.contains("the nearest are: tool.report,"), "{message}");
        // The count is still the whole surface, so a caller can tell a small
        // surface from a truncated view of a large one.
        assert!(message.contains(&format!("{} tools", SURFACE_SAMPLE * 2 + 1)), "{message}");
    }

    #[tokio::test]
    async fn oversized_content_is_offloaded_to_artifact_store() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let (gateway, _, tools) = gateway(tools, allowing(Capability::FS_READ));
        let big = "x".repeat(OFFLOAD_MAX_BYTES + 1);
        *tools.results.lock().unwrap() =
            HashMap::from([("fs.read".to_string(), ToolResult::completed(big))]);

        let result = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap();
        assert_eq!(result.artifacts.len(), 1);
        assert!(result.content.len() < 4_000, "the model keeps a preview, not the whole thing");
        // The fact describes the full text, before the preview replaced it.
        assert_eq!(result.facts.line_count, Some(1));
    }

    #[tokio::test]
    async fn a_long_but_not_large_result_is_offloaded_too() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let (gateway, _, tools) = gateway(tools, allowing(Capability::FS_READ));
        // Barely 4 KB of text, but 2500 lines of it: past the line limit and
        // nowhere near the byte limit.
        let long = "x\n".repeat(OFFLOAD_MAX_LINES + 500);
        assert!(long.len() < OFFLOAD_MAX_BYTES, "the premise: this is a line-count case");
        *tools.results.lock().unwrap() =
            HashMap::from([("fs.read".to_string(), ToolResult::completed(long))]);

        let result = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap();
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(
            result.facts.line_count,
            Some((OFFLOAD_MAX_LINES + 500) as u64),
            "the count is of the text that was offloaded, not of the preview"
        );
    }

    #[tokio::test]
    async fn a_configured_limit_offloads_what_the_default_would_keep_inline() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let (gateway, _, tools) = gateway(tools, allowing(Capability::FS_READ));
        // A small working set: 10 KB of output is past a third of it, while it
        // is nowhere near the historical 50 KiB default.
        let gateway = gateway.with_offload_limit(8 * 1024);
        let content = "x".repeat(10 * 1024);
        *tools.results.lock().unwrap() =
            HashMap::from([("fs.read".to_string(), ToolResult::completed(content))]);

        let result = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap();
        assert_eq!(result.artifacts.len(), 1);
        assert!(result.content.contains("output truncated"));
    }

    #[tokio::test]
    async fn the_offloaded_preview_names_the_locator_and_keeps_both_ends() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let (gateway, _, tools) = gateway(tools, allowing(Capability::FS_READ));
        let mut big = String::from("FIRST-LINE\n");
        big.push_str(&"middle\n".repeat(20_000));
        big.push_str("LAST-LINE\n");
        *tools.results.lock().unwrap() =
            HashMap::from([("fs.read".to_string(), ToolResult::completed(big))]);

        let result = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap();

        let artifact = &result.artifacts[0];
        // The marker has to be actionable on its own: what happened, where the
        // whole thing is, and how much of it there was.
        assert!(result.content.contains(&artifact.locator()), "{}", result.content);
        assert!(result.content.contains("output truncated"), "{}", result.content);
        assert!(result.content.contains(&artifact.size.to_string()));
        // Both ends survive, so a failure at the top or the bottom is visible.
        assert!(result.content.contains("FIRST-LINE"));
        assert!(result.content.contains("LAST-LINE"));
    }

    #[tokio::test]
    async fn an_unavailable_artifact_store_leaves_the_result_whole() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let store = Arc::new(FakeEventStore::new());
        let approvals = Arc::new(ApprovalService::new(store));
        let gateway = GuardedToolProvider::new(
            tools.clone(),
            allowing(Capability::FS_READ),
            approvals,
            ResourceGuard::new(workspace()),
            Arc::new(UnavailableArtifacts),
        );
        let big = "x".repeat(OFFLOAD_MAX_BYTES + 1);
        *tools.results.lock().unwrap() =
            HashMap::from([("fs.read".to_string(), ToolResult::completed(big.clone()))]);

        let result = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap();

        // Losing the answer is worse than eating context: a store that is down
        // must not turn a finished tool call into a failure, and must not
        // quietly drop what the tool produced.
        assert_eq!(result.status, ToolStatus::Completed);
        assert!(result.artifacts.is_empty());
        assert_eq!(result.content, big);
    }

    #[tokio::test]
    async fn an_edit_is_a_write_because_it_says_so_not_because_of_its_name() {
        // The read/write boundary is decided by the tool's declared side
        // effect. Deriving it from the name is how a newly added write tool
        // ends up outside the boundary without anyone noticing.
        let dir = tempfile::tempdir().unwrap();
        let rw = dir.path().join("rw");
        let ro = dir.path().join("ro");
        std::fs::create_dir_all(&rw).unwrap();
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("a.txt"), "let x = 1;\n").unwrap();
        let guard = ResourceGuard::new(Workspace {
            workspace_root: dir.path().display().to_string(),
            writable_roots: vec![rw.display().to_string()],
            external_directories: vec![],
        });
        let tools = Arc::new(ScriptedTools::echo(vec![
            def("fs.edit", &[Capability::FS_WRITE], ToolSideEffect::Write),
            def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read),
        ]));
        let policy = PolicyEngine::new()
            .with_rules([
                crate::policy::PolicyRule::allow(Capability::new(Capability::FS_WRITE)),
                crate::policy::PolicyRule::allow(Capability::new(Capability::FS_READ)),
            ])
            .with_sandbox([
                Capability::new(Capability::FS_WRITE),
                Capability::new(Capability::FS_READ),
            ]);
        *tools.results.lock().unwrap() =
            HashMap::from([("fs.read".to_string(), ToolResult::completed("let x = 1;"))]);
        let (gateway, _, _) = gateway_over(guard, tools, policy);
        let path = ro.join("a.txt").display().to_string();

        let write = gateway
            .execute(
                &sid(),
                &ToolInvocation::proposed("fs.edit", serde_json::json!({"path": path})),
            )
            .await
            .unwrap_err();
        assert!(matches!(write, ToolGatewayError::ResourceDenied { .. }), "{write:?}");

        // The very same path is readable: it is the write that the boundary
        // refuses, not the file.
        let read = gateway
            .execute(
                &sid(),
                &ToolInvocation::proposed("fs.read", serde_json::json!({"path": path})),
            )
            .await;
        assert!(read.is_ok(), "reading the same path must not be denied: {read:?}");
    }

    #[tokio::test]
    async fn secrets_in_output_are_redacted() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let policy = PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::allow(Capability::new(Capability::FS_READ))])
            .with_sandbox([Capability::new(Capability::FS_READ)]);
        let (gateway, _, tools) = gateway(tools, policy);
        *tools.results.lock().unwrap() = HashMap::from([(
            "fs.read".to_string(),
            ToolResult::completed("token=ghp_abcdefghijklmnopqrstuvwxyz"),
        )]);

        let result = gateway.execute(&sid(), &invocation("fs.read")).await.unwrap();
        assert!(!result.content.contains("ghp_"));
        assert!(result.redaction_metadata.redacted);
    }

    #[tokio::test]
    async fn allow_but_path_outside_workspace_is_resource_denied() {
        let tools = Arc::new(ScriptedTools::echo(vec![def(
            "fs.read",
            &[Capability::FS_READ],
            ToolSideEffect::Read,
        )]));
        let policy = PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::allow(Capability::new(Capability::FS_READ))])
            .with_sandbox([Capability::new(Capability::FS_READ)]);
        let (gateway, _, _) = gateway(tools, policy);

        // On Windows an absolute path on another drive is outside the workspace.
        let outside =
            if cfg!(windows) { "C:/Windows/System32/drivers/etc/hosts" } else { "/etc/passwd" };
        let inv = ToolInvocation::proposed("fs.read", serde_json::json!({ "path": outside }));
        let err = gateway.execute(&sid(), &inv).await.unwrap_err();
        assert!(matches!(err, ToolGatewayError::ResourceDenied { .. }), "unexpected: {err:?}");
    }

    #[tokio::test]
    async fn a_path_argument_named_otherwise_is_still_bounded() {
        // A tool that calls its path argument `file` used to slip past the
        // boundary: the gateway looked only under `path`, found nothing, and
        // checked nothing. The provider now declares the argument, and the
        // check follows the declaration (K-08).
        let outside =
            if cfg!(windows) { "C:/Windows/System32/drivers/etc/hosts" } else { "/etc/passwd" };
        let tools = Arc::new(RenamedPathTools {
            defs: vec![def("fs.write_file", &[Capability::FS_WRITE], ToolSideEffect::Write)],
            argument: "file".to_string(),
        });
        let gateway =
            gateway_any(ResourceGuard::new(workspace()), tools, allowing(Capability::FS_WRITE));

        let inv = ToolInvocation::proposed("fs.write_file", serde_json::json!({ "file": outside }));
        let err = gateway.execute(&sid(), &inv).await.unwrap_err();
        assert!(matches!(err, ToolGatewayError::ResourceDenied { .. }), "unexpected: {err:?}");

        // The same tool with no path in the call is not refused for a key it
        // did not send — the declaration marks a boundary, not a requirement.
        let no_path = gateway.execute(&sid(), &invocation("fs.write_file")).await;
        assert!(no_path.is_ok(), "a call without the argument must run: {no_path:?}");
    }

    #[tokio::test]
    async fn a_tool_that_declares_no_capabilities_is_denied_with_a_reason() {
        // Capabilities are resolved from the definition, so an empty
        // declaration is reachable through the ordinary surface — and it must
        // not be a way around the engine. The reason is asserted and not just
        // the decision: `refusals` has no capability to name here, so a blank
        // sentence would be the failure mode.
        let tools =
            Arc::new(ScriptedTools::echo(vec![def("mystery.tool", &[], ToolSideEffect::Read)]));
        let (gateway, _, tools) = gateway(tools, PolicyEngine::new());
        *tools.results.lock().unwrap() =
            HashMap::from([("mystery.tool".to_string(), ToolResult::completed("ran"))]);

        let err = gateway.execute(&sid(), &invocation("mystery.tool")).await.unwrap_err();
        match err {
            ToolGatewayError::PolicyDenied { reason, .. } => {
                assert!(reason.contains("declares no capabilities"), "unexpected reason: {reason}");
            }
            other => panic!("expected a policy denial, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_tool_that_does_not_classify_itself_is_treated_as_a_write() {
        // The boundary is decided by asking the provider, and a provider is
        // allowed to describe its surface without answering that question. The
        // absent answer is not "harmless": the same path is refused for a tool
        // that says nothing and allowed for one that declares itself pure.
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        let rw = dir.path().join("rw");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::create_dir_all(&rw).unwrap();
        std::fs::write(ro.join("a.txt"), "x").unwrap();
        let path = ro.join("a.txt").display().to_string();
        // `ro` is inside the workspace root and outside the one writable root,
        // so the same invocation reads or writes purely by classification.
        let guard = ResourceGuard::new(Workspace {
            workspace_root: dir.path().display().to_string(),
            writable_roots: vec![rw.display().to_string()],
            external_directories: vec![],
        });
        let policy = allowing(Capability::FS_READ);

        let unclassified = gateway_any(
            guard.clone(),
            Arc::new(SpeakingTools {
                defs: vec![def("mystery.tool", &[Capability::FS_READ], ToolSideEffect::Write)],
                answer: None,
            }),
            policy.clone(),
        );
        let err = unclassified
            .execute(
                &sid(),
                &ToolInvocation::proposed("mystery.tool", serde_json::json!({ "path": path })),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolGatewayError::ResourceDenied { .. }), "unexpected: {err:?}");

        // `Pure` is an affirmative claim — this builds nothing and touches
        // nothing — so it is taken as one, unlike silence.
        let declaring = gateway_any(
            guard,
            Arc::new(SpeakingTools {
                defs: vec![def("plain.tool", &[Capability::FS_READ], ToolSideEffect::Pure)],
                answer: Some(ToolSideEffect::Pure),
            }),
            policy,
        );
        let ok = declaring
            .execute(
                &sid(),
                &ToolInvocation::proposed("plain.tool", serde_json::json!({ "path": path })),
            )
            .await;
        assert!(ok.is_ok(), "a declared-pure call must not be refused as a write: {ok:?}");
    }

    fn declared(name: &str, schema: serde_json::Value) -> ToolDefinition {
        ToolDefinition { input_schema: schema, ..def(name, &[], ToolSideEffect::Read) }
    }

    #[tokio::test]
    async fn arguments_that_miss_the_declaration_never_reach_the_tool() {
        let tools = Arc::new(ScriptedTools::echo(vec![declared(
            "fs.write",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        )]));
        let gateway = gateway_any(ResourceGuard::new(workspace()), tools, PolicyEngine::new());

        let err = gateway
            .execute(
                &sid(),
                &ToolInvocation::proposed("fs.write", serde_json::json!({ "path": "a.txt" })),
            )
            .await
            .expect_err("content is missing");
        let ToolGatewayError::InvalidArguments { tool, problems } = &err else {
            panic!("unexpected: {err:?}")
        };
        assert_eq!(tool, "fs.write");
        assert!(problems[0].contains("content"), "{problems:?}");
        assert!(err.to_string().contains("fs.write"), "{err}");
    }

    #[tokio::test]
    async fn a_call_refused_for_its_arguments_never_asks_a_person() {
        // The order is the point: a person is not asked to approve a call that
        // could never run, and a policy review is not spent on one.
        let tools = Arc::new(ScriptedTools::echo(vec![ToolDefinition {
            capability_requirements: vec![Capability::new(Capability::FS_WRITE)],
            ..declared("fs.write", serde_json::json!({ "type": "object", "required": ["path"] }))
        }]));
        let policy = PolicyEngine::new()
            .with_rules([crate::policy::PolicyRule::ask(Capability::new(Capability::FS_WRITE))])
            .with_sandbox([Capability::new(Capability::FS_WRITE)]);
        let (gateway, approvals, _) = gateway_over(ResourceGuard::new(workspace()), tools, policy);

        let err = gateway
            .execute(&sid(), &ToolInvocation::proposed("fs.write", serde_json::json!({})))
            .await
            .expect_err("path is missing");
        assert!(matches!(err, ToolGatewayError::InvalidArguments { .. }), "unexpected: {err:?}");

        let pending = approvals.pending(&sid()).await.expect("the approval list is readable").len();
        assert_eq!(pending, 0, "a call that could not run must not raise an approval");
    }

    #[tokio::test]
    async fn a_declaration_this_build_cannot_enforce_refuses_the_call() {
        let tools = Arc::new(ScriptedTools::echo(vec![declared(
            "broken.tool",
            serde_json::json!({ "type": "object", "required": "path" }),
        )]));
        let gateway = gateway_any(ResourceGuard::new(workspace()), tools, PolicyEngine::new());

        let err = gateway
            .execute(&sid(), &ToolInvocation::proposed("broken.tool", serde_json::json!({})))
            .await
            .expect_err("the declaration is not a schema");
        let ToolGatewayError::InvalidSchema { tool, reason } = &err else {
            panic!("unexpected: {err:?}")
        };
        assert_eq!(tool, "broken.tool");
        assert!(reason.contains("JSON Schema"), "{reason}");
        assert_eq!(err.kind(), Some(agent_protocol::tool::ToolErrorKind::Protocol));
    }

    #[tokio::test]
    async fn an_approved_call_is_checked_too() {
        // The resume path does not go through `execute`, so a call that was
        // held, approved and then replayed would otherwise skip the check.
        let tools = Arc::new(ScriptedTools::echo(vec![declared(
            "fs.write",
            serde_json::json!({ "type": "object", "required": ["path"] }),
        )]));
        let gateway = gateway_any(ResourceGuard::new(workspace()), tools, PolicyEngine::new());

        let err = gateway
            .execute_approved(&ToolInvocation::proposed("fs.write", serde_json::json!({})))
            .await
            .expect_err("path is missing");
        assert!(matches!(err, ToolGatewayError::InvalidArguments { .. }), "unexpected: {err:?}");
    }

    #[test]
    fn every_refusal_names_the_class_a_metric_would_count() {
        use agent_protocol::tool::ToolErrorKind;
        let cases = [
            (
                ToolGatewayError::PolicyDenied { tool: "t".into(), reason: "r".into() },
                Some(ToolErrorKind::PermissionDenied),
            ),
            (
                ToolGatewayError::ResourceDenied { tool: "t".into(), reason: "r".into() },
                Some(ToolErrorKind::PermissionDenied),
            ),
            (
                ToolGatewayError::UnknownTool { tool: "t".into(), reason: "r".into() },
                Some(ToolErrorKind::UnknownTool),
            ),
            (
                ToolGatewayError::InvalidArguments { tool: "t".into(), problems: vec![] },
                Some(ToolErrorKind::InvalidArguments),
            ),
            (
                ToolGatewayError::InvalidSchema { tool: "t".into(), reason: "r".into() },
                Some(ToolErrorKind::Protocol),
            ),
            (
                ToolGatewayError::Tool(ToolProviderError::failed("t", "no")),
                Some(ToolErrorKind::Failed),
            ),
            (
                ToolGatewayError::Tool(ToolProviderError::classified(
                    ToolErrorKind::Timeout,
                    "t",
                    "late",
                )),
                Some(ToolErrorKind::Timeout),
            ),
            // The two that are not answers about a call at all.
            (ToolGatewayError::ApprovalPending { tool: "t".into(), approval_id: "a".into() }, None),
            (ToolGatewayError::Approval("broken".into()), None),
        ];
        for (error, expected) in cases {
            assert_eq!(error.kind(), expected, "{error}");
        }
    }

    /// A provider that records what it was handed and answers with `reply`.
    struct RecordingTools {
        defs: Vec<ToolDefinition>,
        reply: ToolResult,
        fail_with: Option<String>,
        seen: Mutex<Vec<ToolInvocation>>,
    }

    impl RecordingTools {
        fn new(reply: &str) -> Self {
            Self {
                defs: vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
                reply: ToolResult::completed(reply),
                fail_with: None,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn failing(message: &str) -> Self {
            let mut tools = Self::new("");
            tools.fail_with = Some(message.to_string());
            tools
        }

        fn seen(&self) -> Vec<ToolInvocation> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for RecordingTools {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }

        fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
            self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
        }

        async fn invoke(
            &self,
            invocation: &ToolInvocation,
        ) -> Result<ToolResult, ToolProviderError> {
            self.seen.lock().unwrap().push(invocation.clone());
            match &self.fail_with {
                Some(message) => {
                    Err(ToolProviderError::failed(&invocation.tool_name, message.clone()))
                }
                None => Ok(self.reply.clone()),
            }
        }
    }

    /// A secret provider with one resolvable name.
    struct OneSecret;

    impl SecretProvider for OneSecret {
        fn resolve(&self, secret: &SecretRef) -> Result<String, SecretProviderError> {
            if secret.source == "env" && secret.name == "TOOL_TOKEN" {
                Ok("s3cret-value-12345".to_string())
            } else {
                Err(SecretProviderError::NotFound {
                    provider: secret.source.clone(),
                    name: secret.name.clone(),
                })
            }
        }
    }

    fn secret_gateway(tools: Arc<RecordingTools>) -> GuardedToolProvider {
        let store = Arc::new(FakeEventStore::new());
        GuardedToolProvider::new(
            tools,
            allowing(Capability::FS_READ),
            Arc::new(ApprovalService::new(store)),
            ResourceGuard::new(workspace()),
            Arc::new(agent_testkit::InMemoryArtifactStore::default()),
        )
        .with_secrets(Arc::new(OneSecret))
    }

    fn secret_invocation(arguments: serde_json::Value) -> ToolInvocation {
        ToolInvocation::proposed("fs.read", arguments)
    }

    #[tokio::test]
    async fn a_secret_reference_is_resolved_at_the_tool_boundary_and_scrubbed_from_the_result() {
        let tools = Arc::new(RecordingTools::new("the token is s3cret-value-12345"));
        let gateway = secret_gateway(tools.clone());
        let invocation = secret_invocation(serde_json::json!({
            "path": "notes.txt",
            "command": "echo 'Bearer secret://env/TOOL_TOKEN'",
        }));

        // The call is a secret-using one, so it stops for approval first.
        let pending = gateway.execute(&sid(), &invocation).await.unwrap_err();
        assert!(matches!(pending, ToolGatewayError::ApprovalPending { .. }), "{pending:?}");
        assert!(tools.seen().is_empty(), "nothing may run before the approval");

        // What the log and the summary hold is the reference, untouched.
        assert_eq!(invocation.arguments["command"], "echo 'Bearer secret://env/TOOL_TOKEN'");

        let result = gateway.execute_approved(&invocation).await.unwrap();
        let seen = tools.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].arguments["command"], "echo 'Bearer s3cret-value-12345'");
        assert!(!result.content.contains("s3cret-value"), "secret leaked: {}", result.content);
        assert!(result.content.contains("***"));
        assert!(
            result
                .redaction_metadata
                .patterns
                .iter()
                .any(|pattern| pattern == "secret://env/TOOL_TOKEN")
        );
    }

    #[tokio::test]
    async fn an_unknown_secret_is_refused_before_the_tool_runs() {
        let tools = Arc::new(RecordingTools::new("ok"));
        let gateway = secret_gateway(tools.clone());

        let error = gateway
            .execute(
                &sid(),
                &secret_invocation(serde_json::json!({ "command": "echo secret://env/NOPE" })),
            )
            .await
            .unwrap_err();
        match error {
            ToolGatewayError::SecretUnavailable { reference, kind, .. } => {
                assert_eq!(reference, "secret://env/NOPE");
                assert_eq!(kind, ToolErrorKind::PermissionDenied);
            }
            other => panic!("expected a secret refusal, got {other:?}"),
        }
        assert!(tools.seen().is_empty(), "an unresolvable reference must not reach the tool");
    }

    #[tokio::test]
    async fn a_malformed_secret_reference_is_refused() {
        let tools = Arc::new(RecordingTools::new("ok"));
        let gateway = secret_gateway(tools.clone());

        let error = gateway
            .execute(
                &sid(),
                &secret_invocation(serde_json::json!({ "command": "echo secret://TOOL_TOKEN" })),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                ToolGatewayError::SecretUnavailable { kind: ToolErrorKind::InvalidArguments, .. }
            ),
            "{error:?}"
        );
        assert!(tools.seen().is_empty());
    }

    #[tokio::test]
    async fn a_run_without_a_secret_provider_refuses_a_reference() {
        let tools = Arc::new(RecordingTools::new("ok"));
        let store = Arc::new(FakeEventStore::new());
        let gateway = GuardedToolProvider::new(
            tools.clone(),
            allowing(Capability::FS_READ),
            Arc::new(ApprovalService::new(store)),
            ResourceGuard::new(workspace()),
            Arc::new(agent_testkit::InMemoryArtifactStore::default()),
        );

        let error = gateway
            .execute(
                &sid(),
                &secret_invocation(
                    serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" }),
                ),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                ToolGatewayError::SecretUnavailable { kind: ToolErrorKind::PermissionDenied, .. }
            ),
            "{error:?}"
        );
        assert!(tools.seen().is_empty());
    }

    #[tokio::test]
    async fn a_provider_error_that_echoes_the_secret_is_scrubbed() {
        let tools = Arc::new(RecordingTools::failing("rejected header: Bearer s3cret-value-12345"));
        let gateway = secret_gateway(tools);

        let error = gateway
            .execute_approved(&secret_invocation(
                serde_json::json!({ "command": "echo secret://env/TOOL_TOKEN" }),
            ))
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(!text.contains("s3cret-value"), "secret leaked: {text}");
        assert!(text.contains("***"));
    }
}
