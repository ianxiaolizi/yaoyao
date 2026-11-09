//! Help is a read-only answer, not a command the parser has to invent.
//!
//! `--help`, `-h`, and `agent help [command]` all stop before assembly. That
//! matters for commands whose normal path reads configuration or starts a
//! process: asking what a command does must not have side effects.

use agent::config;
use agent_cli::args::DEFAULT_SESSION;

pub(crate) fn usage_text() -> String {
    format!(
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
         verify [flags] -- <program> [args]\n                              Run a check without a shell and record what it found\n  \
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
         subagent serve [args]      Answer one delegation's objective, then exit. A parent\n                              process starts this, not a person: it is the child half\n                              of kind = \"self\" in [subagents.<id>]\n  \
         web [--port <n>]           Serve the GUI on loopback, token printed once\n  \
         memory search <text>       Find events whose text contains <text>\n  \
         memory reindex             Rebuild the searchable index from the session log\n  \
         memory remember <text>     Keep a fact across sessions (--key, --supersedes, --private)\n  \
         memory list [--all]        Show remembered facts (--all includes replaced and expired)\n  \
         memory forget <id>         Delete a remembered fact for good\n  \
         memory export [--out]      Every remembered fact as JSON\n  \
         context inspect            Show what the next request would carry (size, kinds, budget)\n  \
         metrics [--out <path>]     Count turns, tools, approvals, tokens and time\n                              from the event log (every session unless --session names one)\n  \
         artifact get <ref> [--out] Fetch a stored blob (locator or hash). Text to stdout otherwise\n  \
         rollback <snapshot_id>     Restore the workspace to a recovery point; --session names\n                              the log the rollback is recorded in\n  \
         sandbox setup              Create the AppContainer profile and its grants (elevated)\n  \
         sandbox status             Report whether the AppContainer setup is in place\n  \
         doctor                     Run health checks\n\n\
         Options:\n  \
         --session <id>             Act on this session (default: {DEFAULT_SESSION})\n  \
         --profile <name>           Compose this profile for the run\n  \
         --model <name>             Ask the provider for this model\n  \
         --task <name>              Apply the user configuration's [task_budgets.<name>]\n  \
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
    )
}

struct CommandHelp {
    name: &'static str,
    usage: &'static str,
    detail: &'static str,
}

const COMMANDS: &[CommandHelp] = &[
    CommandHelp {
        name: "run",
        usage: "Usage: agent run <text> [--session <id>] [--json|--events]",
        detail: "Ask the agent to do something. `ask` is an alias.",
    },
    CommandHelp {
        name: "ask",
        usage: "Usage: agent ask <text> [--session <id>] [--json|--events]",
        detail: "Alias of `run`.",
    },
    CommandHelp {
        name: "resume",
        usage: "Usage: agent resume [text] [--session <id>]",
        detail: "Continue a turn that is waiting on a decision.",
    },
    CommandHelp {
        name: "fork",
        usage: "Usage: agent fork [--at <turn>] [--session <id>]",
        detail: "Derive a new session from a closed turn boundary.",
    },
    CommandHelp {
        name: "steer",
        usage: "Usage: agent steer \"<text>\" [--session <id>]",
        detail: "Queue text for the turn in flight; it is applied at the next step.",
    },
    CommandHelp {
        name: "interrupt",
        usage: "Usage: agent interrupt [--session <id>]",
        detail: "Stop the run in flight at its next step boundary.",
    },
    CommandHelp {
        name: "export",
        usage: "Usage: agent export [--out <dir>] [--redacted] [--session <id>]",
        detail: "Write the session as session.md and bundle.json, or print them.",
    },
    CommandHelp {
        name: "why",
        usage: "Usage: agent why [--turn <id>] [--depth <n>] [--session <id>] [--json]",
        detail: "Explain a turn's reasoning chain backwards to its evidence.",
    },
    CommandHelp {
        name: "status",
        usage: "Usage: agent status [--session <id>] [--json]",
        detail: "Show the session's durable state.",
    },
    CommandHelp {
        name: "verify",
        usage: "Usage: agent verify [flags] -- <program> [args]",
        detail: "Run a check without a shell and record what it found.",
    },
    CommandHelp {
        name: "approve",
        usage: "Usage: agent approve <id> [once|turn|session|always]",
        detail: "Answer a pending approval. The answer must come from an interactive terminal.",
    },
    CommandHelp {
        name: "deny",
        usage: "Usage: agent deny <id> [--only]",
        detail: "Deny a pending request; without `--only`, deny the session's pending set.",
    },
    CommandHelp {
        name: "tool",
        usage: "Usage: agent tool <name> [json-args] [--session <id>] [--json]",
        detail: "Run one tool through the same policy gateway the model uses.",
    },
    CommandHelp {
        name: "plugin",
        usage: "Usage: agent plugin <list|disable|enable> [id]",
        detail: "Inspect or change the plugin registry at a turn boundary.",
    },
    CommandHelp {
        name: "session",
        usage: "Usage: agent session <new|list|archive|unarchive|delete> [id]",
        detail: "Create, list, archive, restore, or delete session logs.",
    },
    CommandHelp {
        name: "schedule",
        usage: "Usage: agent schedule <at|list|cancel> ...",
        detail: "Create, list, or cancel a scheduled turn.",
    },
    CommandHelp {
        name: "tick",
        usage: "Usage: agent tick [--session <id>]",
        detail: "Run at most one pending or due turn.",
    },
    CommandHelp {
        name: "serve",
        usage: "Usage: agent serve [--session <id>]",
        detail: "Keep running turns as they become due; one driver per session.",
    },
    CommandHelp {
        name: "subagent",
        usage: "Usage: agent subagent serve [args]",
        detail: "Run the child half of a delegated subagent turn.",
    },
    CommandHelp {
        name: "web",
        usage: "Usage: agent web [--port <n>] [--provider <echo|openai>] [--model <name>]",
        detail: "Serve the embedded GUI on loopback and print a one-time token URL.",
    },
    CommandHelp {
        name: "memory",
        usage: "Usage: agent memory <search|reindex|remember|list|forget|export> ...",
        detail: "Search the session index or manage durable remembered facts.",
    },
    CommandHelp {
        name: "context",
        usage: "Usage: agent context inspect [--json] [--top <n>] [--session <id>]",
        detail: "Show what the next model request would carry.",
    },
    CommandHelp {
        name: "metrics",
        usage: "Usage: agent metrics [--out <path>] [--session <id>] [--json]",
        detail: "Derive turn, tool, approval, token, and timing metrics from logs.",
    },
    CommandHelp {
        name: "artifact",
        usage: "Usage: agent artifact get <ref> [--out <path>]",
        detail: "Fetch an artifact by locator or content hash.",
    },
    CommandHelp {
        name: "rollback",
        usage: "Usage: agent rollback <snapshot_id> [--session <id>]",
        detail: "Restore a workspace recovery point and record the rollback.",
    },
    CommandHelp {
        name: "sandbox",
        usage: "Usage: agent sandbox <setup|status> [--readonly <path>]",
        detail: "Prepare or inspect the Windows AppContainer backend.",
    },
    CommandHelp {
        name: "doctor",
        usage: "Usage: agent doctor [--json]",
        detail: "Run health checks for the selected runtime.",
    },
    CommandHelp {
        name: "help",
        usage: "Usage: agent help [command]",
        detail: "Show the command list or one command's usage.",
    },
];

fn find(name: &str) -> Option<&'static CommandHelp> {
    COMMANDS.iter().find(|entry| entry.name == name)
}

fn is_command(name: &str) -> bool {
    find(name).is_some()
}

/// Return `Some(target)` when this line asks for help.
///
/// `Some(None)` is top-level help and `Some(Some(command))` is command help.
/// Everything after `--` belongs to a child program, so a child's own
/// `--help` must not be mistaken for the agent's help.
pub(crate) fn target(argv: &[String]) -> Option<Option<String>> {
    let before_separator: Vec<String> =
        argv.iter().take_while(|argument| argument.as_str() != "--").cloned().collect();
    if before_separator.first().is_some_and(|argument| argument == "help") {
        return Some(
            before_separator.iter().skip(1).find(|argument| is_command(argument)).cloned(),
        );
    }
    if before_separator.iter().any(|argument| argument == "--help" || argument == "-h") {
        return Some(before_separator.iter().find(|argument| is_command(argument)).cloned());
    }
    None
}

pub(crate) fn print(target: Option<&str>) {
    let Some(command) = target else {
        println!("{}", usage_text());
        return;
    };
    let Some(entry) = find(command) else {
        println!("Unknown help topic '{command}'.");
        println!();
        println!("{}", usage_text());
        return;
    };
    println!("{}", entry.usage);
    println!();
    println!("{}", entry.detail);
    println!();
    println!(
        "Global options: --session <id>, --profile <name>, --model <name>, --task <name>, \
         --json, --events."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn help_flags_and_the_help_verb_find_the_same_target() {
        assert_eq!(target(&args(&["--help"])), Some(None));
        assert_eq!(target(&args(&["help"])), Some(None));
        assert_eq!(target(&args(&["help", "run"])), Some(Some("run".to_string())));
        assert_eq!(target(&args(&["--help", "run"])), Some(Some("run".to_string())));
        assert_eq!(target(&args(&["run", "--help"])), Some(Some("run".to_string())));
    }

    #[test]
    fn a_child_programs_help_stays_after_the_separator() {
        assert_eq!(target(&args(&["verify", "--", "echo", "--help"])), None);
        assert_eq!(target(&args(&["verify", "--help", "--", "echo"])), Some(Some("verify".into())));
    }

    #[test]
    fn unknown_help_topics_are_not_commands() {
        assert_eq!(target(&args(&["help", "not-a-command"])), Some(None));
    }
}
