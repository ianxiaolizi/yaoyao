//! The readers: commands that answer from what is already written down and
//! assemble as little as possible to do it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_cli::args::Args;
use agent_cli::context::run_context_inspect;
use agent_cli::memory::{DEFAULT_SEARCH_LIMIT, run_memory_reindex, run_memory_search};
use agent_cli::run_artifact_get;
use agent_index_sqlite::SqliteMemoryIndex;
use agent_ports::context_engine::ContextEngine;
use agent_ports::event_store::EventStore;
use agent_protocol::ids::EventSeq;
use agent_store_artifact::FsArtifactStore;
use agent_store_jsonl::JsonlEventStore;

use agent::config;

use crate::support::{answer, data_dir, fail, option, print, workspace_root};
use crate::{EXIT_DID_NOT_RUN, exit_on, positionals, usage};

/// `memory search` and `memory reindex`: the derived index over the log.
///
/// Neither assembles the run. Reading a session's memory has to work when the
/// model configuration does not, for the same reason `schedule` and `plugin`
/// do, and neither command starts a turn. `--events` is refused because nothing
/// here appends: a search that wrote would make the model's history depend on
/// who looked.
pub(crate) async fn memory(args: &Args) {
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
pub(crate) async fn context(args: &Args) {
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
pub(crate) async fn metrics(args: &Args) {
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
pub(crate) async fn artifact(args: &Args) {
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
