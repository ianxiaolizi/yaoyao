//! `subagent.run` — delegation as a tool the parent can call.
//!
//! Everything below the tool already exists by the time this module is reached:
//! the port says how a child is started and how its ending is heard, the kernel
//! says a child's history is a fork and that the parent's log records the
//! delegation, and the stdio backend runs the process. What is left is the one
//! thing none of them can decide — **what a model's tool call means** — and that
//! is assembly policy, so it lives here.
//!
//! ## Blocking, and why that is the right shape for this build
//!
//! The tool delegates, waits, and returns the outcome. The kernel's delegation
//! has one way to hand a result back — the caller that is standing there gets it
//! — and a tool call is that caller, so the outcome is the answer and there is
//! nothing to queue. The other half, an outcome queued for a run loop that ends
//! its turn and comes back later, is not built (YA-40); queueing it here instead
//! would leave the parent's Inbox claiming pending work nobody is waiting to do.
//!
//! Because the call blocks, a model cannot delegate two things at once in one
//! step. That is a real limitation of the shape rather than a hidden one: the
//! port is what would change first, and it is the port that says a parent has
//! the child's result only when it waits.
//!
//! ## What the call costs, and who agrees to it
//!
//! The tool declares `subagent.spawn` and it is the only thing standing between
//! a configuration and a process being started for it: the default policy
//! advertises the capability and grants nothing, so a profile that wants to
//! delegate has to say so, and one that says `ask` gets the ordinary approval
//! flow. Nothing else about the delegation is negotiated — the child does not
//! inherit the parent's permissions, which is why the capability is
//! asked about the *parent's* act of delegating and not about what the child
//! will do.
//!
//! ## The child is a process this build starts, and it is recorded as a session
//!
//! A backend is a program named by the configuration, and the child's history is
//! a fork of the parent's — so a delegation produces two durable things: events
//! in the parent's log (`subagent/spawned`, then the settlement) and a session
//! of its own whose log opens by saying the objective it was forked with. The
//! tool touches neither directly. It asks the kernel driver to, which is the
//! point of the driver existing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_config::SubagentConfig;
use agent_kernel::resource_guard::ResourceGuard;
use agent_kernel::subagent::SubagentDelegation;
use agent_ports::event_store::EventStore;
use agent_ports::subagent::{SubagentOutcome, SubagentRequest, SubagentStatus};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource, ToolStatus,
};
use agent_protocol::ids::SessionId;
use agent_protocol::permissions::Capability;
use agent_subagent_stdio::StdioSubagentProvider;
use serde::Deserialize;
use serde_json::json;

/// The name a model calls this by.
pub const TOOL_NAME: &str = "subagent.run";

/// What a model reads when deciding whether to delegate.
///
/// The provider list is written in because it is the one thing about this tool
/// that the configuration decides, and a model that has to guess a backend name
/// will guess wrong — or, worse, call the tool with a name that exists in its
/// imagination and read the refusal as the task having failed.
const DESCRIPTION: &str = "Delegate one self-contained objective to a child agent. The child runs \
     as its own process and its own session, forked from this session's most recent completed \
     turn, and does not inherit this session's permissions or tools. This call blocks until the \
     child ends and returns what it said: its answer, or why it could not finish. Use it for work \
     that is separable and whose result you need before going on.";

/// The arguments a model may pass, refused if it passes anything else.
///
/// `deny_unknown_fields` is doing real work here: a tool argument that is
/// quietly ignored is a model believing it asked for something. A misspelled
/// `timeout_ms` would otherwise leave a child that runs for its provider's
/// ceiling while the caller thinks it set a deadline.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    /// Which configured backend runs the child.
    provider: String,
    /// What the child is asked to do, in the parent's words.
    objective: String,
    #[serde(default)]
    context_refs: Vec<String>,
    #[serde(default)]
    tool_allowlist: Vec<String>,
    #[serde(default)]
    model_route: Option<agent_protocol::model::ModelRoute>,
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    budget: Option<agent_protocol::profile::RunBudget>,
}

impl Args {
    fn into_request(self) -> SubagentRequest {
        SubagentRequest {
            objective: self.objective,
            context_refs: self.context_refs,
            tool_allowlist: self.tool_allowlist,
            model_route: self.model_route,
            budget: self.budget,
            output_schema: self.output_schema,
            timeout_ms: self.timeout_ms,
        }
    }
}

/// The delegation tool for one parent session.
///
/// Bound to the session it was assembled for, because the port's `invoke` is
/// handed an invocation and no session: the tool provider is not what decides
/// which session a call belongs to, the gateway is, and this build has one
/// session per command. A tool that took the parent session from its arguments
/// would let a model name someone else's.
pub struct SubagentToolProvider {
    parent: SessionId,
    /// One delegation per configured backend, keyed by the name a caller asks
    /// for. Keyed rather than listed because the caller *names* the backend: a
    /// single provider would have to decide which one an unqualified call meant,
    /// and that answer would change with the configuration.
    delegations: BTreeMap<String, SubagentDelegation>,
}

impl SubagentToolProvider {
    /// Build one delegation per configured backend.
    ///
    /// `tools` is the surface a fork repairs an inherited history with — the
    /// built-in family, deliberately not the surface this tool is part of: a
    /// composite cannot hold itself, and repairing a fork has no question about
    /// delegation in it.
    pub fn new(
        store: Arc<dyn EventStore>,
        tools: Arc<dyn ToolProvider>,
        parent: SessionId,
        configured: &[SubagentConfig],
    ) -> Self {
        let delegations = configured
            .iter()
            .map(|config| {
                let provider: Arc<dyn agent_ports::subagent::SubagentProvider> =
                    Arc::new(backend(config));
                (
                    config.provider.clone(),
                    SubagentDelegation::new(store.clone(), tools.clone(), provider),
                )
            })
            .collect();
        Self { parent, delegations }
    }

    /// The backends this run can delegate to, in name order.
    pub fn providers(&self) -> Vec<&str> {
        self.delegations.keys().map(String::as_str).collect()
    }
}

/// The configured backends whose working directory the boundary admits, with the
/// ones it does not dropped and the reason for each returned.
///
/// A backend's `cwd` is a path in a configuration file, and a path in a
/// configuration file is relative to the workspace root — never to the process's
/// working directory, which is what the child would otherwise inherit. It is
/// also held to the same boundary as any path a tool is handed, so a backend
/// cannot be started outside the workspace by configuration alone. Resolving it
/// here rather than in the adapter keeps that boundary in the one layer that
/// holds the workspace root, and hands the adapter an absolute path it can use
/// without knowing the workspace exists.
///
/// A backend the boundary refuses is **skipped, not fatal**: like a server that
/// does not answer it is reported and left off the surface, so a misconfigured
/// provider does not stop a run that has others — and a run whose every backend
/// was refused ends up with no delegation tool at all, which is what "no backend
/// this run can reach" should look like.
pub fn admit_backends(
    configured: &[SubagentConfig],
    guard: &ResourceGuard,
) -> (Vec<SubagentConfig>, Vec<String>) {
    let mut admitted = Vec::new();
    let mut notes = Vec::new();
    for config in configured {
        match &config.cwd {
            None => admitted.push(config.clone()),
            Some(cwd) => match guard.resolve(cwd) {
                Ok(path) => {
                    let mut config = config.clone();
                    config.cwd = Some(path.display().to_string());
                    admitted.push(config);
                }
                Err(error) => {
                    notes.push(format!(
                        "subagent provider '{}' is not started: {error}",
                        config.provider
                    ));
                }
            },
        }
    }
    (admitted, notes)
}

/// One configured backend, as the stdio adapter.
///
/// The ceiling is the configured one and never absent: a child with no deadline
/// is a process the parent cannot bound, and the adapter's own default is what
/// this build calls "no ceiling configured" — not what "no ceiling" means.
fn backend(config: &SubagentConfig) -> StdioSubagentProvider {
    let mut provider = StdioSubagentProvider::new(config.provider.clone(), config.command.clone())
        .with_args(config.args.clone())
        .with_env(config.env.clone());
    if let Some(cwd) = &config.cwd {
        provider = provider.with_cwd(PathBuf::from(cwd));
    }
    if let Some(ceiling_ms) = config.ceiling_ms {
        provider = provider.with_ceiling(Duration::from_millis(ceiling_ms));
    }
    provider
}

#[async_trait::async_trait]
impl ToolProvider for SubagentToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: TOOL_NAME.to_string(),
            description: format!(
                "{DESCRIPTION} Backends this run can delegate to: {}.",
                self.providers().join(", ")
            ),
            // Builtin, and deliberately not `Plugin`: nothing loaded this tool's
            // shape from anywhere. The *program* a child runs is configured, but
            // what a delegation is — its arguments, its result — is this binary's
            // own port, unlike an MCP tool whose shape a server declares.
            source: ToolSource::Builtin,
            // A child runs a program that can write files, commit, and call out.
            // Nothing about a delegation is reversible, so it is the strictest
            // class the vocabulary has rather than a judgement about this
            // adapter.
            side_effect: ToolSideEffect::Destructive,
            capability_requirements: vec![Capability::new(Capability::SUBAGENT_SPAWN)],
            // A second call starts a second child: the objective is the same,
            // the session is new, and the work happens twice.
            idempotent: false,
            // The deadline belongs to the request and to the configured ceiling,
            // which the backend enforces. A framework timeout here would be a
            // second, shorter bound whose expiry nobody recorded as a fact about
            // the child.
            timeout_ms: None,
            input_schema: self.input_schema(),
        }]
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        (tool_name == TOOL_NAME).then_some(ToolSideEffect::Destructive)
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let args: Args = serde_json::from_value(invocation.arguments.clone()).map_err(|error| {
            refused(format!("the arguments are not a subagent request: {error}"))
        })?;

        let Some(delegation) = self.delegations.get(&args.provider) else {
            return Err(refused(format!(
                "'{}' is not a backend this run has; it has: {}",
                args.provider,
                self.providers().join(", ")
            )));
        };

        // The clock covers the delegation and not the argument parsing: what a
        // caller wants to know is what the child cost, and the child starts at
        // the fork.
        let started = Instant::now();

        // `None` is the parent's most recent closed turn, which is the only
        // boundary a fork will copy up to: a child delegated mid-turn inherits
        // what the parent has finished, not what it is in the middle of.
        let handle = delegation
            .delegate(&self.parent, None, args.into_request())
            .await
            .map_err(|error| refused(error.to_string()))?;

        let outcome = delegation
            .settle(&self.parent, &handle)
            .await
            .map_err(|error| refused(error.to_string()))?;

        // The handle is released as soon as its ending is known, because this
        // build has no path that would ask about the child again — and a backend
        // that kept an entry per delegation would grow one per call. A failure
        // here is reported in the content rather than as the call failing: the
        // child did its job, and bookkeeping going wrong afterwards is not the
        // task's outcome.
        let mut content = describe(&handle.child_session_id, &outcome);
        if let Err(error) = delegation.dispose(&handle).await {
            content.push_str(&format!("\n(the child's handle could not be released: {error})"));
        }

        Ok(ToolResult {
            status: status_of(outcome.status()),
            content,
            artifacts: Vec::new(),
            truncated: false,
            facts: Default::default(),
            // This provider runs a child; it does not decide policy. The
            // gateway that handed it the call stamps the decision.
            policy: None,
            redaction_metadata: Default::default(),
            duration_ms: Some(started.elapsed().as_millis() as u64),
        })
    }
}

impl SubagentToolProvider {
    /// The arguments, as JSON Schema, with the backends this run actually has.
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "enum": self.providers(),
                    "description": "Which configured backend runs the child.",
                },
                "objective": {
                    "type": "string",
                    "description": "What the child is asked to do, in your own words. It becomes \
                                    the reason recorded in the child's history, and the child \
                                    sees it as its task.",
                },
                "context_refs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "References to context the child should be given: artifact \
                                    locators, event ids. Passed through verbatim, not resolved.",
                },
                "tool_allowlist": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Restrict which tools the child may use. Ask for it and a \
                                    backend that cannot restrict a child refuses the delegation.",
                },
                "model_route": {
                    "type": "object",
                    "description": "Pin the child to a model route.",
                },
                "output_schema": {
                    "type": "object",
                    "description": "Ask the child to answer in this shape. A backend passes it to \
                                    the child, and whether the child obeys is the child's; the \
                                    schema is a requirement the child is told, not a filter the \
                                    parent applies afterwards.",
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "How long the child may run, in milliseconds. A request can \
                                    shorten the configured ceiling and never extend it.",
                },
                "budget": {
                    "type": "object",
                    "description": "Bound what the child may spend. A backend that enforces no \
                                    budget refuses the delegation instead of dropping the limit.",
                },
            },
            "required": ["provider", "objective"],
            "additionalProperties": false,
        })
    }
}

/// How a child's ending reads as a tool call's status.
///
/// The same three groups the parent's log uses for the settlement event, because
/// a caller reading both should not have to reconcile two vocabularies: a
/// partial answer is a completed call that did not finish the job, and a
/// delegation the parent cancelled is neither of the two failure words.
fn status_of(status: SubagentStatus) -> ToolStatus {
    match status {
        SubagentStatus::Completed | SubagentStatus::Partial => ToolStatus::Completed,
        SubagentStatus::Failed | SubagentStatus::TimedOut => ToolStatus::Failed,
        SubagentStatus::Cancelled | SubagentStatus::CancelledUncertain => ToolStatus::Cancelled,
        // Unreachable: `settle` returns an outcome, and every outcome is
        // terminal. A tool is the wrong place to refuse a modelling error, so
        // the strictest reading — the call did not do what it was for — is the
        // one that fails closed.
        SubagentStatus::Running => ToolStatus::Failed,
    }
}

/// What the caller is told, in the child's own terms plus what the child was.
///
/// The session id is carried because it is the child's identity: it is where the
/// child's history is, and it is what the parent's log names in
/// `subagent/spawned`. Without it, "the child failed" is a fact a reader cannot
/// follow up.
fn describe(child: &SessionId, outcome: &SubagentOutcome) -> String {
    let mut text = format!("subagent {child}: {}", outcome.describe());
    match outcome {
        SubagentOutcome::Completed { result: Some(result), .. } => {
            text.push_str(&format!("\n{result}"));
        }
        SubagentOutcome::Failed { side_effects, .. } if !side_effects.is_empty() => {
            text.push_str("\nIt says it changed: ");
            text.push_str(&side_effects.join("; "));
        }
        _ => {}
    }
    text
}

/// A refusal in the vocabulary the gateway passes on.
fn refused(message: impl Into<String>) -> ToolProviderError {
    ToolProviderError::Failed { tool: TOOL_NAME.to_string(), message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::permissions::Workspace;
    use std::path::Path;

    /// A backend that names a program nothing here starts.
    ///
    /// These tests are about which backends a run keeps and where their working
    /// directory lands, and no command written here is ever executed — the
    /// adapter is not built, only the configuration list is filtered.
    fn config(provider: &str, cwd: Option<&str>) -> SubagentConfig {
        SubagentConfig {
            provider: provider.to_string(),
            command: "unused-by-this-test".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: cwd.map(str::to_string),
            requires: Vec::new(),
            ceiling_ms: None,
        }
    }

    fn guard(dir: &Path) -> ResourceGuard {
        ResourceGuard::new(Workspace {
            workspace_root: dir.display().to_string(),
            writable_roots: Vec::new(),
            external_directories: Vec::new(),
        })
    }

    #[test]
    fn a_relative_backend_directory_resolves_against_the_workspace_root() {
        // A `cwd` in a configuration file is relative to the workspace root, not
        // to wherever the process was launched — the child would otherwise be
        // started in the launcher's directory, which is the defect this method
        // exists to close. It is resolved to an absolute path here so the adapter
        // never has to know the workspace exists.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let guard = guard(&root);

        // The premise: the process is not sitting in the workspace, so nothing
        // resolves to the right place by accident.
        assert_ne!(std::env::current_dir().unwrap(), root.canonicalize().unwrap());

        let (admitted, notes) = admit_backends(&[config("one", Some("sub"))], &guard);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(admitted.len(), 1);
        assert_eq!(
            admitted[0].cwd.as_deref(),
            Some(root.canonicalize().unwrap().join("sub").to_str().unwrap())
        );
    }

    #[test]
    fn a_backend_whose_directory_escapes_the_workspace_is_skipped_with_a_note() {
        // The boundary is the same one every path a tool is handed is held to, so
        // a backend cannot be started outside the workspace by configuration
        // alone — and the run says so rather than starting it in the wrong place.
        let dir = tempfile::tempdir().unwrap();
        let guard = guard(dir.path());

        let (admitted, notes) = admit_backends(&[config("one", Some("../outside"))], &guard);
        assert!(admitted.is_empty(), "a backend outside the workspace was admitted");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("one"), "{:?}", notes[0]);
        assert!(notes[0].contains("not started"), "{:?}", notes[0]);
        assert!(notes[0].contains("path outside workspace"), "{:?}", notes[0]);
    }

    #[test]
    fn a_backend_with_no_configured_directory_is_admitted_unchanged() {
        // No `cwd` means the child inherits the process's working directory —
        // unchanged by this pass. The inconsistency between that and the
        // workspace-root rule is the configuration's to resolve, not something a
        // run may silently rewrite.
        let dir = tempfile::tempdir().unwrap();
        let guard = guard(dir.path());

        let (admitted, notes) = admit_backends(&[config("one", None)], &guard);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].cwd, None);
    }

    /// One delegation to a backend nothing ever starts, for the two argument
    /// checks below. Both refuse before the child would be forked, so the
    /// program named here is never executed.
    fn provider() -> SubagentToolProvider {
        SubagentToolProvider::new(
            Arc::new(agent_testkit::FakeEventStore::new()),
            Arc::new(agent_testkit::FakeToolProvider::new()),
            SessionId::from_string("parent"),
            &[config("fake", None)],
        )
    }

    /// The gateway refuses a name outside the backends this run has before the
    /// tool is reached, so this is the second line: a caller that bypasses it —
    /// or a declaration that stopped excluding the name — still gets a sentence
    /// naming what the run does have rather than a bare "unknown".
    #[tokio::test]
    async fn a_backend_the_run_does_not_have_is_named_with_the_ones_it_has() {
        let error = provider()
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "ghost", "objective": "x" }),
            ))
            .await
            .expect_err("a backend this run does not have was accepted");

        let message = error.to_string();
        assert!(message.contains("'ghost' is not a backend this run has"), "{message}");
        assert!(message.contains("fake"), "it says what the run has: {message}");
    }

    /// Likewise for an argument the request does not define: the schema says
    /// `additionalProperties: false`, and this is the check behind it.
    #[tokio::test]
    async fn an_argument_the_request_does_not_define_never_reaches_a_child() {
        let error = provider()
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "x", "timeout": "5s" }),
            ))
            .await
            .expect_err("an undefined argument was accepted");

        let message = error.to_string();
        assert!(message.contains("not a subagent request"), "{message}");
        assert!(message.contains("timeout_ms"), "it names the field it wanted: {message}");
    }

    #[test]
    fn one_refused_backend_does_not_take_the_others_with_it() {
        // A misconfigured provider is skipped, not fatal: a run that has a backend
        // it can reach still has one.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let guard = guard(&root);

        let (admitted, notes) = admit_backends(
            &[config("bad", Some("../outside")), config("good", Some("sub"))],
            &guard,
        );
        assert_eq!(admitted.len(), 1, "{admitted:?}");
        assert_eq!(admitted[0].provider, "good");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("bad"), "{:?}", notes[0]);
    }
}
