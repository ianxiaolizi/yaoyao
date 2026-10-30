//! `subagent <verb>`: the child half of a `kind = "self"` delegation.
//!
//! Not a user command. The parent starts this binary with `subagent serve` and
//! speaks a protocol over its stdin and stdout, so this module takes no
//! configuration, reads no session flag and belongs to no operator workflow —
//! it is a program another program runs.

use agent_cli::args::Args;

use crate::EXIT_DID_NOT_RUN;

pub(crate) async fn subagent(args: &Args) {
    let verb = args.command_args().first().map(String::as_str);
    match verb {
        Some(agent::subagent_serve::SERVE) => {
            if let Err(error) = agent::subagent_serve::serve(args).await {
                eprintln!("subagent serve failed: {error}");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
        }
        Some(other) => {
            eprintln!("unknown subagent command '{other}' (this build has 'serve')");
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        None => {
            eprintln!("subagent needs a verb: serve");
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}
