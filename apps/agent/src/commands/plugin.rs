//! `plugin`: what the configuration, the log and the queue say about plugins,
//! and the two requests that change the log's last word.

use std::sync::Arc;

use agent_cli::args::Args;
use agent_kernel::inbox::Inbox;
use agent_ports::event_store::EventStore;
use agent_store_jsonl::JsonlEventStore;

use agent::{config, plugins};

use crate::support::{Produced, chain_start, data_dir, option, report, workspace_root};
use crate::{EXIT_DID_NOT_RUN, positionals, usage};

pub(crate) async fn plugin(args: &Args) {
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
