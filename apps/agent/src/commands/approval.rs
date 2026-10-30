//! `approve`, `deny` and `tool`: the verbs that settle or exercise the tool
//! boundary.

use agent_cli::args::Args;
use agent_cli::{run_approve, run_deny, run_tool};

use crate::support::{assemble, chain_start, watch};
use crate::{EXIT_DID_NOT_RUN, exit_on, positionals};

pub(crate) async fn approve(args: &Args) {
    let positional = positionals(args);
    let Some(approval_id) = positional.first() else {
        eprintln!("Usage: agent approve <approval_id> [once|turn|session|always]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let scope = positional.get(1).map(|s| s.as_str()).unwrap_or("once");
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    let outcome = watch(
        &runtime,
        args,
        from,
        run_approve(
            runtime.approvals.as_ref(),
            runtime.scheduler.inbox(),
            &args.session,
            approval_id,
            scope,
            args.json,
        ),
    )
    .await;
    exit_on(outcome.verdict);
}

pub(crate) async fn deny(args: &Args) {
    let positional = positionals(args);
    let Some(approval_id) = positional.first() else {
        eprintln!("Usage: agent deny <approval_id> [--only]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let only = args.command_args().iter().any(|a| a == "--only");
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    let outcome = watch(
        &runtime,
        args,
        from,
        run_deny(
            runtime.approvals.as_ref(),
            runtime.scheduler.inbox(),
            &args.session,
            approval_id,
            only,
            args.json,
        ),
    )
    .await;
    exit_on(outcome.verdict);
}

/// `tool`: run one tool through the same gateway a model's call goes through.
///
/// Its exit status is the command's whole alphabet. `0` when the call ran and
/// completed. `1` when it ran and came back failed: the result is the answer, it
/// is printed on stdout in whichever shape was asked for, and the status carries
/// the verdict — the one thing that cannot be put *in* the output, so a script
/// reads it without parsing. A call that could not be dispatched at all — an
/// unknown name, a policy denial, a path outside the workspace — is not an
/// answer but the absence of one: nothing on stdout, the reason on stderr, and
/// `2`, the same status every command returns when it did not run.
pub(crate) async fn tool(args: &Args) {
    let Some(tool_name) = args.command_args().first() else {
        eprintln!("Usage: agent tool <tool_name> [<json-args>]");
        std::process::exit(EXIT_DID_NOT_RUN);
    };
    let arguments: serde_json::Value = match args.command_args().get(1) {
        Some(raw) => match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("invalid JSON args: {e}");
                std::process::exit(EXIT_DID_NOT_RUN);
            }
        },
        None => serde_json::json!({}),
    };
    let runtime = assemble(args).await;
    let from = chain_start(runtime.store.as_ref(), args).await;

    let outcome = watch(
        &runtime,
        args,
        from,
        run_tool(runtime.gateway.as_ref(), &args.session, tool_name, arguments, args.json),
    )
    .await;
    // The tool's result has been printed by now, on stdout and in whichever
    // shape was asked for. The exit status is the one thing that cannot be put
    // in the output — it is what a script reads without parsing it.
    exit_on(outcome.verdict);
}
