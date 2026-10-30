//! Agent binary — the only concrete assembly point.
//!
//! Wires the real JSONL store, the default loop (echo model by default, a real
//! OpenAI-compatible endpoint when `AGENT_MODEL_PROVIDER=openai`), the guarded
//! tool gateway (policy + approval + resource boundary + redaction), and the
//! kernel Scheduler/Inbox/Recovery into CLI commands.
//!
//! Everything shared by the commands — which session to act on, whether the
//! output is structured — is read once from the parsed command line, and every
//! command reports through one place so that `--events` means the same thing
//! wherever it is used.

#![deny(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_cli::args::{self, Args, DEFAULT_SESSION};
use agent_cli::context::run_context_inspect;
use agent_cli::memory::{DEFAULT_SEARCH_LIMIT, run_memory_reindex, run_memory_search};
use agent_cli::reason::run_why;
use agent_cli::schedule::{
    SERVE_POLL, Tick, earliest_due, next_wait, render_did_not_run, render_ran, run_schedule_at,
    run_schedule_cancel, run_schedule_list, run_tick, tick_once,
};
use agent_cli::{
    Verdict, run_approve, run_artifact_get, run_ask, run_deny, run_doctor, run_export, run_fork,
    run_interrupt, run_resume, run_status, run_steer, run_tool,
};
use agent_index_sqlite::SqliteMemoryIndex;
use agent_kernel::doctor::KernelDoctor;
use agent_kernel::inbox::Inbox;
use agent_ports::context_engine::ContextEngine;
use agent_ports::event_store::EventStore;
use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId, TurnId};
use agent_store_artifact::FsArtifactStore;
use agent_store_jsonl::JsonlEventStore;

use agent::checks::{
    BuildHealthCheck, ExecutionSandboxCheck, ProfileHealthCheck, SessionHealthCheck,
};
use agent::{config, plugins, sandbox};

mod support;

use support::*;

/// The exit status of a command whose subject answered "no".
///
/// Reserved for an answer that *is* a refusal: the command ran, the thing it was
/// asked about came back, and what came back was bad. Two commands have such a
/// subject — `tool`, when the call itself failed, and `doctor`, when the build
/// is unhealthy — and neither writes `1` itself: both say `AnsweredNo` and
/// [`exit_on`] reads the number off. It is named here rather than there because
/// it is about what the status means to a caller, not about which command
/// happens to return it first.
const EXIT_ANSWERED_NO: i32 = 1;

/// The exit status of a command that did not run.
///
/// Every way a command can end without running: a usage error, a configuration
/// that does not resolve, a name nobody registered, a call the gateway would not
/// dispatch. The two statuses are the whole alphabet — a caller that reads only
/// `$?` learns whether the answer was "no" or whether there was no answer at
/// all — and this is the one that must never be confused with the other, because
/// "nothing ran, read stderr" and "it ran, read the answer" call for opposite
/// things next.
const EXIT_DID_NOT_RUN: i32 = 2;

/// The port `agent web` listens on unless `--port` names another.
const DEFAULT_WEB_PORT: u16 = 7431;

/// The exit status a verdict calls for — the whole alphabet, in one place.
///
/// One place, because a second one would be a second answer to the same
/// question: two sites that each decided what "no" costs could disagree, and
/// nothing would say which was right. Everything a command learns about its own
/// outcome arrives as a [`Verdict`]; this is what turns it into what a caller
/// reads.
fn exit_status(verdict: Verdict) -> i32 {
    match verdict {
        Verdict::Answered => 0,
        Verdict::AnsweredNo => EXIT_ANSWERED_NO,
        Verdict::DidNotRun => EXIT_DID_NOT_RUN,
    }
}

/// End the process on a command's verdict, unless the verdict is an answer.
///
/// Every command that is handed a verdict ends here, so a verdict is honoured
/// wherever it is produced rather than only where someone remembered. An
/// `Answered` verdict is left alone: falling out of the command is the same
/// exit, without skipping a destructor on the way.
fn exit_on(verdict: Verdict) {
    let status = exit_status(verdict);
    if status != 0 {
        std::process::exit(status);
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // The sandbox file worker runs this same binary inside the AppContainer. It
    // is not a user command — it takes one root and speaks the worker protocol
    // on stdio — so it is dispatched before the argument parser, which would
    // refuse a command it does not know.
    if argv.first().is_some_and(|argument| argument == agent_tools_fs::WORKER_COMMAND) {
        let root = argv
            .iter()
            .position(|argument| argument == "--root")
            .and_then(|index| argv.get(index + 1));
        let Some(root) = root else {
            eprintln!("the sandbox file worker needs --root");
            std::process::exit(EXIT_DID_NOT_RUN);
        };
        match agent_tools_fs::serve_worker(Path::new(root)) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("the sandbox file worker failed: {error}");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
        }
    }
    let args = match args::parse(&argv) {
        Ok(args) => args,
        Err(args::ArgsError::NoCommand) => {
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        Err(e) => {
            eprintln!("{e}");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    };

    match args.command.as_str() {
        "doctor" => doctor(&args).await,
        "run" | "ask" => run(&args).await,
        "resume" => resume(&args).await,
        "fork" => fork(&args).await,
        "steer" => steer(&args).await,
        "interrupt" => interrupt(&args).await,
        "export" => export(&args).await,
        "status" => status(&args).await,
        "why" => why(&args).await,
        "approve" => approve(&args).await,
        "deny" => deny(&args).await,
        "tool" => tool(&args).await,
        "plugin" => plugin(&args).await,
        "session" => session(&args).await,
        "schedule" => schedule(&args).await,
        "tick" => tick(&args).await,
        "serve" => serve(&args).await,
        "web" => web(&args).await,
        "memory" => memory(&args).await,
        "context" => context(&args).await,
        "metrics" => metrics(&args).await,
        "artifact" => artifact(&args).await,
        "sandbox" => sandbox_command(&args),
        other => {
            eprintln!("unknown command '{other}'");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

fn usage() {
    eprintln!(
        "Usage: agent <command> [options]\n\n\
         Commands:\n  \
         run <text>                 Ask the agent to do something (alias: ask)\n  \
         ask <text>                 Alias of run\n  \
         resume [text]              Continue a turn that was waiting on a decision\n  \
         fork [--at <turn>]         Derive a new session from a closed turn boundary\n  \
         steer \"<text>\"           Queue text for the turn in flight (applied at the next step)\n  \
         interrupt                  Stop the run in flight at its next step boundary\n  \
         export [--out <dir>]       Write the session as session.md + bundle.json (or stdout)\n  \
         export --redacted          Same, with secret-shaped tokens replaced by ***\n  \
         why [--turn <id>] [--depth <n>]\n                              Explain that turn's reasoning chain, backwards\n                              (--help says what a turn's end and verdict mean)\n  \
         status                     Show session state\n  \
         approve <id> [scope]       Approve a pending request [once|turn|session|always]\n  \
         deny <id> [--only]         Deny a pending request (cascades to the session's pending)\n  \
         tool <name> [json-args]    Run a tool through the policy gateway\n  \
         plugin list                What the configuration, the log and the queue say about plugins\n  \
         plugin disable <id>        Ask for a plugin to be withdrawn at the next turn boundary\n  \
         plugin enable <id>         Ask for a disabled plugin to be registered again\n  \
         session new [--id <id>]    Record that a session has started (idempotent)\n  \
         session list               Every session under the data directory, newest first\n  \
         session archive <id>       Hide a session from the working list; its log stays\n  \
         session unarchive <id>     Show an archived session again\n  \
         session delete <id>        Remove a session's log (refuses while a turn runs there)\n  \
         schedule at <when> <text>  Schedule a turn: RFC3339 or +10m; run it with tick\n  \
         schedule list              Show scheduled turns that have not run\n  \
         schedule cancel <id>       Withdraw a scheduled turn before it runs\n  \
         tick                       Run at most one pending or due turn\n  \
         serve                      Keep running turns as they become due (one per session)\n  \
         web [--port <n>]           Serve the GUI on loopback, token printed once\n  \
         memory search <text>       Find events whose text contains <text>\n  \
         memory reindex             Rebuild the searchable index from the session log\n  \
         context inspect            Show what the next request would carry (size, kinds, budget)\n  \
         metrics [--out <path>]     Count turns, tools, approvals, tokens and time\n                              from the event log (every session unless --session names one)\n  \
         artifact get <ref> [--out] Fetch a stored blob (locator or hash). Text to stdout otherwise\n  \
         sandbox setup              Create the AppContainer profile and its grants (elevated)\n  \
         sandbox status             Report whether the AppContainer setup is in place\n  \
         doctor                     Run health checks\n\n\
         Options:\n  \
         --session <id>             Act on this session (default: {DEFAULT_SESSION})\n  \
         --profile <name>           Compose this profile for the run\n  \
         --model <name>             Ask the provider for this model\n  \
         --json                     One structured object on stdout\n  \
         --events                   The events this command produced, one JSON object per line\n\
         \nOptions may appear before or after the command. --json and --events are \
         mutually exclusive.\n\n\
         Configuration, in order of authority:\n  \
         {managed}   machine-wide policy (deny list)\n  \
         {user}      capabilities and trusted plugins\n  \
         <workspace>/{workspace}   project configuration\n\
         \nA profile selects inside what the user configuration authorized; it cannot add to it.",
        managed = config::ENV_MANAGED_POLICY,
        user = config::ENV_USER_CONFIG,
        workspace = config::WORKSPACE_CONFIG,
    );
}

async fn doctor(args: &Args) {
    let schema_version = SchemaVersion::current();
    let data_dir = PathBuf::from(data_dir());

    // Zero wait, because a session another process is driving has no evidence
    // to offer right now: waiting for the writer would make `doctor` hang behind
    // a long turn, and judging the log mid-append would call a live writer's
    // torn line a crash. The store is shared by the session checks so a log is
    // replayed once, and `log_tail` is registered first because it reports the
    // repair that replay performs.
    let store = Arc::new(JsonlEventStore::new(&data_dir).with_lease_wait(Duration::ZERO));
    let doctor = KernelDoctor::new(vec![
        Box::new(BuildHealthCheck::new(schema_version.clone())),
        Box::new(SessionHealthCheck::log_tail(store.clone(), data_dir.clone())),
        Box::new(SessionHealthCheck::seq_contiguity(store.clone(), data_dir.clone())),
        Box::new(SessionHealthCheck::open_entities(store.clone(), data_dir.clone())),
        Box::new(SessionHealthCheck::checkpoint_consistency(store, data_dir)),
        Box::new(ProfileHealthCheck::new(workspace_root(), args.clone())),
        Box::new(ExecutionSandboxCheck::from_environment()),
    ])
    .expect("the doctor's check IDs are unique");
    let outcome = match run_doctor(&doctor, schema_version, args.json).await {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    print(&outcome.output);
    // The verdict goes to the exit status, not into the report: a check that
    // reports "unhealthy" and exits `0` is a check nobody can act on (§3 C3.4).
    exit_on(outcome.verdict);
}

/// `sandbox setup|status`: the machine-level half of the AppContainer backend.
///
/// Not a session command: it reads no workspace configuration, opens no store
/// and writes no event. `setup` changes the machine once, from an elevated
/// prompt; `status` only reads, and is what the doctor and a caller who wants to
/// gate on readiness use.
fn sandbox_command(args: &Args) {
    match args.command_args().first().map(String::as_str) {
        Some("setup") => sandbox_setup(args),
        Some("status") => sandbox_status(args),
        Some(other) => {
            eprintln!("unknown sandbox subcommand '{other}'");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        None => {
            eprintln!(
                "Usage: agent sandbox setup [--readonly <path>]...\n       agent sandbox status"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// `sandbox setup`: idempotent, and refuses rather than half-succeeds.
fn sandbox_setup(args: &Args) {
    let readonly = sandbox_readonly_paths(args);
    let report = agent_sandbox_win::setup(agent_sandbox_win::PROFILE_NAME, &readonly)
        .unwrap_or_else(|e| fail(args, &e.to_string()));
    if args.json {
        match agent_cli::json_output(&serde_json::json!({
            "profile": report.profile,
            "sid": report.sid,
            "created": report.created,
            "nul_device": report.nul_granted,
            "read_execute": report.granted,
        })) {
            Ok(text) => println!("{text}"),
            Err(e) => fail(args, &e.to_string()),
        }
        return;
    }
    println!(
        "execution sandbox: profile={} sid={} created={} nul_device={}",
        report.profile,
        report.sid,
        if report.created { "yes" } else { "no" },
        if report.nul_granted { "granted" } else { "missing" }
    );
    for path in &report.granted {
        println!("granted read+execute: {path}");
    }
}

/// `sandbox status`: says whether the machine is ready, and is not ready when it
/// is not.
fn sandbox_status(args: &Args) {
    let status = agent_sandbox_win::status(agent_sandbox_win::PROFILE_NAME)
        .unwrap_or_else(|e| fail(args, &e.to_string()));
    if args.json {
        match agent_cli::json_output(&serde_json::json!({
            "profile": status.profile,
            "sid": status.sid,
            "nul_device": status.nul_granted,
        })) {
            Ok(text) => println!("{text}"),
            Err(e) => fail(args, &e.to_string()),
        }
    } else {
        println!(
            "execution sandbox: profile={} sid={} nul_device={}",
            status.profile,
            status.sid,
            if status.nul_granted { "granted" } else { "missing" }
        );
    }
    if !status.nul_granted {
        eprintln!(
            "the AppContainer setup is not in place: the null-device grant is missing. \
             Windows resets that device's security descriptor at every boot, so the grant \
             must be re-applied after a reboot: run `agent sandbox setup` from an elevated \
             prompt"
        );
        std::process::exit(EXIT_DID_NOT_RUN);
    }
}

/// The `--readonly` paths a setup run names, which must exist.
fn sandbox_readonly_paths(args: &Args) -> Vec<PathBuf> {
    let words = args.command_args();
    let mut paths = Vec::new();
    let mut index = 1; // after `setup`
    while let Some(word) = words.get(index) {
        if word == "--readonly" {
            let Some(value) = words.get(index + 1) else {
                fail(args, "--readonly needs a path");
            };
            paths.push(PathBuf::from(value));
            index += 2;
        } else if let Some(value) = word.strip_prefix("--readonly=") {
            if value.is_empty() {
                fail(args, "--readonly needs a path");
            }
            paths.push(PathBuf::from(value));
            index += 1;
        } else {
            fail(args, &format!("unexpected argument '{word}'"));
        }
    }
    paths
}

/// `run` and `ask`: one implementation, two names.
async fn run(args: &Args) {
    let Some(text) = args.command_args().first() else {
        eprintln!("Usage: agent run <text>");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    // Repair any dangling open state from a previous crash before running.
    if let Err(e) = runtime.recovery.recover(&args.session).await {
        eprintln!("recovery failed: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    }

    let outcome = watch(
        &runtime,
        args,
        from,
        run_ask(&runtime.scheduler, &args.session, text, agent::clock::now(), args.json),
    )
    .await;
    exit_on(outcome.verdict);
}

/// Settle the call a paused turn is waiting on, then drive the turn that
/// carries on from it.
///
/// Recovery runs first for the same reason `run` runs it: a crash in the middle
/// of a previous resume leaves a dispatched call with no outcome, and resume
/// must not read the older `approval_pending` behind it as a wait that is still
/// open. Recovery is idempotent, so running it here costs nothing.
async fn resume(args: &Args) {
    let text = args.joined_text();
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    if let Err(e) = runtime.recovery.recover(&args.session).await {
        eprintln!("recovery failed: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    }

    let outcome = watch(
        &runtime,
        args,
        from,
        run_resume(
            &runtime.resume,
            &runtime.scheduler,
            &args.session,
            &text,
            agent::clock::now(),
            args.json,
        ),
    )
    .await;
    exit_on(outcome.verdict);
}

/// Derive a new session from a closed turn boundary of this one.
///
/// No recovery pass first, unlike `run` and `resume`: recovery writes to the
/// log it repairs, and a fork must not change the session it reads from. What
/// the child inherits is the source exactly as it stands — including a tail
/// that needs closing, which the child closes in its own copy.
async fn fork(args: &Args) {
    let at = option(args, "--at");
    let reason = option(args, "--reason").unwrap_or_else(|| "user fork".to_string());
    let runtime = assemble(args).await;

    // The chain is the *child's*, and a new session's log is entirely this
    // command's doing, so it starts at zero rather than at the source's head.
    let from = args.events.then_some(EventSeq::ZERO);
    let produced = run_fork(&runtime.fork, &args.session, at.as_deref(), &reason, args.json)
        .await
        .map(|(output, child)| Produced {
            output,
            session: child.unwrap_or_else(|| args.session.clone()),
        });
    report(runtime.store.as_ref(), args, from, produced).await;
}

async fn status(args: &Args) {
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;
    let outcome =
        watch(&runtime, args, from, run_status(&runtime.scheduler, &args.session, args.json)).await;
    exit_on(outcome.verdict);
}

/// `why`: read the latest turn's reasoning chain back from the log.
///
/// A reader like `memory` and `context`: no model, no tool surface, no sandbox
/// is assembled, so it still answers when the configuration that ran the turn
/// has since broken. It writes nothing, so `--events` has nothing to stream.
async fn why(args: &Args) {
    // Help is answered before the log is opened: asking what `why` does should
    // work in a directory with no session at all.
    if args.command_args().iter().any(|argument| argument == "--help" || argument == "-h") {
        print_why_help();
        std::process::exit(0);
    }
    if args.events {
        eprintln!("why writes no events; --events has nothing to stream");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir()));
    let turn = option(args, "--turn");
    let depth = option(args, "--depth").map(|value| {
        value
            .parse::<usize>()
            .unwrap_or_else(|_| fail(args, &format!("--depth needs a whole number, not '{value}'")))
    });
    let outcome =
        match run_why(store.as_ref(), &args.session, turn.as_deref(), depth, args.json).await {
            Ok(outcome) => outcome,
            Err(error) => fail(args, &error),
        };
    print(&outcome.output);
    exit_on(outcome.verdict);
}

/// What `why` is and how to aim it.
fn print_why_help() {
    println!(
        "Usage: agent why [--turn <id>] [--depth <n>] [--session <id>] [--json]\n\n\
         Read a turn's reasoning chain back from the session log and render it\n\
         backwards: the newest end first, then the nodes it was derived from.\n\
         The chain is written by the kernel around the model's choices; it never\n\
         enters a model request, so reading it costs no tokens.\n\n\
         Options:\n  \
         --turn <id>   Explain this turn (default: the latest turn with a chain)\n  \
         --depth <n>   Levels of derivation to show (default: {depth}; 0 shows the ends)\n  \
         --json        One structured object on stdout (nodes, verdict, stop, check)\n\
         \nThe first lines name the turn's end and its verdict, if it wrote one:\n\
         a turn cut by the step budget, held for approval or cancelled has no answer.",
        depth = agent_cli::reason::DEFAULT_DEPTH,
    );
}

/// `web --help`: what starting the server would do, without starting it.
fn print_web_help() {
    println!(
        "Usage: agent web [--port <n>] [--provider <echo|openai>] [--model <name>]\n\n\
         Serve the web GUI on loopback. The URL is printed with its one-time\n\
         access token in the fragment; open it as printed, because a URL without\n\
         the token cannot talk to the API. The process stays in the foreground\n\
         until Ctrl-C.\n\n\
         Options:\n  \
         --port <n>                    Listen on this port (default: {port})\n  \
         --provider <echo|openai>      Model provider (default: AGENT_MODEL_PROVIDER, else echo)\n  \
         --model <name>                Ask the provider for this model\n\n\
         The GUI is a view over the same session log the CLI writes: turns it\n\
         starts land in that log, and a session the CLI ran reads back here.",
        port = DEFAULT_WEB_PORT,
    );
}

/// `steer`: queue text for whatever turn is in flight in this session.
///
/// No recovery pass: steering is an append to a session another process may be
/// driving right now, and repairing that session's tail would be writing to a
/// log this command is only here to talk to.
async fn steer(args: &Args) {
    let text = args.joined_text();
    if text.trim().is_empty() {
        eprintln!("Usage: agent steer \"<text>\"");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;
    let outcome =
        watch(&runtime, args, from, run_steer(&runtime.scheduler, &args.session, &text, args.json))
            .await;
    exit_on(outcome.verdict);
}

/// `interrupt`: ask for the run in flight to stop at its next boundary.
///
/// Same road as `steer` and the same reason for it — this appends to a session
/// another process may be driving right now, so there is no recovery pass. The
/// kernel decides whether there is anything to interrupt; refusing here would
/// be guessing at a lifecycle this process has not read.
async fn interrupt(args: &Args) {
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;
    let outcome =
        watch(&runtime, args, from, run_interrupt(&runtime.scheduler, &args.session, args.json))
            .await;
    exit_on(outcome.verdict);
}

/// `export`: write the session out as a readable document and a machine bundle.
async fn export(args: &Args) {
    let out = option(args, "--out");
    // A command-level flag, read the way `deny --only` is: `--redacted` is not
    // in the shared option parser because no other command has a use for it,
    // and teaching the parser about a flag exactly one command reads would make
    // every command's help a little less true.
    let redacted = args.command_args().iter().any(|a| a == "--redacted");
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;
    let outcome = watch(
        &runtime,
        args,
        from,
        run_export(runtime.store.as_ref(), &args.session, out.as_deref(), redacted, args.json),
    )
    .await;
    exit_on(outcome.verdict);
}

async fn approve(args: &Args) {
    let positional = positionals(args);
    let Some(approval_id) = positional.first() else {
        eprintln!("Usage: agent approve <approval_id> [once|turn|session|always]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let scope = positional.get(1).map(|s| s.as_str()).unwrap_or("once");
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    let outcome = watch(
        &runtime,
        args,
        from,
        run_approve(
            runtime.approvals.as_ref(),
            runtime.scheduler.inbox(),
            &args.session,
            approval_id,
            scope,
            args.json,
        ),
    )
    .await;
    exit_on(outcome.verdict);
}

async fn deny(args: &Args) {
    let positional = positionals(args);
    let Some(approval_id) = positional.first() else {
        eprintln!("Usage: agent deny <approval_id> [--only]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let only = args.command_args().iter().any(|a| a == "--only");
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    let outcome = watch(
        &runtime,
        args,
        from,
        run_deny(
            runtime.approvals.as_ref(),
            runtime.scheduler.inbox(),
            &args.session,
            approval_id,
            only,
            args.json,
        ),
    )
    .await;
    exit_on(outcome.verdict);
}

/// `tool`: run one tool through the same gateway a model's call goes through.
///
/// Its exit status is the command's whole alphabet. `0` when the call ran and
/// completed. `1` when it ran and came back failed: the result is the answer, it
/// is printed on stdout in whichever shape was asked for, and the status carries
/// the verdict — the one thing that cannot be put *in* the output, so a script
/// reads it without parsing. A call that could not be dispatched at all — an
/// unknown name, a policy denial, a path outside the workspace — is not an
/// answer but the absence of one: nothing on stdout, the reason on stderr, and
/// `2`, the same status every command returns when it did not run.
async fn tool(args: &Args) {
    let Some(tool_name) = args.command_args().first() else {
        eprintln!("Usage: agent tool <tool_name> [<json-args>]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let arguments: serde_json::Value = match args.command_args().get(1) {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("invalid JSON args: {e}");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
        },
        None => serde_json::json!({}),
    };
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    let outcome = watch(
        &runtime,
        args,
        from,
        run_tool(runtime.gateway.as_ref(), &args.session, tool_name, arguments, args.json),
    )
    .await;
    // The tool's result has been printed by now, on stdout and in whichever
    // shape was asked for. The exit status is the one thing that cannot be put
    // in the output — it is what a script reads without parsing it.
    exit_on(outcome.verdict);
}

/// `plugin`: the verbs that read or change the plugin surface.
///
/// None of them assembles a runtime, and that is the point rather than a
/// shortcut. `list` answers a question about a file, a log and a queue; `disable`
/// and `enable` append to that queue. None needs a model, a tool surface or a
/// started MCP server. Assembling one anyway would make all three fail whenever
/// the *model provider* is misconfigured — a failure with nothing to do with what
/// was asked — and would make `list` write `plugin/loaded` events into the very
/// log it describes.
///
/// The two changing verbs are mirrors of each other and neither performs its
/// change: the process driving the session owns the registration, so it is the
/// one that applies the request at a boundary. What makes that arrangement work
/// across processes is that a disabled plugin is *recorded* as disabled, which is
/// why a later run does not register it and why `enable` is a registration rather
/// than an erasure of some hidden flag.
async fn plugin(args: &Args) {
    match args.command_args().first().map(String::as_str) {
        Some("list") => plugin_list(args).await,
        Some("disable") => plugin_disable(args).await,
        Some("enable") => plugin_enable(args).await,
        Some(other) => {
            eprintln!("unknown plugin subcommand '{other}'");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        None => {
            eprintln!(
                "Usage: agent plugin list\n       \
                 agent plugin disable <plugin_id> [--reason <text>]\n       \
                 agent plugin enable <plugin_id>"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// `plugin list`: three durable sources, and a note about the one it cannot see.
///
/// The configuration is read here rather than by `assemble`, so that the listing
/// is the same answer whether or not anything could be started.
async fn plugin_list(args: &Args) {
    let root = workspace_root();
    let resolved = config::load(&root, args).unwrap_or_else(|e| {
        eprintln!("configuration: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });
    for note in &resolved.notes {
        eprintln!("configuration: {note}");
    }

    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir()));
    let from = chain_start(store.as_ref(), args).await;
    let enabled = plugins::configured_plugins(&resolved);
    report(
        store.as_ref(),
        args,
        from,
        plugins::list(&enabled, &store, &args.session, args.json)
            .await
            .map(|output| Produced { output, session: args.session.clone() }),
    )
    .await;
}

/// `plugin disable`: queue the change; do not make it.
///
/// The withdrawal belongs to whichever process is driving the session, because
/// only that process holds the registration. This one appends the request and
/// says so, which is also why the wording is "will be disabled" and not "was".
async fn plugin_disable(args: &Args) {
    // The first word is `disable`; the plugin id is the first bare word after
    // it. `--reason`'s value is skipped with the flag, so the flag may come
    // before the id: `plugin disable --reason because mcp.one` names `mcp.one`.
    let plugin_id = plugin_id_argument(args, "disable", "[--reason <text>]");
    let reason = option(args, "--reason");

    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir()));
    let from = chain_start(store.as_ref(), args).await;
    let inbox = Inbox::new(store.clone());
    report(
        store.as_ref(),
        args,
        from,
        plugins::disable(&inbox, &args.session, plugin_id, reason.as_deref(), args.json)
            .await
            .map(|output| Produced { output, session: args.session.clone() }),
    )
    .await;
}

/// `plugin enable`: queue the registration; do not make it.
///
/// The mirror of `disable`, and the reason a disable can be undone at all: what
/// keeps a plugin off in later runs is the log's last word about it, and a
/// registration is a later word. Nothing here reads the configuration either — a
/// name the configuration does not have is refused at the boundary, by the
/// process that can say what the configuration does have.
async fn plugin_enable(args: &Args) {
    let plugin_id = plugin_id_argument(args, "enable", "");
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir()));
    let from = chain_start(store.as_ref(), args).await;
    let inbox = Inbox::new(store.clone());
    report(
        store.as_ref(),
        args,
        from,
        plugins::enable(&inbox, &args.session, plugin_id, args.json)
            .await
            .map(|output| Produced { output, session: args.session.clone() }),
    )
    .await;
}

/// `session`: create, list, hide or delete a session.
///
/// None of the verbs assembles a runtime: a session exists because its log does,
/// and every question here is about that log and the directory that holds it. A
/// model provider that is misconfigured cannot stop a person from tidying up
/// their sessions.
async fn session(args: &Args) {
    match args.command_args().first().map(String::as_str) {
        Some("new") => session_new(args).await,
        Some("list") => session_list(args).await,
        Some("archive") => session_set_archived(args, true).await,
        Some("unarchive") => session_set_archived(args, false).await,
        Some("delete") => session_delete(args).await,
        Some(other) => {
            eprintln!("unknown session subcommand '{other}'");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        None => {
            eprintln!(
                "Usage: agent session new [--id <id>]\n       \
                 agent session list\n       \
                 agent session archive <session_id>\n       \
                 agent session unarchive <session_id>\n       \
                 agent session delete <session_id>"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// The store and data directory the `session` verbs work on.
fn session_store() -> (Arc<dyn EventStore>, String) {
    let data = data_dir();
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(&data));
    (store, data)
}

/// `session new [--id <id>]`: record that a session has started.
async fn session_new(args: &Args) {
    let (store, _data) = session_store();
    let id = option(args, "--id").map_or_else(SessionId::new, SessionId::from_string);
    // The chain starts where the session's log already ends — nothing, for an
    // id nobody has used. A name the store would refuse is left to the verb to
    // report: reading a stream cannot say what is wrong with it as clearly.
    let from = if agent_ports::event_store::is_safe_dir_name(id.as_str()) {
        chain_start_for(store.as_ref(), &id, args).await
    } else {
        None
    };
    let outcome = match agent::session::run_session_new(&store, &id, args.json).await {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    let verdict = outcome.verdict;
    report(store.as_ref(), args, from, Ok(Produced { output: outcome.output, session: id })).await;
    exit_on(verdict);
}

/// `session list`: every session in the data directory, newest activity first.
///
/// Nothing is written, so there is no chain: with `--events` the listing is the
/// answer and goes to stderr, where every other command's answer goes when the
/// chain owns stdout.
async fn session_list(args: &Args) {
    let (store, data) = session_store();
    let outcome = match agent::session::run_session_list(&store, Path::new(&data), args.json).await
    {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    let verdict = outcome.verdict;
    answer(args, &outcome.output);
    exit_on(verdict);
}

/// `session archive` / `session unarchive`: hide a session or show it again.
async fn session_set_archived(args: &Args, archived: bool) {
    let verb = if archived { "archive" } else { "unarchive" };
    let id = session_id_argument(args, verb);
    let (store, data) = session_store();
    // Only a session that exists has a chain to start at, and asking the store
    // about one that does not would create the very directory the verb is about
    // to say does not exist.
    let from = if agent_store_jsonl::has_session(Path::new(&data), id.as_str()) {
        chain_start_for(store.as_ref(), &id, args).await
    } else {
        None
    };
    let outcome = match agent::session::run_session_archive(
        &store,
        Path::new(&data),
        &id,
        archived,
        args.json,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    let verdict = outcome.verdict;
    report(
        store.as_ref(),
        args,
        from,
        Ok(Produced { output: outcome.output, session: id.clone() }),
    )
    .await;
    exit_on(verdict);
}

/// `session delete`: remove the log, refusing while a turn is running there.
///
/// A deleted log has no events left to show, so `--events` yields no chain and
/// the answer moves to stderr — the same split as any other command.
async fn session_delete(args: &Args) {
    let id = session_id_argument(args, "delete");
    let (store, _data) = session_store();
    let outcome = match agent::session::run_session_delete(&store, &id, args.json).await {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    let verdict = outcome.verdict;
    answer(args, &outcome.output);
    exit_on(verdict);
}

/// The session id an `<verb> <session_id>` command names, or a usage line and
/// no process.
///
/// The first word of the arguments is the verb; the id is the first word after
/// it that is not a flag, so `session archive --id s1` names `s1` just as well
/// as `session archive s1` does.
fn session_id_argument(args: &Args, verb: &str) -> SessionId {
    let positional = positionals(args);
    let Some(id) = positional.get(1) else {
        eprintln!("Usage: agent session {verb} <session_id>");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    SessionId::from_string(id.as_str())
}

/// `schedule`: arrange, list, or cancel turns that run at a time.
///
/// None of the three assembles a runtime, for the same reason the `plugin`
/// verbs do not: what they touch is a log and a queue, and assembling a model,
/// a tool surface and a plugin registry would make scheduling fail whenever the
/// model provider is misconfigured — a failure with nothing to do with the
/// question asked.
async fn schedule(args: &Args) {
    match args.command_args().first().map(String::as_str) {
        Some("at") => schedule_at(args).await,
        Some("list") => schedule_list(args).await,
        Some("cancel") => schedule_cancel(args).await,
        Some(other) => {
            eprintln!("unknown schedule subcommand '{other}'");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        None => {
            eprintln!(
                "Usage: agent schedule at <when> <text>\n       \
                 agent schedule list\n       \
                 agent schedule cancel <item_id>"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// The store and queue the `schedule` verbs work on.
fn schedule_queue() -> (Arc<dyn EventStore>, Inbox) {
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir()));
    (store.clone(), Inbox::new(store))
}

/// `schedule at`: append one turn with the time it should run.
///
/// The time is read here, at the app edge, and passed in — the kernel compares
/// `due_at` against a `now` it is given, and never reads a clock itself.
async fn schedule_at(args: &Args) {
    let positional = positionals(args);
    let Some(when) = positional.get(1) else {
        eprintln!("Usage: agent schedule at <when> <text>");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let text = positional.iter().skip(2).map(|word| word.as_str()).collect::<Vec<_>>().join(" ");
    if text.trim().is_empty() {
        eprintln!("Usage: agent schedule at <when> <text>");
        std::process::exit(EXIT_DID_NOT_RUN);
    }

    let (store, inbox) = schedule_queue();
    let from = chain_start(store.as_ref(), args).await;
    let outcome =
        match run_schedule_at(&inbox, &args.session, when, &text, agent::clock::now(), args.json)
            .await
        {
            Ok(outcome) => outcome,
            Err(e) => fail(args, &e),
        };
    let verdict = outcome.verdict;
    report(
        store.as_ref(),
        args,
        from,
        Ok(Produced { output: outcome.output, session: args.session.clone() }),
    )
    .await;
    exit_on(verdict);
}

/// `schedule list`: the scheduled turns that have not run.
async fn schedule_list(args: &Args) {
    let (store, inbox) = schedule_queue();
    let from = chain_start(store.as_ref(), args).await;
    let outcome =
        match run_schedule_list(&inbox, &args.session, agent::clock::now(), args.json).await {
            Ok(outcome) => outcome,
            Err(e) => fail(args, &e),
        };
    let verdict = outcome.verdict;
    report(
        store.as_ref(),
        args,
        from,
        Ok(Produced { output: outcome.output, session: args.session.clone() }),
    )
    .await;
    exit_on(verdict);
}

/// `schedule cancel`: withdraw a scheduled turn before it runs.
async fn schedule_cancel(args: &Args) {
    let positional = positionals(args);
    let Some(item_id) = positional.get(1) else {
        eprintln!("Usage: agent schedule cancel <item_id>");
        std::process::exit(EXIT_DID_NOT_RUN);
    };

    let (store, inbox) = schedule_queue();
    let from = chain_start(store.as_ref(), args).await;
    let outcome = match run_schedule_cancel(&inbox, &args.session, item_id, args.json).await {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    let verdict = outcome.verdict;
    report(
        store.as_ref(),
        args,
        from,
        Ok(Produced { output: outcome.output, session: args.session.clone() }),
    )
    .await;
    exit_on(verdict);
}

/// `tick`: drive at most one turn — an explicit `next_turn` first, then the
/// earliest due scheduled item.
///
/// `--events` is refused because a tick can run a whole turn: the chain is not
/// unbounded, but watching it would mean holding a subscription for a command
/// whose whole purpose is to be called from a scheduler. Read the log instead.
async fn tick(args: &Args) {
    if args.events {
        eprintln!("tick does not support --events; read the session log instead");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let runtime = assemble(args).await;
    // A tick may be the first process to look at a session after a crash, so
    // repair dangling state before deciding what to run.
    if let Err(e) = runtime.recovery.recover(&args.session).await {
        eprintln!("recovery failed: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    }

    let outcome = match run_tick(
        &runtime.scheduler,
        runtime.approvals.as_ref(),
        &args.session,
        agent::clock::now(),
        args.json,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    print(&outcome.output);
    exit_on(outcome.verdict);
}

/// The models a session may be switched to from the page.
///
/// Offered rather than hardcoded by the page: a provider is listed only when
/// this process could actually build it, so the composer never shows a choice
/// whose only possible outcome is an error. The echo stub is always available;
/// the HTTP adapter is listed when its endpoint is configured.
fn model_options(resolved_model: Option<&str>) -> Vec<agent_web::ModelOption> {
    let mut options = vec![agent_web::ModelOption {
        provider: "echo".to_string(),
        model: None,
        label: "echo".to_string(),
    }];
    if let Ok(config) = agent_model_openai::OpenAiCompatConfig::from_env() {
        // The configuration's model wins over the environment's for the same
        // reason it does in `providers::select_kind`: a run must have exactly
        // one answer to "which model".
        let model = resolved_model.map(str::to_string).unwrap_or(config.model);
        options.push(agent_web::ModelOption {
            provider: "openai".to_string(),
            label: model.clone(),
            model: Some(model),
        });
    }
    options
}

/// `web`: serve the GUI on loopback.
///
/// The server binds `127.0.0.1` and prints its URL with a one-time token in the
/// fragment. Nothing about the run is decided here: the turn endpoints will call
/// the same command layer `ask` does, so the browser cannot reach a state the
/// terminal could not.
///
/// An invalid `AGENT_SANDBOX` refuses to start, exactly as `ask` does: the page
/// accepts turns, and a boundary that cannot be built must be said before the
/// first click rather than only in `/api/meta` afterward.
async fn web(args: &Args) {
    // Asking what `web` does must not start a server; `why --help` answers
    // before it opens the log for the same reason.
    if args.command_args().iter().any(|argument| argument == "--help" || argument == "-h") {
        print_web_help();
        std::process::exit(0);
    }

    let port = match option(args, "--port") {
        Some(value) => value
            .parse::<u16>()
            .unwrap_or_else(|_| fail(args, &format!("--port needs a port number, not '{value}'"))),
        None => DEFAULT_WEB_PORT,
    };

    let root = workspace_root();
    let workspace = Path::new(&root)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace")
        .to_string();

    // Resolve the configuration before the page is served. The page itself runs
    // nothing, but every turn it accepts will run under this profile, and a
    // configuration that does not resolve is better said now than on the first
    // click. It is also what `/api/meta` reports, so the page cannot claim a
    // model or sandbox the first turn would not use.
    let resolved = config::load(&root, args).unwrap_or_else(|e| {
        eprintln!("configuration: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });
    for note in &resolved.notes {
        eprintln!("configuration: {note}");
    }
    let sandbox_mode = sandbox::from_environment().unwrap_or_else(|e| {
        eprintln!("execution sandbox: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });
    let network_allowed = sandbox::network_from_environment().unwrap_or_else(|e| {
        eprintln!("execution sandbox: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });

    // `/api/meta` must describe what the first turn will actually use, not what
    // the environment happens to carry. The echo provider has no model of its
    // own, so a stale `AGENT_MODEL_NAME` (usually set for the OpenAI-compatible
    // endpoint) must not leak into the label and claim a model nothing runs.
    //
    // `--provider` wins over the environment: one server can run the echo stub
    // while another talks to a real endpoint, without either changing what a
    // third one sees. The provider is built once here so a bad name or a
    // missing credential stops the server before it prints a URL the page
    // cannot honour.
    let provider = match option(args, "--provider") {
        Some(named) => named,
        None => agent::providers::env_provider(),
    };
    if let Err(error) = agent::providers::select_kind(&provider, resolved.model.as_deref()) {
        eprintln!("model provider: {error}");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let model_label = if provider == "echo" {
        "echo".to_string()
    } else {
        resolved
            .model
            .clone()
            .or_else(|| std::env::var(agent_model_openai::ENV_MODEL_NAME).ok())
            .unwrap_or_else(|| provider.clone())
    };

    let meta = agent_web::Meta {
        workspace,
        model: model_label,
        sandbox: sandbox::label(sandbox_mode).to_string(),
        judgment: std::env::var("AGENT_JUDGMENT").unwrap_or_else(|_| "gates".to_string()),
        budget: resolved.budget.max_input_tokens,
        capabilities: [
            "meta",
            "sessions",
            "transcript",
            "why",
            "diff",
            "context",
            "plan",
            "subagents",
            "schedule",
            "export",
            "artifact",
            "events",
            "turn",
            "approval",
            "resume",
            "fork",
            "steer",
            "interrupt",
            "model",
        ]
        .iter()
        .map(|name| name.to_string())
        .collect(),
        models: model_options(resolved.model.as_deref()),
    };

    let data_dir = PathBuf::from(data_dir());
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(&data_dir));
    let sessions = Arc::new(agent::web_runtime::WebBackend::new(agent::web_runtime::WebInputs {
        workspace_root: PathBuf::from(&root),
        data_dir: data_dir.clone(),
        resolved,
        sandbox: sandbox_mode,
        network_allowed,
        judgment: true,
        provider,
        store: store.clone(),
    }));
    sessions.start_reaper();
    let backend: Arc<dyn agent_web::Backend> = sessions;
    let config = agent_web::Config {
        port,
        meta,
        store,
        data_dir,
        backend: Some(backend),
        now: agent::clock::now,
    };

    let server = match agent_web::Server::bind(config).await {
        Ok(server) => server,
        Err(error) => fail(args, &error.to_string()),
    };
    println!("爻爻 · web  {}", server.url());
    println!("按 Ctrl-C 停止");
    if let Err(error) = server.serve().await {
        fail(args, &error.to_string());
    }
}

/// `serve`: keep running turns as their time arrives.
///
/// One process per session. Within one loop the runs are serialized by
/// construction; two loops would be kept from interleaving appends by the
/// store's lease, but nothing would stop them choosing the same item, so the
/// help says "one per session" rather than pretending the log enforces it.
///
/// The loop is quiet on purpose: it prints when a turn ran, and when it first
/// enters the "held at a decision" state — not every poll, or a session left
/// waiting overnight would fill the terminal with the same sentence.
async fn serve(args: &Args) {
    if args.events {
        eprintln!("serve does not support --events; read the session log instead");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let runtime = assemble(args).await;
    // A long-lived process is the likeliest first reader of a session after a
    // crash, so repair dangling state once before the loop starts. Per-tick
    // failures are transient and handled below.
    if let Err(e) = runtime.recovery.recover(&args.session).await {
        eprintln!("recovery failed: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    eprintln!("serving {} (one serve per session; Ctrl-C to stop)", args.session);

    let mut waiting_on: Option<TurnId> = None;
    loop {
        let now = agent::clock::now();
        // A failed tick sleeps a full poll interval below: retrying a store
        // error at zero delay would turn a transient fault into a hot loop.
        let mut failed = false;
        match tick_once(&runtime.scheduler, runtime.approvals.as_ref(), &args.session, now).await {
            Ok(Tick::Ran(report)) => {
                waiting_on = None;
                match render_ran(report, args.json) {
                    Ok(outcome) => print(&outcome.output),
                    Err(e) => eprintln!("tick failed: {e}"),
                }
            }
            Ok(Tick::AwaitingDecision { turn_id }) => {
                // Announced on entering, not on every poll: the decision is a
                // person's to make, and polling does not change the answer.
                if waiting_on.as_ref() != Some(&turn_id) {
                    let reason =
                        format!("turn {turn_id} is waiting on a decision; settle it with `resume`");
                    match render_did_not_run(&reason, args.json) {
                        Ok(outcome) => print(&outcome.output),
                        Err(e) => eprintln!("tick failed: {e}"),
                    }
                    waiting_on = Some(turn_id);
                }
            }
            Ok(Tick::Idle) => waiting_on = None,
            Err(e) => {
                eprintln!("tick failed: {e}");
                failed = true;
            }
        }

        let wait = if waiting_on.is_some() || failed {
            // A held session keeps its due item pending; sleeping until that
            // due time would be a zero-length sleep and a spin.
            SERVE_POLL
        } else {
            let earliest = match earliest_due(runtime.scheduler.inbox(), &args.session).await {
                Ok(earliest) => earliest,
                Err(e) => {
                    eprintln!("cannot read the schedule: {e}");
                    None
                }
            };
            // The clock is read again after the tick: a turn can outlast the
            // wait that was computed before it, and the item it was waiting for
            // may already be due.
            next_wait(earliest, agent::clock::now(), SERVE_POLL)
        };
        let wait = wait.to_std().unwrap_or(Duration::ZERO);

        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = tokio::signal::ctrl_c() => {
                print_stop(args.json);
                return;
            }
        }
    }
}

/// The line `serve` ends on.
fn print_stop(json: bool) {
    if json {
        match agent_cli::json_output(&serde_json::json!({ "stopped": true })) {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!("could not render the stop line: {e}"),
        }
    } else {
        println!("stopped");
    }
}

/// `memory search` and `memory reindex`: the derived index over the log.
///
/// Neither assembles the run. Reading a session's memory has to work when the
/// model configuration does not, for the same reason `schedule` and `plugin`
/// do, and neither command starts a turn. `--events` is refused because nothing
/// here appends: a search that wrote would make the model's history depend on
/// who looked.
async fn memory(args: &Args) {
    let words: Vec<&String> =
        args.command_args().iter().filter(|word| !word.starts_with("--")).collect();
    match words.first().map(|word| word.as_str()) {
        Some("search") => {
            let query = words[1..].iter().map(|word| word.as_str()).collect::<Vec<_>>().join(" ");
            if query.trim().is_empty() {
                eprintln!("Usage: agent memory search <text>");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
            if args.events {
                eprintln!("memory search does not support --events; it writes no events");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
            let index = memory_index();
            let outcome = match run_memory_search(
                &index,
                &args.session,
                &query,
                DEFAULT_SEARCH_LIMIT,
                args.json,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(e) => fail(args, &e),
            };
            print(&outcome.output);
            exit_on(outcome.verdict);
        }
        Some("reindex") => {
            if args.events {
                eprintln!("memory reindex does not support --events; it writes no events");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
            let index = memory_index();
            let outcome = match run_memory_reindex(&index, &args.session, args.json).await {
                Ok(outcome) => outcome,
                Err(e) => fail(args, &e),
            };
            print(&outcome.output);
            exit_on(outcome.verdict);
        }
        Some(other) => {
            eprintln!("unknown memory subcommand '{other}'");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        None => {
            eprintln!(
                "Usage: agent memory search <text>\n       \
                 agent memory reindex"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// The index the `memory` verbs read: under the same data directory as the log.
fn memory_index() -> SqliteMemoryIndex {
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir()));
    SqliteMemoryIndex::open(store, data_dir())
}

/// `context inspect`: what the next request would carry, measured but not sent.
///
/// A reader, like `memory` and `artifact get`: no model, no tool surface, no
/// plugin registry is assembled, so it still answers when the provider or the
/// sandbox is misconfigured. It does read the configuration, because the window
/// the answer is measured against lives there — and a window nobody can read
/// would make the budget line a guess, so that is a failed run (`2`) rather
/// than an answer.
///
/// Reading the log waits for no writer. The command only looks, and waiting
/// behind a long turn would make "what does the context look like" hang for
/// exactly as long as the thing it is inspecting.
async fn context(args: &Args) {
    let positional = positionals(args);
    let Some(verb) = positional.first() else {
        eprintln!("Usage: agent context inspect [--top <n>] [--json]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    if verb.as_str() != "inspect" {
        eprintln!("unknown context subcommand '{verb}'");
        usage();
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    if args.events {
        eprintln!("context inspect does not support --events; it writes no events");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let top = match option(args, "--top") {
        None => agent_cli::context::DEFAULT_TOP_MESSAGES,
        Some(text) => match text.parse::<usize>() {
            Ok(top) if top > 0 => top,
            _ => {
                eprintln!("--top needs a positive number of messages, got '{text}'");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
        },
    };

    let resolved = config::load(&workspace_root(), args).unwrap_or_else(|e| {
        eprintln!("configuration: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });
    for note in &resolved.notes {
        eprintln!("configuration: {note}");
    }

    // The same policy `stack::assemble` builds from the same configuration: a
    // report measured against a default window would describe a request this
    // build would never send.
    let data_dir = PathBuf::from(data_dir());
    let store: Arc<dyn EventStore> =
        Arc::new(JsonlEventStore::new(&data_dir).with_lease_wait(Duration::ZERO));
    let engine = agent_context::ProjectionContextEngine::new(store.clone(), &data_dir);
    let engine = match resolved.budget.max_input_tokens {
        Some(max_input_tokens) => engine.with_context_policy(agent_context::trim::ContextPolicy {
            max_input_tokens,
            reserved_output_tokens: resolved.budget.reserved_output_tokens.unwrap_or(0),
            compact_at_percent: resolved.budget.compact_at_percent,
        }),
        None => engine,
    };

    let derived = match engine.derive(&args.session).await {
        Ok(derived) => derived,
        Err(e) => {
            eprintln!("context derive failed: {e}");
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    };
    let report = agent_context::inspect::report(&derived.messages, &derived.usage, top);

    // The final section is read back from the log rather than recomputed: what
    // the last request actually carried is a recorded fact, and a prediction
    // would answer a different question.
    let events = match store.read_stream(&args.session, EventSeq::ZERO, usize::MAX).await {
        Ok(events) => events,
        Err(e) => {
            eprintln!("context log read failed: {e}");
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    };
    let outcome =
        match run_context_inspect(&report, &args.session, derived.revision, &events, args.json) {
            Ok(outcome) => outcome,
            Err(e) => fail(args, &e),
        };
    print(&outcome.output);
    exit_on(outcome.verdict);
}

/// `metrics`: count what the event logs say happened.
///
/// A reader like `context inspect`: no model, no tool surface and no runtime is
/// assembled, so it answers when the provider or the sandbox is misconfigured.
/// The store takes a zero lease wait for the same reason `doctor` does — a
/// session another process is driving still has a readable prefix, and waiting
/// behind its open lease would make "how is this going?" hang behind the thing
/// being asked about.
///
/// `--events` is refused because nothing here appends, and `--out` writes only
/// derived data: the report is a function of the logs, and the same logs always
/// produce the same bytes.
async fn metrics(args: &Args) {
    if args.events {
        eprintln!("metrics does not support --events; it writes no events");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let out = option(args, "--out");
    let data_dir = data_dir();
    let store: Arc<dyn EventStore> =
        Arc::new(JsonlEventStore::new(&data_dir).with_lease_wait(Duration::ZERO));
    // Only a typed `--session` narrows the scope; the parser's default session
    // is a real session, so its value alone cannot tell "all" from "default".
    let only = args.session_given.then(|| args.session.clone());
    let outcome = match agent::metrics::run_metrics(
        &store,
        Path::new(&data_dir),
        only.as_ref(),
        out.as_deref(),
        args.json,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    let verdict = outcome.verdict;
    answer(args, &outcome.output);
    exit_on(verdict);
}

/// `artifact get`: fetch the bytes behind a locator the log handed out.
///
/// Neither assembles the run nor reads the configuration: stored output has to
/// be retrievable when the model or the sandbox is misconfigured, for the same
/// reason `memory` does not assemble one. The store is built from the same
/// `<data_dir>/artifacts` root the run writes to, and nothing here appends.
async fn artifact(args: &Args) {
    let positional = positionals(args);
    let Some(verb) = positional.first() else {
        eprintln!("Usage: agent artifact get <locator|hash> [--out <path>]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    if verb.as_str() != "get" {
        eprintln!("unknown artifact subcommand '{verb}'");
        usage();
        std::process::exit(EXIT_DID_NOT_RUN);
    }
    let Some(reference) = positional.get(1) else {
        eprintln!("Usage: agent artifact get <locator|hash> [--out <path>]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    if args.events {
        eprintln!("artifact get does not support --events; it writes no events");
        std::process::exit(EXIT_DID_NOT_RUN);
    }

    let out = option(args, "--out");
    let store = FsArtifactStore::new(PathBuf::from(data_dir()).join("artifacts"));
    let outcome = match run_artifact_get(&store, reference, out.as_deref(), args.json) {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    print(&outcome.output);
    exit_on(outcome.verdict);
}

/// The plugin id both changing verbs take, or a usage line and no process.
///
/// The first word of the arguments is the verb; the id is the first word after
/// it that is not itself a flag or a flag's value. The value is skipped with
/// its flag, so `plugin disable --reason because mcp.one` reads `mcp.one`.
fn plugin_id_argument<'a>(args: &'a Args, verb: &str, extra: &str) -> &'a str {
    let positional = positionals(args);
    let Some(plugin_id) = positional.get(1) else {
        let extra = if extra.is_empty() { String::new() } else { format!(" {extra}") };
        eprintln!("Usage: agent plugin {verb} <plugin_id>{extra}");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    plugin_id.as_str()
}

/// The command's positional arguments, with each flag and its value skipped.
///
/// A flag's *value* is not a positional. Dropping only the flag itself used to
/// make `plugin disable --reason because mcp.one` read `because` as the plugin
/// id, and the same filter serves `approve` and `deny` (YA-76). The
/// value-taking flags are a closed set shared by every command — a flag does
/// not change its arity from one verb to the next — so this reader knows them
/// without being told which command it serves.
///
/// Both spellings are handled: `--flag value` consumes the next word, and
/// `--flag=value` is a single word whose value is already inside it. Boolean
/// flags (`--only`) and flags this binary does not define are dropped alone:
/// there is no declared value to skip, and guessing that the next word belongs
/// to an unknown flag would silently eat a positional.
fn positionals(args: &Args) -> Vec<&String> {
    const VALUE_FLAGS: &[&str] = &["--at", "--reason", "--out"];
    let mut out = Vec::new();
    let mut tokens = args.command_args().iter();
    while let Some(token) = tokens.next() {
        if token.starts_with("--") {
            if VALUE_FLAGS.contains(&token.as_str()) {
                tokens.next();
            }
            continue;
        }
        out.push(token);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_cli::CliOutput;
    use agent_kernel::live_bus::InMemoryLiveEventBus;
    use agent_ports::live_event_bus::LiveEventBus;
    use agent_protocol::events::{Actor, EventDraft, EventType, LiveEvent};
    use agent_protocol::ids::{EventId, RunId, StepId, TurnId};
    use agent_protocol::model::ModelStreamEvent;
    use agent_testkit::FakeEventStore;
    use std::time::Duration;

    /// The alphabet is three statuses, and no two verdicts share one.
    ///
    /// A caller that reads only `$?` has to be able to name what happened from
    /// the number alone, so the numbers may not collide — and a fourth verdict
    /// added to the enum without a status here would be a verdict that exits
    /// like one of the three.
    #[test]
    fn the_verdict_alphabet_is_three_distinct_statuses() {
        let statuses =
            [Verdict::Answered, Verdict::AnsweredNo, Verdict::DidNotRun].map(exit_status);

        assert_eq!(statuses, [0, EXIT_ANSWERED_NO, EXIT_DID_NOT_RUN]);
        assert_eq!(
            statuses.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3,
            "two verdicts share a status: {statuses:?}"
        );
    }

    /// The data directory follows the workspace root, not the process.
    ///
    /// `AGENT_DATA_DIR` is the one path a run cannot do without when nobody
    /// named one, and where "nowhere named" points has to be decided by the
    /// rule every other relative path follows. Otherwise the event log of a run
    /// against a workspace goes to whatever directory the command was invoked
    /// from: a different place per invocation, and none of them the workspace.
    #[test]
    fn an_unnamed_data_directory_is_resolved_inside_the_workspace() {
        let root = if cfg!(windows) { "C:\\ws" } else { "/ws" };

        // Unset and empty are the same request: a name inside the workspace.
        for configured in [None, Some("")] {
            let resolved = resolve_data_dir(root, configured);
            assert_eq!(resolved, Path::new(root).join(DEFAULT_DATA_DIR).display().to_string());
            assert!(
                Path::new(&resolved).starts_with(root),
                "{resolved:?} is outside the workspace"
            );
        }

        // A relative value is relative to the workspace root, which is the whole
        // point — it is not relative to whatever cwd the process has.
        assert_eq!(
            resolve_data_dir(root, Some("state")),
            Path::new(root).join("state").display().to_string()
        );
        assert_eq!(
            resolve_data_dir(root, Some(Path::new("nested").join("state").to_str().unwrap())),
            Path::new(root).join("nested").join("state").display().to_string()
        );

        // An absolute value is already unambiguous and is taken as given.
        let absolute = if cfg!(windows) { "C:\\agent-state" } else { "/var/agent-state" };
        assert_eq!(resolve_data_dir(root, Some(absolute)), absolute);
    }

    /// A run's base is the configured root, or the cwd — and never a panic.
    ///
    /// `AGENT_WORKSPACE_DIR` decides the base when it is set. With none named,
    /// the current directory is the answer; and when even that cannot be read,
    /// the command has no workspace to root a run in and says so (YA-30). The
    /// failure is a message and an exit status, not a panic: a `current_dir()`
    /// that returns `Err` used to be `.unwrap()`ed, which took the whole process
    /// down with a backtrace a CLI caller can neither read nor catch.
    #[test]
    fn a_workspace_root_is_the_configured_dir_or_the_cwd_and_never_a_panic() {
        let configured = if cfg!(windows) { "C:\\ws" } else { "/ws" };
        // A configured root is taken as given, readable cwd or not.
        assert_eq!(
            resolve_workspace_root(Some(configured), Err(no_current_dir())).unwrap(),
            configured
        );

        // With none named, the current directory is the base.
        let cwd = PathBuf::from(if cfg!(windows) { "C:\\cwd" } else { "/cwd" });
        assert_eq!(
            resolve_workspace_root(None, Ok(cwd.clone())).unwrap(),
            cwd.display().to_string()
        );

        // With none named and no readable cwd, the reason is carried out — the
        // caller prints it and stops, rather than a panic the caller never sees.
        let error = resolve_workspace_root(None, Err(no_current_dir())).unwrap_err();
        assert!(error.to_string().contains("the directory is gone"), "reason lost: {error}");
    }

    fn no_current_dir() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::NotFound, "the directory is gone")
    }

    /// Records what a terminal would have been shown, in order.
    #[derive(Default)]
    struct Recording {
        lines: Vec<String>,
    }
    struct RecordingSink(Arc<std::sync::Mutex<Recording>>);

    impl RecordingSink {
        fn record(&self, line: String) {
            self.0.lock().unwrap().lines.push(line);
        }
    }

    impl WatchSink for RecordingSink {
        fn chain(&mut self, line: &str) {
            self.record(format!("chain|{line}"));
        }

        fn live(&mut self, text: &str) {
            self.record(format!("live|{text}"));
        }

        fn answer(&mut self, output: &CliOutput) {
            let text = match output {
                CliOutput::Text(text) => text.clone(),
                CliOutput::Json(text) => text.as_str().to_string(),
            };
            self.record(format!("answer|{text}"));
        }

        fn note(&mut self, message: &str) {
            self.record(format!("note|{message}"));
        }
    }

    fn draft(session_id: &str, event_type: &str) -> EventDraft {
        EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: agent_protocol::ids::SchemaVersion::from_string("0.1.0"),
            session_id: SessionId::from_string(session_id),
            run_id: Some(RunId::new()),
            turn_id: Some(TurnId::new()),
            step_id: Some(StepId::new()),
            actor: Actor::System,
            created_at: chrono::Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::json!({}),
            metadata: serde_json::json!({}),
            ignorable: false,
        }
    }

    fn text_delta(text: &str) -> LiveEvent {
        LiveEvent {
            live_seq: 1,
            event_type: EventType::new(EventType::MODEL_STREAM_DELTA),
            session_id: SessionId::from_string("watched"),
            created_at: chrono::Utc::now(),
            payload: serde_json::to_value(ModelStreamEvent::TextDelta { text: text.to_string() })
                .unwrap(),
            metadata: serde_json::json!({}),
        }
    }

    /// Wait until the recorder has something, or give up.
    async fn wait_for(recorded: &Arc<std::sync::Mutex<Recording>>, want: usize) -> bool {
        for _ in 0..400 {
            if recorded.lock().unwrap().lines.len() >= want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    /// The claim E2 exists to make: a line a person can read appears while the
    /// command is still working, not after it returns.
    #[tokio::test]
    async fn the_chain_appears_before_the_command_finishes() {
        let store = Arc::new(FakeEventStore::new());
        let bus = InMemoryLiveEventBus::new(8);
        let sid = SessionId::from_string("watched");
        let recorded = Arc::new(std::sync::Mutex::new(Recording::default()));
        let mut sink = RecordingSink(recorded.clone());
        let mut live = bus.subscribe().await.unwrap();

        let writing = store.clone();
        let watched = sid.clone();
        let observed = recorded.clone();
        let command = async move {
            writing
                .append(&watched, EventSeq::ZERO, vec![draft("watched", EventType::TURN_STARTED)])
                .await
                .unwrap();
            let appeared = wait_for(&observed, 1).await;
            // Asserted from inside the command: this future has not returned, so
            // if the line is here, it was printed while the turn was running.
            assert!(appeared, "the chain line did not appear before the command finished");
            Ok(CliOutput::Text("done".to_string()))
        };

        // The verdict is not what these tests are about: the sink is. It is
        // dropped on purpose, not by omission.
        let _ = watch_with(store.as_ref(), live.as_mut(), &sid, EventSeq::ZERO, command, &mut sink)
            .await
            .unwrap();

        let lines = recorded.lock().unwrap().lines.clone();
        assert_eq!(lines.len(), 2, "one chain line and the answer: {lines:?}");
        assert!(lines[0].starts_with("chain|"), "{}", lines[0]);
        assert_eq!(lines[1], "answer|done");
    }

    /// Live model text goes to the human half; the chain stays a machine's.
    #[tokio::test]
    async fn live_text_reaches_the_human_half_and_never_the_chain() {
        let store = Arc::new(FakeEventStore::new());
        let bus = Arc::new(InMemoryLiveEventBus::new(8));
        let sid = SessionId::from_string("watched");
        let recorded = Arc::new(std::sync::Mutex::new(Recording::default()));
        let mut sink = RecordingSink(recorded.clone());
        let mut live = bus.subscribe().await.unwrap();

        let publishing = bus.clone();
        let observed = recorded.clone();
        let command = async move {
            publishing.publish(text_delta("hi")).await.unwrap();
            assert!(wait_for(&observed, 1).await, "the live text did not appear while running");
            Ok(CliOutput::Text("done".to_string()))
        };

        // The verdict is not what these tests are about: the sink is. It is
        // dropped on purpose, not by omission.
        let _ = watch_with(store.as_ref(), live.as_mut(), &sid, EventSeq::ZERO, command, &mut sink)
            .await
            .unwrap();

        let lines = recorded.lock().unwrap().lines.clone();
        assert_eq!(lines[0], "live|[model] hi");
        assert_eq!(lines[1], "live|\n", "the open line is closed before the answer");
        assert_eq!(lines[2], "answer|done");
        assert!(
            !lines.iter().any(|line| line.starts_with("chain|")),
            "a live delta is not a durable fact: {lines:?}"
        );
    }

    /// A command that returns in the same poll that publishes its delta still
    /// shows that delta.
    ///
    /// This is the shape a fast provider has — the local echo model finishes
    /// before the select ever reaches its live branch — and without the drain
    /// the preview is thrown away with the queue, so the feature looks inert
    /// against every model quick enough not to need it.
    #[tokio::test]
    async fn a_command_that_finishes_at_once_still_shows_its_delta() {
        let store = Arc::new(FakeEventStore::new());
        let bus = Arc::new(InMemoryLiveEventBus::new(8));
        let sid = SessionId::from_string("watched");
        let recorded = Arc::new(std::sync::Mutex::new(Recording::default()));
        let mut sink = RecordingSink(recorded.clone());
        let mut live = bus.subscribe().await.unwrap();

        let publishing = bus.clone();
        let command = async move {
            publishing.publish(text_delta("quick")).await.unwrap();
            Ok(CliOutput::Text("quick".to_string()))
        };

        // The verdict is not what these tests are about: the sink is. It is
        // dropped on purpose, not by omission.
        let _ = watch_with(store.as_ref(), live.as_mut(), &sid, EventSeq::ZERO, command, &mut sink)
            .await
            .unwrap();

        let lines = recorded.lock().unwrap().lines.clone();
        assert_eq!(lines[0], "live|[model] quick", "{lines:?}");
        assert_eq!(lines[1], "live|\n");
        assert_eq!(lines[2], "answer|quick");
    }

    /// A command that appends and returns immediately still gets its whole chain
    /// printed: the final read is what decides which lines exist, so the live
    /// pass is early, never different.
    #[tokio::test]
    async fn the_final_read_covers_what_the_live_pass_missed() {
        let store = Arc::new(FakeEventStore::new());
        let bus = InMemoryLiveEventBus::new(8);
        let sid = SessionId::from_string("watched");
        let recorded = Arc::new(std::sync::Mutex::new(Recording::default()));
        let mut sink = RecordingSink(recorded.clone());
        let mut live = bus.subscribe().await.unwrap();

        let writing = store.clone();
        let watched = sid.clone();
        // No waiting and no yielding: the command returns before the live pass
        // can possibly have delivered anything.
        let command = async move {
            writing
                .append(&watched, EventSeq::ZERO, vec![draft("watched", EventType::TURN_STARTED)])
                .await
                .unwrap();
            Ok(CliOutput::Text("done".to_string()))
        };

        // The verdict is not what these tests are about: the sink is. It is
        // dropped on purpose, not by omission.
        let _ = watch_with(store.as_ref(), live.as_mut(), &sid, EventSeq::ZERO, command, &mut sink)
            .await
            .unwrap();

        let lines = recorded.lock().unwrap().lines.clone();
        let chain: Vec<&String> = lines.iter().filter(|l| l.starts_with("chain|")).collect();
        assert_eq!(chain.len(), 1, "printed exactly once, not once per path: {lines:?}");
        assert!(chain[0].contains("\"seq\":0"), "{}", chain[0]);
        assert!(lines.last().unwrap().starts_with("answer|"), "the answer comes last");
    }

    /// The recorder is shared with the command future, so the sink needs to be
    /// usable from two tasks at once. Guarded here rather than in each test.
    #[tokio::test]
    async fn a_failing_command_is_reported_and_not_answered() {
        let store = Arc::new(FakeEventStore::new());
        let bus = InMemoryLiveEventBus::new(8);
        let sid = SessionId::from_string("watched");
        let recorded = Arc::new(std::sync::Mutex::new(Recording::default()));
        let mut sink = RecordingSink(recorded.clone());
        let mut live = bus.subscribe().await.unwrap();

        let command = async move { Err::<CliOutput, String>("model refused".to_string()) };
        let outcome =
            watch_with(store.as_ref(), live.as_mut(), &sid, EventSeq::ZERO, command, &mut sink)
                .await;
        assert_eq!(outcome.unwrap_err(), "model refused");

        let lines = recorded.lock().unwrap().lines.clone();
        assert_eq!(lines.len(), 0, "nothing was answered or invented: {lines:?}");
    }

    /// A flag's value is never a positional (YA-76).
    ///
    /// The three commands that read a positional word — `approve`, `deny`, and
    /// the two plugin verbs — used to drop only the flag itself, so a value
    /// before the id became the id. This pins the reader they now share,
    /// including the two spellings and a boolean flag that must not eat the
    /// word after it.
    #[test]
    fn a_flag_value_is_never_read_as_a_positional() {
        let words = |argv: &[&str]| {
            let argv: Vec<String> = argv.iter().map(|word| word.to_string()).collect();
            let args = args::parse(&argv).expect("a readable command line");
            positionals(&args).into_iter().cloned().collect::<Vec<_>>()
        };

        assert_eq!(
            words(&["plugin", "disable", "--reason", "because", "mcp.one"]),
            ["disable", "mcp.one"]
        );
        assert_eq!(
            words(&["plugin", "disable", "--reason=because", "mcp.one"]),
            ["disable", "mcp.one"]
        );
        assert_eq!(words(&["deny", "--only", "call-1"]), ["call-1"]);
        assert_eq!(words(&["approve", "call-1", "turn"]), ["call-1", "turn"]);
    }
}
