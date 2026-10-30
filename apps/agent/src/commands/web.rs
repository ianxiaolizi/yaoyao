//! `web` and `serve`: the two commands that stay running.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_cli::args::Args;
use agent_cli::schedule::{
    SERVE_POLL, Tick, earliest_due, next_wait, render_did_not_run, render_ran, tick_once,
};
use agent_ports::event_store::EventStore;
use agent_protocol::ids::TurnId;
use agent_store_jsonl::JsonlEventStore;

use agent::{config, sandbox};

use crate::EXIT_DID_NOT_RUN;
use crate::support::{assemble, data_dir, fail, option, print, workspace_root};

/// The port `agent web` listens on unless `--port` names another.
const DEFAULT_WEB_PORT: u16 = 7431;

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
pub(crate) async fn web(args: &Args) {
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
pub(crate) async fn serve(args: &Args) {
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
