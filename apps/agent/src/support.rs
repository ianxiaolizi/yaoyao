//! The plumbing every command shares: where a `--events` chain starts, where
//! its lines go, how a failure is reported, and how the stack is assembled.
//!
//! None of it decides what a command does; all of it decides what a caller
//! reads. It is split from `main.rs` so the dispatch table and the handlers can
//! be read as commands rather than as output handling.

use std::future::Future;
use std::path::{Path, PathBuf};

use agent_cli::args::Args;
use agent_cli::{CliOutcome, CliOutput, LiveLines, render_events, render_live};
use agent_ports::event_store::EventStore;
use agent_ports::live_event_bus::{LiveEventBus, LiveEventSubscription};
use agent_protocol::ids::{EventSeq, SessionId};
use futures_util::FutureExt;

use agent::stack::{self, Runtime};
use agent::{config, sandbox};

use crate::EXIT_DID_NOT_RUN;

/// A command's output, and the session whose log gained the events.
///
/// The two are the same except for `fork`, which writes to the child it
/// creates — so a caller printing the event chain cannot assume the session it
/// was given is the one that grew.
pub(crate) struct Produced {
    pub(crate) output: CliOutput,
    pub(crate) session: SessionId,
}

/// Where a `--events` chain starts for a command that writes to the session it
/// was given: just past the history that was already there.
///
/// Read before the command runs, because afterwards the same question has a
/// different answer. `None` means the chain was not asked for.
///
/// Takes the store rather than the `Runtime`, because the two `plugin` verbs
/// have a store and no runtime — and `--events` has to mean the same thing for
/// them as for everything else.
pub(crate) async fn chain_start(store: &dyn EventStore, args: &Args) -> Option<EventSeq> {
    chain_start_for(store, &args.session, args).await
}

/// Where the chain starts for a session other than the one `--session` names.
///
/// The `session` verbs act on a name of their own (`new --id`, or the id after
/// the verb), so they cannot use [`chain_start`] — and `fork` is the precedent:
/// the session that grew is not always the one that was given.
pub(crate) async fn chain_start_for(
    store: &dyn EventStore,
    session: &SessionId,
    args: &Args,
) -> Option<EventSeq> {
    if !args.events {
        return None;
    }
    match store.read_stream(session, EventSeq::ZERO, usize::MAX).await {
        Ok(events) => Some(events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO)),
        Err(e) => {
            eprintln!("cannot read the session's history: {e}");
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// Where a watched command's output goes.
///
/// A trait rather than three closures because the three streams are distinct
/// audiences — a program reads the chain, a person reads the live text — and
/// because a test can implement this to see exactly what a terminal would have
/// been shown, without a terminal.
pub(crate) trait WatchSink {
    /// One durable event, already rendered as the chain's JSON object.
    fn chain(&mut self, line: &str);
    /// Live text, already assembled with its labels and line breaks.
    fn live(&mut self, text: &str);
    /// A command's result, printed after the chain it produced.
    fn answer(&mut self, output: &CliOutput);
    /// Something went wrong while watching. The command itself is unaffected.
    fn note(&mut self, message: &str);
}

/// Everything a watched command writes goes to stdout as the chain and stderr
/// as the human-readable half, which is what keeps `… --events | jq` usable
/// while a person still sees the answer.
pub(crate) struct TerminalSink;

impl WatchSink for TerminalSink {
    fn chain(&mut self, line: &str) {
        println!("{line}");
    }

    fn live(&mut self, text: &str) {
        eprint!("{text}");
        // stderr is unbuffered in Rust, but a delta is written without a
        // newline and the point of watching is that it appears now.
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }

    fn answer(&mut self, output: &CliOutput) {
        match output {
            CliOutput::Text(text) => eprintln!("{text}"),
            CliOutput::Json(json) => eprintln!("{json}"),
        }
    }

    fn note(&mut self, message: &str) {
        eprintln!("{message}");
    }
}

/// Watch a command's own work when `--events` asks for it, or just run it.
///
/// `from` is where the chain starts, read before the command ran — by the
/// caller rather than here, because some commands repair the log before they
/// work and that repair is part of what the command did.
pub(crate) async fn watch<F, O>(
    runtime: &Runtime,
    args: &Args,
    from: Option<EventSeq>,
    command: F,
) -> CliOutcome
where
    F: Future<Output = Result<O, String>> + Send,
    // `Send` because the command's answer is held across the drain below: a
    // child the command started is collected before the answer is printed, and
    // the two are on opposite sides of an await.
    O: Into<CliOutcome> + Send,
{
    // A background delegation outlives the command that started it, so the
    // command is not over until its children are. The drain sits *inside* the
    // watched future rather than after it, so the settlements it writes are part
    // of the chain the caller asked to see, and so the answer below is printed
    // only once nothing is left running.
    let children = runtime.children.clone();
    let command = async move {
        let result = command.await;
        // Each note is a child whose ending could not be recorded or whose
        // handle could not be released. Said out loud because it is the one
        // thing about a delegation that would otherwise go missing silently.
        for note in children.drain().await {
            eprintln!("subagent: {note}");
        }
        result
    };

    let Some(from) = from else {
        return match command.await {
            Ok(output) => {
                let outcome = output.into();
                print(&outcome.output);
                outcome
            }
            Err(e) => fail(args, &e),
        };
    };

    let mut live = match runtime.live.subscribe().await {
        Ok(live) => live,
        Err(e) => fail(args, &format!("cannot watch live output: {e}")),
    };
    let mut sink = TerminalSink;
    match watch_with(runtime.store.as_ref(), live.as_mut(), &args.session, from, command, &mut sink)
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    }
}

/// A command's own option, or a usage refusal.
///
/// [`Args::option`] tells "not given" apart from "given without a value", and
/// the difference is the point: whoever typed `--at` and nothing after it meant
/// something, and reading that as "not given" would quietly substitute a
/// default for a typo. The refusal is the same one the shared flags get —
/// `<command> failed: --at needs a value`, and nothing ran.
pub(crate) fn option(args: &Args, name: &str) -> Option<String> {
    args.option(name).unwrap_or_else(|e| fail(args, &e.to_string()))
}

/// Report a command's failure the way every command does, and stop.
///
/// A command that reaches here did not run: the message is the reason, and there
/// is no answer to read. That is [`EXIT_DID_NOT_RUN`] and never
/// [`crate::EXIT_ANSWERED_NO`] — the difference is which stream the caller
/// should have been reading.
pub(crate) fn fail(args: &Args, message: &str) -> ! {
    eprintln!("{} failed: {message}", args.command);
    std::process::exit(EXIT_DID_NOT_RUN);
}

/// Run a command while its own events stream out.
///
/// Without `--events` nothing is watched and the output is printed as before.
/// With it, two subscriptions are driven alongside the command: durable events
/// to the chain as they are appended, live model text to the human half.
///
/// The live path decides *when* a line appears and never *which* lines exist.
/// When the command returns, the session is read once from `from` and anything
/// not yet printed is printed, so the chain that ends up on stdout is exactly
/// what reading the log afterwards would have produced — a long turn shows
/// progress instead of a blank screen, and a crash mid-turn still shows what
/// happened, without the live path being able to change the answer.
pub(crate) async fn watch_with<F, O>(
    store: &dyn EventStore,
    live: &mut dyn LiveEventSubscription,
    session: &SessionId,
    from: EventSeq,
    command: F,
    sink: &mut dyn WatchSink,
) -> Result<CliOutcome, String>
where
    F: Future<Output = Result<O, String>> + Send,
    O: Into<CliOutcome>,
{
    let mut events = store
        .subscribe(session, from)
        .await
        .map_err(|e| format!("cannot watch the session: {e}"))?;

    let mut lines = LiveLines::default();
    let mut last: Option<u64> = None;
    let mut events_open = true;
    let mut live_open = true;

    tokio::pin!(command);
    let outcome = loop {
        tokio::select! {
            outcome = &mut command => break outcome,
            event = events.next(), if events_open => match event {
                Ok(Some(stored)) => {
                    last = Some(stored.seq.as_u64());
                    sink.chain(&render_events(std::slice::from_ref(&stored)));
                }
                // The store is gone; the read at the end still has the last word.
                Ok(None) => events_open = false,
                // A subscription that cannot read right now might read later, but
                // polling one that failed would spin: the final read covers it.
                Err(e) => {
                    sink.note(&format!("live view stopped: {e}"));
                    events_open = false;
                }
            },
            chunk = live.next(), if live_open => match chunk {
                Some(event) => {
                    if let Some(chunk) = render_live(&event) {
                        sink.live(&lines.push(chunk));
                    }
                }
                None => live_open = false,
            },
        }
    };

    // A command can return in the very poll that published its last deltas, and
    // the select stops as soon as it does: what was already published would be
    // thrown away with the queue. Those deltas were produced while the command
    // ran, so they are drained rather than dropped. `now_or_never` does not
    // wait — a delta that has not arrived by now is not part of this run's
    // preview, and the answer that follows is the model's own final text.
    while live_open {
        match live.next().now_or_never() {
            Some(Some(event)) => {
                if let Some(chunk) = render_live(&event) {
                    sink.live(&lines.push(chunk));
                }
            }
            Some(None) => live_open = false,
            None => break,
        }
    }

    // The authoritative pass, and the reason the live one is a preview. It also
    // closes the gap the select left behind: events appended just before the
    // command returned may not have been delivered to the subscription yet.
    match store.read_stream(session, from, usize::MAX).await {
        Ok(events) => {
            for stored in events {
                if last.is_some_and(|l| stored.seq.as_u64() <= l) {
                    continue;
                }
                last = Some(stored.seq.as_u64());
                sink.chain(&render_events(std::slice::from_ref(&stored)));
            }
        }
        Err(e) => sink.note(&format!("could not read the event chain: {e}")),
    }

    // Close an open live line before anything else is written to stderr, so the
    // answer never runs into the middle of a model's sentence.
    let close = lines.finish();
    if !close.is_empty() {
        sink.live(close);
    }

    match outcome {
        Ok(output) => {
            let outcome = output.into();
            sink.answer(&outcome.output);
            Ok(outcome)
        }
        Err(e) => Err(e),
    }
}

/// Print a command's result, and with `--events` the chain it produced.
///
/// The chain goes to stdout and the human line to stderr when both are asked
/// for. That is the point of the flag: stdout stays something a program can
/// read, and nothing is hidden to get it.
pub(crate) async fn report(
    store: &dyn EventStore,
    args: &Args,
    from: Option<EventSeq>,
    produced: Result<Produced, String>,
) {
    let produced = match produced {
        Ok(produced) => produced,
        Err(e) => fail(args, &e),
    };

    let Some(from) = from else {
        print(&produced.output);
        return;
    };
    match store.read_stream(&produced.session, from, usize::MAX).await {
        Ok(events) => {
            let chain = render_events(&events);
            if !chain.is_empty() {
                println!("{chain}");
            }
        }
        Err(e) => eprintln!("could not read the event chain: {e}"),
    }
    match &produced.output {
        CliOutput::Text(text) => eprintln!("{text}"),
        CliOutput::Json(json) => eprintln!("{json}"),
    }
}

/// Assemble the stack this process runs on.
///
/// Everything the process environment decides is read here — the workspace root,
/// the data directory, the configuration, and the provider `AGENT_MODEL_PROVIDER`
/// names — and every way the assembly can refuse to start becomes one exit status
/// with a sentence on stderr. What the stack *is* is built in [`stack::assemble`],
/// which is the same function the acceptance tests call; this wrapper exists so
/// that "which run, where, under what configuration" is answered from the
/// environment rather than from a caller.
///
/// The configuration is read and resolved first, because everything after it
/// depends on the answer: which policy the gateway enforces, which model the
/// provider is asked for, and which budget the scheduler runs under. A
/// configuration that does not resolve stops the command here, before a store
/// is opened — a run that began under a configuration nobody agreed to is worse
/// than one that did not begin.
pub(crate) async fn assemble(args: &Args) -> Runtime {
    let root = workspace_root();
    let resolved = config::load(&root, args).unwrap_or_else(|e| {
        eprintln!("configuration: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });
    // Narrowing that a higher layer applied is reported, not swallowed: the
    // person who wrote the layer that lost is the one who can fix it.
    for note in &resolved.notes {
        eprintln!("configuration: {note}");
    }
    // The backend is the host's choice, and a choice this build cannot honour
    // stops the command before any store is opened rather than falling back to
    // a less confined one.
    let sandbox = sandbox::from_environment().unwrap_or_else(|e| {
        eprintln!("execution sandbox: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });
    // The network policy is parsed for every mode, so a typo is reported now
    // rather than the first time someone switches to a sandboxed backend.
    let network_allowed = sandbox::network_from_environment().unwrap_or_else(|e| {
        eprintln!("execution sandbox: {e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    });

    stack::assemble_with_delegation(
        stack::Inputs {
            session: args.session.clone(),
            workspace_root: PathBuf::from(root),
            data_dir: PathBuf::from(data_dir()),
            network_allowed,
            judgment: true,
            resolved,
            model: None,
            decisions: stack::Decisions::FromConfiguration,
            sandbox,
        },
        delegation_for(args),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(EXIT_DID_NOT_RUN);
    })
}

/// Whether a command's `subagent.run` calls wait for the child they started.
///
/// The three commands that run **one call and then exit** cannot wait for a
/// child anywhere else: there is no later step in the process to collect it,
/// and a receipt would be the last thing anyone saw of that child. Everything
/// else runs a turn, and a turn's next step is the reader a background
/// delegation exists for.
fn delegation_for(args: &Args) -> agent::subagents::Delegation {
    match args.command.as_str() {
        "tool" | "approve" | "resume" => agent::subagents::Delegation::Await,
        _ => agent::subagents::Delegation::Background,
    }
}

/// Where a run's filesystem boundary is rooted.
///
/// `AGENT_WORKSPACE_DIR` names it outright; with nothing named it is the
/// directory the command was invoked from. There is no third answer and no
/// default hidden in the code: a run whose base cannot be determined must not
/// begin, so an unreadable current directory is reported and stops the command
/// rather than panicking where no caller can catch it.
pub(crate) fn workspace_root() -> String {
    match resolve_workspace_root(
        std::env::var("AGENT_WORKSPACE_DIR").ok().as_deref(),
        std::env::current_dir(),
    ) {
        Ok(root) => root,
        Err(error) => {
            eprintln!(
                "workspace root: AGENT_WORKSPACE_DIR is not set and the current directory \
                 could not be read: {error}"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// `AGENT_WORKSPACE_DIR` against the process's current directory.
///
/// Split from [`workspace_root`] the way [`resolve_data_dir`] is split from
/// [`data_dir`]: the choice is a pure function of what the environment said and
/// what the current directory turned out to be, so "there is no current
/// directory" can be tested without making the test's own directory unreadable.
pub(crate) fn resolve_workspace_root(
    configured: Option<&str>,
    cwd: std::io::Result<PathBuf>,
) -> Result<String, std::io::Error> {
    match configured {
        Some(configured) => Ok(configured.to_string()),
        None => cwd.map(|dir| dir.display().to_string()),
    }
}

/// The data directory's name when the environment names none.
///
/// A bare name rather than a path: it resolves against the workspace root like
/// every other relative path, so it means "inside the workspace", not "wherever"
/// this process was started from".
pub(crate) const DEFAULT_DATA_DIR: &str = ".agent-data";

/// Where event logs and artifacts go.
pub(crate) fn data_dir() -> String {
    resolve_data_dir(&workspace_root(), std::env::var("AGENT_DATA_DIR").ok().as_deref())
}

/// `AGENT_DATA_DIR` against the workspace root, or the default inside it.
///
/// A relative value resolves against the workspace root — the same rule
/// `ResourceGuard` and the tool layer follow, and the reason the workspace root
/// exists: a run's paths all share one base, so a run's own state cannot land
/// in whatever directory the command happened to be invoked from. An absolute
/// value is already unambiguous and is taken as given. An empty value names no
/// directory at all and is treated as unset, because resolving "" against the
/// root yields the root itself — which would put the event log and the artifact
/// directory at the top of the workspace.
pub(crate) fn resolve_data_dir(root: &str, configured: Option<&str>) -> String {
    let configured = configured.filter(|dir| !dir.is_empty()).unwrap_or(DEFAULT_DATA_DIR);
    let path = Path::new(configured);
    if path.is_absolute() {
        configured.to_string()
    } else {
        Path::new(root).join(path).display().to_string()
    }
}

pub(crate) fn print(output: &agent_cli::CliOutput) {
    match output {
        agent_cli::CliOutput::Text(t) => println!("{t}"),
        agent_cli::CliOutput::Json(j) => println!("{j}"),
    }
}

/// Print a command's answer, honouring where `--events` puts it.
///
/// A command that produces no events has nothing for stdout when the chain was
/// asked for, so the answer moves to stderr — exactly as it does for a command
/// whose chain ends up empty. Writing it to stdout instead would hand a parser
/// a sentence where it expected JSON lines.
pub(crate) fn answer(args: &Args, output: &agent_cli::CliOutput) {
    if !args.events {
        print(output);
        return;
    }
    match output {
        agent_cli::CliOutput::Text(t) => eprintln!("{t}"),
        agent_cli::CliOutput::Json(j) => eprintln!("{j}"),
    }
}
