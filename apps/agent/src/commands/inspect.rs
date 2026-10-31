//! The readers: commands that answer from what is already written down and
//! assemble as little as possible to do it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_cli::args::Args;
use agent_cli::context::run_context_inspect;
use agent_cli::memory::{
    DEFAULT_SEARCH_LIMIT, run_memory_export, run_memory_forget, run_memory_list,
    run_memory_reindex, run_memory_remember, run_memory_search,
};
use agent_cli::run_artifact_get;
use agent_index_sqlite::SqliteMemoryIndex;
use agent_memory_sqlite::SqliteMemoryStore;
use agent_ports::context_engine::ContextEngine;
use agent_ports::event_store::EventStore;
use agent_ports::memory_store::{ListFacts, MemoryOrigin, MemorySensitivity, Remember};
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
///
/// `remember`, `list`, `forget` and `export` are the other half: the facts kept
/// in the memory store, which a person writes here and the model writes through
/// `memory.remember`. They also write no events — the fact store is durable on
/// its own, and the session log's job is the conversation, not the memory.
pub(crate) async fn memory(args: &Args) {
    let words = memory_words(args);
    match words.first().map(String::as_str) {
        Some("search") => {
            let query = words[1..].join(" ");
            if query.trim().is_empty() {
                eprintln!("Usage: agent memory search <text>");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
            refuse_events(args, "memory search");
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
            refuse_events(args, "memory reindex");
            let index = memory_index();
            let outcome = match run_memory_reindex(&index, &args.session, args.json).await {
                Ok(outcome) => outcome,
                Err(e) => fail(args, &e),
            };
            print(&outcome.output);
            exit_on(outcome.verdict);
        }
        Some("remember") => {
            refuse_events(args, "memory remember");
            let text = words[1..].join(" ");
            if text.trim().is_empty() {
                eprintln!(
                    "Usage: agent memory remember <text> [--key <key>] [--namespace <ns>] \
                     [--confidence <0..1>] [--private] [--supersedes <id>] \
                     [--expires-in-days <n>]"
                );
                std::process::exit(EXIT_DID_NOT_RUN);
            }
            let confidence = match option(args, "--confidence") {
                None => None,
                Some(text) => match text.parse::<f32>() {
                    Ok(confidence) => Some(confidence),
                    Err(_) => {
                        eprintln!("--confidence needs a number between 0 and 1, got '{text}'");
                        std::process::exit(EXIT_DID_NOT_RUN);
                    }
                },
            };
            let expires_at = match option(args, "--expires-in-days") {
                None => None,
                Some(text) => match text.parse::<i64>() {
                    Ok(days) if days > 0 => {
                        Some(agent::clock::now() + chrono::Duration::days(days))
                    }
                    _ => {
                        eprintln!(
                            "--expires-in-days needs a positive number of days, got '{text}'"
                        );
                        std::process::exit(EXIT_DID_NOT_RUN);
                    }
                },
            };
            let request = Remember {
                namespace: option(args, "--namespace"),
                key: option(args, "--key"),
                text,
                confidence,
                origin: MemoryOrigin::User,
                source: None,
                sensitivity: if args.command_args().iter().any(|word| word == "--private") {
                    MemorySensitivity::Private
                } else {
                    MemorySensitivity::Normal
                },
                supersedes: option(args, "--supersedes"),
                expires_at,
            };
            let store = memory_store();
            let outcome =
                match run_memory_remember(&store, request, agent::clock::now(), args.json).await {
                    Ok(outcome) => outcome,
                    Err(e) => fail(args, &e),
                };
            print(&outcome.output);
            exit_on(outcome.verdict);
        }
        Some("list") => {
            refuse_events(args, "memory list");
            let all = args.command_args().iter().any(|word| word == "--all");
            let filter = ListFacts {
                namespace: option(args, "--namespace"),
                include_superseded: all,
                include_expired: all,
            };
            let store = memory_store();
            let outcome =
                match run_memory_list(&store, &filter, agent::clock::now(), args.json).await {
                    Ok(outcome) => outcome,
                    Err(e) => fail(args, &e),
                };
            print(&outcome.output);
            exit_on(outcome.verdict);
        }
        Some("forget") => {
            refuse_events(args, "memory forget");
            let Some(id) = words.get(1) else {
                eprintln!("Usage: agent memory forget <id>");
                std::process::exit(EXIT_DID_NOT_RUN);
            };
            let store = memory_store();
            let outcome = match run_memory_forget(&store, id, args.json).await {
                Ok(outcome) => outcome,
                Err(e) => fail(args, &e),
            };
            print(&outcome.output);
            exit_on(outcome.verdict);
        }
        Some("export") => {
            refuse_events(args, "memory export");
            let out = option(args, "--out");
            let store = memory_store();
            let outcome =
                match run_memory_export(&store, agent::clock::now(), out.as_deref(), args.json)
                    .await
                {
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
                 agent memory reindex\n       \
                 agent memory remember <text> [--key <key>] [--supersedes <id>]\n       \
                 agent memory list [--all]\n       \
                 agent memory forget <id>\n       \
                 agent memory export [--out <path>]"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// The words of a `memory` subcommand, with that subcommand's own value flags
/// and their values taken out.
///
/// The shared parser leaves a command's own options in place, so a `--key` and
/// the word after it would otherwise both read as text — and `remember` would
/// store the key as part of the sentence.
fn memory_words(args: &Args) -> Vec<String> {
    const VALUE_FLAGS: &[&str] =
        &["--key", "--namespace", "--confidence", "--supersedes", "--expires-in-days", "--out"];
    let mut words = Vec::new();
    let mut rest = args.command_args().iter();
    while let Some(word) = rest.next() {
        if word.starts_with("--") {
            if !word.contains('=') && VALUE_FLAGS.contains(&word.as_str()) {
                rest.next();
            }
            continue;
        }
        words.push(word.clone());
    }
    words
}

/// These verbs write to the memory store, not to a log, so a caller asking for
/// the chain would be asking for something that will not exist.
fn refuse_events(args: &Args, command: &str) {
    if args.events {
        eprintln!("{command} does not support --events; it writes no events");
        std::process::exit(EXIT_DID_NOT_RUN);
    }
}

/// The index the `memory` verbs read: under the same data directory as the log.
fn memory_index() -> SqliteMemoryIndex {
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(data_dir()));
    SqliteMemoryIndex::open(store, data_dir())
}

/// The fact store those verbs read and write: beside the session logs, so every
/// session in this data directory sees the same facts.
fn memory_store() -> SqliteMemoryStore {
    SqliteMemoryStore::open(data_dir())
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

    let data_dir = PathBuf::from(data_dir());
    let store: Arc<dyn EventStore> =
        Arc::new(JsonlEventStore::new(&data_dir).with_lease_wait(Duration::ZERO));
    // The log is read first, because the numbers this report is measured
    // against are the ones the run froze at assembly: the last
    // `profile/resolved` record carries the resolved context budget and where
    // each field came from. Re-deriving a policy from today's configuration
    // would measure the context against a request this run never sent.
    let events = match store.read_stream(&args.session, EventSeq::ZERO, usize::MAX).await {
        Ok(events) => events,
        Err(e) => {
            eprintln!("context log read failed: {e}");
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    };
    let recorded = recorded_budget(&events);

    // The engine still needs a policy to derive against: the recorded budget
    // when the log has one, and the explicit configuration only for a log
    // written before the record existed. A run with neither measures nothing,
    // exactly as before.
    let policy = match &recorded {
        Some(recorded) => Some(agent_context::trim::ContextPolicy::resolved(recorded.budget)),
        None => legacy_policy(&resolved.budget),
    };
    let engine = agent_context::ProjectionContextEngine::new(store.clone(), &data_dir);
    let engine = match policy {
        Some(policy) => engine.with_context_policy(policy),
        None => engine,
    };

    let derived = match engine.derive(&args.session).await {
        Ok(derived) => derived,
        Err(e) => {
            eprintln!("context derive failed: {e}");
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    };
    let mut report = agent_context::inspect::report(&derived.messages, &derived.usage, top);
    if let Some(recorded) = &recorded {
        report = agent_context::inspect::with_budget(report, &recorded.budget, &recorded.sources);
    }

    // The final section is read back from the log rather than recomputed: what
    // the last request actually carried is a recorded fact, and a prediction
    // would answer a different question.
    let outcome =
        match run_context_inspect(&report, &args.session, derived.revision, &events, args.json) {
            Ok(outcome) => outcome,
            Err(e) => fail(args, &e),
        };
    print(&outcome.output);
    exit_on(outcome.verdict);
}

/// The context budget a run froze, read from its `profile/resolved` record.
struct RecordedBudget {
    budget: agent_protocol::profile::ResolvedContextBudget,
    sources: std::collections::BTreeMap<String, String>,
}

/// The last resolved context budget in the log, if the run recorded one.
///
/// The snapshot is the run's own statement of what it derived; a missing
/// `context` field means the record predates model-adaptive budgets (or the
/// run never discovered a window), and the caller falls back to the explicit
/// configuration. The sources are read from the snapshot's canonical map, so a
/// report can say which field was declared and which was derived.
fn recorded_budget(events: &[agent_protocol::events::StoredEvent]) -> Option<RecordedBudget> {
    let payload = events
        .iter()
        .rev()
        .find(|event| {
            event.event.event_type.as_str() == agent_protocol::events::EventType::PROFILE_RESOLVED
        })
        .map(|event| &event.event.payload)?;
    let budget = payload.get("budget")?;
    let context = serde_json::from_value(budget.get("context")?.clone()).ok()?;
    let sources = budget
        .get("budget_sources")
        .and_then(|sources| serde_json::from_value(sources.clone()).ok())
        .unwrap_or_default();
    Some(RecordedBudget { budget: context, sources })
}

/// The policy the explicit configuration would have produced, for a log that
/// predates `profile/resolved`.
///
/// A best effort: the old fields carry a window and a reservation, not the
/// trigger and retention a resolved budget derives from them. The trigger
/// keeps the configured percentage, and retention is half the working set —
/// the summarizer's old default share.
fn legacy_policy(
    budget: &agent_protocol::profile::RunBudget,
) -> Option<agent_context::trim::ContextPolicy> {
    let window = budget.max_input_tokens?;
    let reserved = budget.reserved_output_tokens.unwrap_or(0);
    let working = window.saturating_sub(reserved);
    if working == 0 {
        return None;
    }
    let trigger = budget
        .compact_at_percent
        .map(|percent| {
            u32::try_from(u64::from(working) * u64::from(percent) / 100).unwrap_or(working)
        })
        .unwrap_or(working);
    Some(agent_context::trim::ContextPolicy {
        context_window_tokens: window,
        input_working_set_tokens: working,
        completion_cap_tokens: reserved,
        compaction_trigger_tokens: trigger,
        retain_recent_tokens: working / 2,
    })
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
