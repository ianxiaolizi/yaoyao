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

use std::path::Path;

use agent_cli::Verdict;
use agent_cli::args::{self, Args, DEFAULT_SESSION};

use agent::config;

mod commands;
mod support;

use commands::approval::{approve, deny, tool};
use commands::inspect::{artifact, context, memory, metrics};
use commands::plugin::plugin;
use commands::sandbox::{doctor, sandbox_command};
use commands::schedule::{schedule, tick};
use commands::session::session;
use commands::turn::{export, fork, interrupt, resume, run, status, steer, why};
use commands::web::{serve, web};

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
pub(crate) fn exit_on(verdict: Verdict) {
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

pub(crate) fn usage() {
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
pub(crate) fn positionals(args: &Args) -> Vec<&String> {
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
    use crate::support::{
        DEFAULT_DATA_DIR, WatchSink, resolve_data_dir, resolve_workspace_root, watch_with,
    };
    use agent_cli::CliOutput;
    use agent_kernel::live_bus::InMemoryLiveEventBus;
    use agent_ports::event_store::EventStore;
    use agent_ports::live_event_bus::LiveEventBus;
    use agent_protocol::events::{Actor, EventDraft, EventType, LiveEvent};
    use agent_protocol::ids::EventSeq;
    use agent_protocol::ids::SessionId;
    use agent_protocol::ids::{EventId, RunId, StepId, TurnId};
    use agent_protocol::model::ModelStreamEvent;
    use agent_testkit::FakeEventStore;
    use std::path::PathBuf;
    use std::sync::Arc;
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
