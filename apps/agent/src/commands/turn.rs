//! The verbs about a turn: starting, continuing, forking, inspecting, steering,
//! stopping and exporting one.

use std::sync::Arc;

use agent_cli::args::Args;
use agent_cli::reason::run_why;
use agent_cli::{run_ask, run_export, run_fork, run_interrupt, run_resume, run_status, run_steer};
use agent_ports::event_store::EventStore;
use agent_protocol::ids::EventSeq;
use agent_store_jsonl::JsonlEventStore;

use crate::support::{
    Produced, assemble, chain_start, data_dir, fail, option, print, report, watch,
};
use crate::{EXIT_DID_NOT_RUN, exit_on};

/// `run` and `ask`: one implementation, two names.
pub(crate) async fn run(args: &Args) {
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
pub(crate) async fn resume(args: &Args) {
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
pub(crate) async fn fork(args: &Args) {
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

pub(crate) async fn status(args: &Args) {
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
pub(crate) async fn why(args: &Args) {
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

/// `steer`: queue text for whatever turn is in flight in this session.
///
/// No recovery pass: steering is an append to a session another process may be
/// driving right now, and repairing that session's tail would be writing to a
/// log this command is only here to talk to.
pub(crate) async fn steer(args: &Args) {
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
pub(crate) async fn interrupt(args: &Args) {
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;
    let outcome =
        watch(&runtime, args, from, run_interrupt(&runtime.scheduler, &args.session, args.json))
            .await;
    exit_on(outcome.verdict);
}

/// `export`: write the session out as a readable document and a machine bundle.
pub(crate) async fn export(args: &Args) {
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
