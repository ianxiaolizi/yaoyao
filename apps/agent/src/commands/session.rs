//! `session`: create, list, hide or delete a session.

use std::path::Path;
use std::sync::Arc;

use agent_cli::args::Args;
use agent_ports::event_store::EventStore;
use agent_protocol::ids::SessionId;
use agent_store_jsonl::JsonlEventStore;

use crate::support::{Produced, answer, chain_start_for, data_dir, fail, option, report};
use crate::{EXIT_DID_NOT_RUN, exit_on, positionals, usage};

/// `session`: create, list, hide or delete a session.
///
/// None of the verbs assembles a runtime: a session exists because its log does,
/// and every question here is about that log and the directory that holds it. A
/// model provider that is misconfigured cannot stop a person from tidying up
/// their sessions.
pub(crate) async fn session(args: &Args) {
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
