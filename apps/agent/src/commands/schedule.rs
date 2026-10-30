//! `schedule` and `tick`: the verbs that arrange turns at a time and the one
//! that runs them.

use std::sync::Arc;

use agent_cli::args::Args;
use agent_cli::schedule::{run_schedule_at, run_schedule_cancel, run_schedule_list, run_tick};
use agent_kernel::inbox::Inbox;
use agent_ports::event_store::EventStore;
use agent_store_jsonl::JsonlEventStore;

use crate::support::{Produced, assemble, chain_start, data_dir, fail, print, report};
use crate::{EXIT_DID_NOT_RUN, exit_on, positionals, usage};

/// `schedule`: arrange, list, or cancel turns that run at a time.
///
/// None of the three assembles a runtime, for the same reason the `plugin`
/// verbs do not: what they touch is a log and a queue, and assembling a model,
/// a tool surface and a plugin registry would make scheduling fail whenever the
/// model provider is misconfigured — a failure with nothing to do with the
/// question asked.
pub(crate) async fn schedule(args: &Args) {
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
pub(crate) async fn tick(args: &Args) {
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
