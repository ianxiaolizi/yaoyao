//! `agent subagent serve` — the child half of a `kind = "self"` delegation.
//!
//! A `self` backend starts *this binary* in a process of its own and speaks the
//! subagent wire to it: one request line on stdin, one result line on stdout.
//! What this module owns is the child's whole assembly, and it is deliberately
//! **not** the parent's: the child reads no configuration file, mounts no
//! plugins, starts no MCP server, cannot delegate in turn, and cannot be handed
//! anything the parent has not written on its command line.
//!
//! ## The grants are the whole of its authority
//!
//! Every capability the child may use arrives as `--grants`, already intersected
//! by the parent with what the parent itself holds and with what the sandbox
//! provides. The child intersects once more with its own backend, then builds a
//! policy with **no ask rule at all**: a child that stopped to ask would be
//! asking nobody. Anything outside the grants is refused as ungranted, which is
//! a deterministic refusal rather than a prompt.
//!
//! ## Where it may write
//!
//! `--scope` names directory prefixes, already resolved by the parent and
//! checked to lie inside the parent's writable face. The child's file boundary
//! uses them as its writable roots, so a write outside them is refused by the
//! same guard that refuses a write outside the workspace. With no `--scope` the
//! child is **read-and-answer**: its surface is filtered to the read-only tools
//! before the policy ever sees a call.
//!
//! ## Storage
//!
//! The child writes to `--data-dir`, which is the parent's, because the child's
//! session is a fork of the parent's and both live in one store. It writes only
//! its own session: the parent's log is the parent's, and the settlement is
//! written there by the parent's collector.
//!
//! ## What it says on stdout
//!
//! One line, in the vocabulary [`agent_subagent_stdio::wire`] defines. Progress
//! is not reported: the port has no channel for it, and a line the parent
//! discards would only be noise in a pipe that also carries the answer.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use agent_cli::args::Args;
use agent_context::ProjectionContextEngine;
use agent_kernel::approval::ApprovalService;
use agent_kernel::guarded::GuardedToolProvider;
use agent_kernel::live_bus::InMemoryLiveEventBus;
use agent_kernel::policy::{PolicyEngine, PolicyRule};
use agent_kernel::resource_guard::ResourceGuard;
use agent_kernel::scheduler::Scheduler;
use agent_kernel::tools::CompositeToolProvider;
use agent_loop::DefaultAgentLoop;
use agent_model_openai::{ENV_API_KEY, OpenAiCompatConfig, OpenAiCompatProvider, ReqwestTransport};
use agent_ports::agent_loop::TurnCapabilityFacts;
use agent_ports::artifact_store::ArtifactStore;
use agent_ports::execution_environment::{ExecutionEnvironment, SandboxMode};
use agent_ports::model_router::ModelTier;
use agent_ports::subagent::SubagentOutcome;
use agent_ports::tool_gateway::ToolGateway;
use agent_ports::tool_provider::ToolProvider;
use agent_protocol::events::Actor;
use agent_protocol::ids::SessionId;
use agent_protocol::inbox::InboxKind;
use agent_protocol::model::ModelRoute;
use agent_protocol::permissions::{Capability, Workspace};
use agent_protocol::profile::RunBudget;
use agent_protocol::redactor::{HighEntropy, Redactor};
use agent_store_artifact::FsArtifactStore;
use agent_store_jsonl::JsonlEventStore;
use agent_store_secret::EnvSecretProvider;
use agent_subagent_stdio::wire::RequestMessage;
use agent_tools_fs::{FsToolProvider, LocalExecutionEnvironment};
use agent_tools_git::GitToolProvider;
use serde_json::json;

use crate::providers::{self, SelectedProvider};
use crate::{clock, inprocess};

/// The subcommand word under `agent subagent`.
pub const SERVE: &str = "serve";

/// Run the child half: read one request, run one turn, write one result line.
///
/// An `Err` is a child that never started a turn, and the caller turns it into
/// the "did not run" status with the sentence on stderr. A turn that ran is
/// always a result line, whatever it ended as: the child's ending is data, not
/// an exit status.
pub async fn serve(args: &Args) -> Result<(), String> {
    let options = Options::read(args)?;
    let request = request_line().await?;
    let outcome = run(&options, &request).await?;

    let line = result_line(&outcome);
    let mut stdout = std::io::stdout();
    stdout
        .write_all(line.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("the result could not be written: {error}"))?;
    Ok(())
}

/// The deployment facts the parent fixes when it builds the backend.
///
/// Every one of them is an argument. The child reads no configuration file and
/// inherits no `AGENT_*` variable, so this is the whole of what it is told
/// beyond the request line itself.
#[derive(Debug)]
struct Options {
    child_session: SessionId,
    workspace_root: PathBuf,
    data_dir: PathBuf,
    /// The provider kind this build knows: `echo` or `openai`.
    provider_kind: String,
    model: Option<String>,
    base_url: Option<String>,
    /// The capabilities the parent authorized, already intersected.
    grants: Vec<Capability>,
    /// Where this child may write. Empty means read-and-answer.
    scopes: Vec<PathBuf>,
    /// `on`, `gates` or `off` — the parent's judgment arm.
    judgment: String,
    network_allowed: bool,
}

impl Options {
    fn read(args: &Args) -> Result<Self, String> {
        let required = |name: &str| {
            option(args, name)?.ok_or_else(|| format!("{name} is required for a self child"))
        };

        let child_session = SessionId::from_string(&required("--child-session")?);
        let workspace_root = PathBuf::from(required("--workspace")?);
        let data_dir = PathBuf::from(required("--data-dir")?);
        let provider_kind = required("--provider")?;
        // `--model` is a flag the command line parser takes out of `rest`
        // wherever it appears, because `agent run --model` is the same fact.
        // Reading it through [`option`] would always come back empty and the
        // child would report that it was told no model.
        let model = args.model.clone();
        let base_url = option(args, "--base-url")?;
        let grants = match option(args, "--grants")? {
            Some(list) => list
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(Capability::new)
                .collect(),
            None => Vec::new(),
        };
        let scopes: Vec<PathBuf> = values(args, "--scope").into_iter().map(PathBuf::from).collect();
        if !scopes.is_empty() && grants.is_empty() {
            return Err("--scope was given with no --grants: a child with somewhere to write and \
                 nothing authorized to do there would only ever be refused"
                .to_string());
        }
        let judgment = option(args, "--judgment")?.unwrap_or_else(|| "gates".to_string());
        if !matches!(judgment.as_str(), "on" | "gates" | "off") {
            return Err(format!("--judgment is '{judgment}'; use on, gates or off"));
        }
        let network_allowed = match option(args, "--network")?.as_deref() {
            None | Some("deny") => false,
            Some("allow") => true,
            Some(other) => return Err(format!("--network is '{other}'; use allow or deny")),
        };
        Ok(Self {
            child_session,
            workspace_root,
            data_dir,
            provider_kind,
            model,
            base_url,
            grants,
            scopes,
            judgment,
            network_allowed,
        })
    }

    /// Whether this child may write anywhere at all.
    fn read_only(&self) -> bool {
        self.scopes.is_empty()
    }
}

/// A command's own option, or the failure of a flag given without a value.
fn option(args: &Args, name: &str) -> Result<Option<String>, String> {
    args.option(name).map_err(|error| error.to_string())
}

/// Every value of a repeatable `--name <value>` flag, in the order given.
///
/// [`Args::option`] answers "the first, or a refusal"; a scope is a list, and
/// each entry is a separate `--scope`.
fn values(args: &Args, name: &str) -> Vec<String> {
    let prefix = format!("{name}=");
    let mut found = Vec::new();
    let mut tokens = args.command_args().iter();
    while let Some(token) = tokens.next() {
        if token == name {
            if let Some(value) = tokens.next() {
                found.push(value.clone());
            }
        } else if let Some(value) = token.strip_prefix(&prefix).filter(|value| !value.is_empty()) {
            found.push(value.to_string());
        }
    }
    found
}

/// The request, read from stdin.
///
/// Blocking, on a task of its own: the child does exactly one thing before this
/// point and nothing else is running, so a thread parked on a pipe costs
/// nothing — and it is the one shape that cannot deadlock against the parent's
/// writer.
async fn request_line() -> Result<RequestMessage, String> {
    let (read, line) = tokio::task::spawn_blocking(|| {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map(|read| (read, line))
    })
    .await
    .map_err(|error| format!("reading the request was cut short: {error}"))?
    .map_err(|error| format!("the request could not be read: {error}"))?;

    if read == 0 {
        return Err("the parent wrote no request and closed the pipe".to_string());
    }
    let request: RequestMessage = serde_json::from_str(line.trim_end()).map_err(|error| {
        format!("the request is not one this build reads: {error}; the line was: {line}")
    })?;
    // The version is checked the same way the parent checks a result, so two
    // halves that disagree say so instead of half-working.
    agent_protocol::versioning::check(
        &request.protocol_version,
        &agent_protocol::versioning::formats::SUBAGENT_WIRE,
    )
    .map_err(|error| format!("the request's wire version: {error}"))?;
    Ok(request)
}

/// Assemble the child's stack, run one turn on the forked session, and say how
/// it ended.
async fn run(options: &Options, request: &RequestMessage) -> Result<SubagentOutcome, String> {
    let store = Arc::new(JsonlEventStore::new(&options.data_dir));

    // What this child's own backend can do, read once. The parent's grants are
    // intersected with it as well: a grant this process cannot honour is not a
    // permission, it is a claim nothing here can keep.
    let environment: Arc<dyn ExecutionEnvironment> =
        Arc::new(LocalExecutionEnvironment::new(&options.workspace_root));
    let advertised = environment.advertised_capabilities();
    let granted: Vec<Capability> =
        options.grants.iter().filter(|grant| advertised.contains(*grant)).cloned().collect();

    let families: Vec<Arc<dyn ToolProvider>> = vec![
        Arc::new(FsToolProvider::new(environment)),
        Arc::new(GitToolProvider::new(options.workspace_root.clone())),
    ];
    let builtins: Arc<dyn ToolProvider> =
        Arc::new(CompositeToolProvider::new(families).map_err(|error| error.to_string())?);
    // The read-only arm is a *surface* rather than a policy rule: a write tool
    // that is not offered cannot be named, which is stronger than a refusal at
    // call time.
    let surface: Arc<dyn ToolProvider> = if options.read_only() {
        Arc::new(inprocess::ReadOnlySurface::new(builtins))
    } else {
        builtins
    };

    // Allow rules only. There is no `ask` anywhere in a child's policy, because
    // a child that stopped to ask would be asking nobody — the refusal is what
    // "outside the scope is refused, not negotiated" means here.
    let policy = PolicyEngine::new()
        .with_sandbox(granted.iter().cloned())
        .with_rules(granted.iter().map(|grant| PolicyRule::allow(grant.clone())));

    // The writable face, and the whole of it. With no scope the guard's
    // writable roots are empty, but nothing on the surface can write either.
    let guard = ResourceGuard::new(Workspace {
        workspace_root: options.workspace_root.display().to_string(),
        writable_roots: options.scopes.iter().map(|path| path.display().to_string()).collect(),
        external_directories: Vec::new(),
    });

    let approvals = Arc::new(ApprovalService::new(store.clone()).with_session_scope(true));
    let artifacts: Arc<dyn ArtifactStore> =
        Arc::new(FsArtifactStore::new(options.data_dir.join("artifacts")));
    let redactor = Redactor::new().with_high_entropy(HighEntropy::Warn);
    let budget = RunBudget::default();
    let gateway: Arc<dyn ToolGateway> = Arc::new(
        GuardedToolProvider::new(surface, policy, approvals, guard, artifacts)
            .with_offload_limit(budget.tool_output_limit_bytes())
            .with_redactor(redactor.clone()),
    );

    let selected = provider(options)?;
    let routes: BTreeMap<String, ModelRoute> = BTreeMap::new();
    let router = Arc::new(providers::router(selected.route.clone(), &routes));
    let context = Arc::new(ProjectionContextEngine::new(store.clone(), &options.data_dir));
    let live = Arc::new(InMemoryLiveEventBus::new(64));
    let mut loop_ = DefaultAgentLoop::new(selected.provider, gateway, context, router)
        .with_live_bus(live)
        .with_redactor(redactor);
    match options.judgment.as_str() {
        "on" => {}
        "gates" => loop_ = loop_.without_ledger(),
        _ => loop_ = loop_.without_judgment(),
    }

    let scheduler = Scheduler::new(store, Arc::new(loop_), budget.max_steps).with_capability(
        TurnCapabilityFacts {
            sandbox: SandboxMode::LocalProcess,
            network_allowed: options.network_allowed,
            tool_secrets: Vec::new(),
        },
    );

    scheduler
        .inbox()
        .append(
            &options.child_session,
            InboxKind::NextTurn,
            json!({ "text": objective(request) }),
            0,
            Actor::Agent,
        )
        .await
        .map_err(|error| format!("the objective could not be queued: {error}"))?;

    let report = scheduler
        .run_pending(&options.child_session, clock::now())
        .await
        .map_err(|error| format!("the turn could not be run: {error}"))?
        .ok_or_else(|| {
            format!(
                "session {} had no pending work when the turn was about to start",
                options.child_session
            )
        })?;

    Ok(inprocess::describe_report(&report, &options.child_session))
}

/// What a child's surface can require, given whether it may write.
///
/// The parent asks this to work out what it can lend a child, so the answer has
/// to be the surface the child actually builds — which is why both read the same
/// list of families. A capability named here and absent from the child's surface
/// would grant nothing; one absent here and needed there would be a tool the
/// child sees and every call refuses.
pub fn surface_capabilities(workspace_root: &std::path::Path, read_only: bool) -> Vec<Capability> {
    let workspace = workspace_root.display().to_string();
    let families: Vec<Arc<dyn ToolProvider>> = vec![
        Arc::new(FsToolProvider::new(Arc::new(LocalExecutionEnvironment::new(workspace_root)))),
        Arc::new(GitToolProvider::new(std::path::PathBuf::from(workspace))),
    ];
    let mut needed: Vec<Capability> = Vec::new();
    for provider in families {
        for definition in provider.definitions() {
            if read_only && !inprocess::is_read_only(definition.side_effect) {
                continue;
            }
            for capability in definition.capability_requirements {
                if !needed.contains(&capability) {
                    needed.push(capability);
                }
            }
        }
    }
    needed
}

/// The objective as the child reads it: the parent's words, and the references
/// it named, which are passed through rather than resolved.
fn objective(request: &RequestMessage) -> String {
    let mut text = request.objective.clone();
    if !request.context_refs.is_empty() {
        text.push_str(&format!(
            "\n\nThe session that delegated this named: {}",
            request.context_refs.join(", ")
        ));
    }
    text
}

/// The model provider this child runs on, built from the arguments and nothing
/// else.
///
/// The one value that is not an argument is the key, which is read from the
/// environment because a credential has to be. It is there only because the
/// parent's backend configuration admitted that one name
/// (`agent_ports::process_env`), so a child started by anything else has none.
fn provider(options: &Options) -> Result<SelectedProvider, String> {
    match options.provider_kind.as_str() {
        "echo" => providers::select_kind("echo", options.model.as_deref())
            .map_err(|error| error.to_string()),
        "openai" => {
            let base_url = options.base_url.clone().ok_or_else(|| {
                "--provider openai needs --base-url; the child reads no configuration".to_string()
            })?;
            let model = options.model.clone().ok_or_else(|| {
                "--provider openai needs --model; the child reads no configuration".to_string()
            })?;
            let config = OpenAiCompatConfig::new(base_url, model);
            let route =
                ModelRoute::new(ModelTier::Main, config.provider_id.clone(), config.model.clone());
            let secrets = Arc::new(EnvSecretProvider::new([ENV_API_KEY]));
            let transport = Arc::new(ReqwestTransport::new().map_err(|error| error.to_string())?);
            Ok(SelectedProvider {
                provider: Arc::new(OpenAiCompatProvider::new(config, secrets, transport)),
                route,
            })
        }
        other => {
            Err(format!("--provider is '{other}'; this build runs a child on 'echo' or 'openai'"))
        }
    }
}

/// How the parent is told how this child ended, as one wire line.
///
/// The three statuses a child may report are the wire's own vocabulary, and the
/// endings only a parent can know — cancelled, timed out — are never among them:
/// the child was stopped, it did not decide to stop, and a child saying
/// otherwise would be describing something it cannot know.
fn result_line(outcome: &SubagentOutcome) -> String {
    let value = match outcome {
        SubagentOutcome::Completed { summary, result } => {
            json!({ "type": "result", "status": "completed", "summary": summary, "result": result })
        }
        SubagentOutcome::Partial { summary } => {
            json!({ "type": "result", "status": "partial", "summary": summary })
        }
        SubagentOutcome::Failed { message, side_effects } => {
            json!({
                "type": "result",
                "status": "failed",
                "message": message,
                "side_effects": side_effects,
            })
        }
        // The child was stopped rather than having stopped, which the wire has
        // no status for. Said as a failure so the parent does not read a
        // truncated turn as an answer; the parent's own collector is what knows
        // a stop happened, and it records that instead. The wording avoids the
        // stopped vocabulary on purpose: a child that named it would be
        // describing a decision that was not its own.
        SubagentOutcome::Cancelled => json!({
            "type": "result",
            "status": "failed",
            "message": "this turn ended because it was stopped from outside before it answered",
        }),
        SubagentOutcome::CancelledUncertain { .. } => json!({
            "type": "result",
            "status": "failed",
            "message": "this turn was asked to end from outside and its end was not observed",
        }),
        SubagentOutcome::TimedOut { .. } => json!({
            "type": "result",
            "status": "failed",
            "message": "this turn ended at its deadline without an answer",
        }),
    };
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(extra: &[&str]) -> Vec<String> {
        let base = [
            "subagent",
            "serve",
            "--child-session",
            "child",
            "--workspace",
            "ws",
            "--data-dir",
            "data",
            "--provider",
            "echo",
        ];
        base.iter().chain(extra.iter()).map(|word| word.to_string()).collect()
    }

    fn parsed(argv: &[String]) -> Args {
        agent_cli::args::parse(argv).expect("a readable command line")
    }

    /// The child reports its ending as data, in the wire's own three statuses.
    #[test]
    fn an_ending_becomes_a_result_line_the_parent_reads_back() {
        let completed = result_line(&SubagentOutcome::Completed {
            summary: "done".to_string(),
            result: Some(json!({ "count": 2 })),
        });
        let parsed = agent_subagent_stdio::wire::parse(&completed).expect("the parent can read it");
        assert_eq!(
            parsed,
            agent_subagent_stdio::wire::ChildMessage::Result(SubagentOutcome::Completed {
                summary: "done".to_string(),
                result: Some(json!({ "count": 2 })),
            })
        );

        let failed = result_line(&SubagentOutcome::Failed {
            message: "no such file".to_string(),
            side_effects: vec!["created src/x.rs".to_string()],
        });
        match agent_subagent_stdio::wire::parse(&failed) {
            Ok(agent_subagent_stdio::wire::ChildMessage::Result(SubagentOutcome::Failed {
                side_effects,
                ..
            })) => assert_eq!(side_effects, vec!["created src/x.rs".to_string()]),
            other => panic!("a failure with side effects must survive the wire: {other:?}"),
        }
    }

    /// A stop the parent caused is not one of the three statuses, so it is not
    /// reported as one: the line says failed, and the parent's collector is what
    /// records the stop as a stop.
    #[test]
    fn a_stop_this_child_did_not_decide_is_not_reported_as_a_status_it_cannot_have() {
        let line = result_line(&SubagentOutcome::Cancelled);
        assert!(line.contains("\"status\":\"failed\""), "{line}");
        assert!(!line.contains("cancel"), "a child may not name a cancelled result: {line}");
    }

    /// A scope is a repeatable flag, and each occurrence is one directory.
    #[test]
    fn a_repeatable_scope_flag_reads_every_value_in_order() {
        let argv = facts(&["--scope", "a", "--scope=b"]);
        assert_eq!(values(&parsed(&argv), "--scope"), vec!["a".to_string(), "b".to_string()]);
        // Only the flag asked for: a shorter name is a prefix of the flag, not
        // the flag, and must not pick up its values.
        assert_eq!(values(&parsed(&argv), "--child-session"), vec!["child".to_string()]);
        assert!(values(&parsed(&argv), "--scop").is_empty());
    }

    /// The deployment facts decide the arm: no scope is a read-and-answer child.
    #[test]
    fn a_child_with_no_scope_is_the_read_only_one() {
        let argv = facts(&["--grants", "fs.read"]);
        let read_only = Options::read(&parsed(&argv)).expect("the facts resolve");
        assert!(read_only.read_only());
        assert_eq!(read_only.grants, vec![Capability::new(Capability::FS_READ)]);

        // A scope without a grant is refused rather than accepted and then
        // refused at every call: it is a child built to fail.
        let argv = facts(&["--scope", "ws/src"]);
        assert!(Options::read(&parsed(&argv)).is_err(), "a scope with no grants must be refused");
    }

    /// A child with nowhere to write is still a child, and its missing
    /// deployment facts are named in the refusal.
    #[test]
    fn a_fact_the_parent_did_not_state_is_named_in_the_refusal() {
        let argv: Vec<String> = ["subagent", "serve"].iter().map(|w| w.to_string()).collect();
        let error = Options::read(&parsed(&argv)).expect_err("nothing was stated");
        assert!(error.contains("--child-session"), "{error}");
    }

    /// A request whose references were named carries them into the objective as
    /// data, in the same shape the in-process backend uses.
    #[test]
    fn named_references_are_appended_to_the_objective() {
        let request: RequestMessage = serde_json::from_str(
            r#"{"type":"request","objective":"read it","context_refs":["artifact://abc"]}"#,
        )
        .expect("a request this build writes");
        let text = objective(&request);
        assert!(text.starts_with("read it"), "{text}");
        assert!(text.contains("artifact://abc"), "{text}");
    }
}
