//! `agent rollback <snapshot_id>`: put the workspace back to a recovery point.
//!
//! The one command that touches the workspace *backwards*. Everything it needs
//! is on the assembled runtime — the snapshotter the gateway captured with and
//! the ledger the session's decisions live in — so a rollback is the same
//! object doing the same job, not a second view of the snapshot index that can
//! disagree with the one the run wrote to.
//!
//! It takes no `--turn`, no objective and no model: a recovery point is
//! identified by its id alone, and `--session` decides which log the completion
//! is written to. It prints restored, removed, skipped and the scope the
//! snapshot never covered, and exits non-zero when the rollback was not
//! complete — a partial restore is a fact a caller has to be able to see from
//! `$?`.

use agent_cli::args::Args;
use agent_cli::run_rollback;

use crate::support::{Produced, assemble, chain_start, fail, report};
use crate::{EXIT_DID_NOT_RUN, exit_on, positionals};

pub(crate) async fn rollback(args: &Args) {
    let words = positionals(args);
    let Some(snapshot_id) = words.first() else {
        eprintln!("Usage: agent rollback <snapshot_id>");
        std::process::exit(EXIT_DID_NOT_RUN);
    };

    let runtime = assemble(args).await;
    // Read before the restore runs: the rollback appends `workspace/
    // rollback.completed` to this session, and a chain asked for afterwards
    // would start past it.
    let from = chain_start(runtime.store.as_ref(), args).await;
    let session = args.session.clone();
    let outcome = match run_rollback(
        runtime.snapshots.as_ref(),
        runtime.permissions.as_ref(),
        &session,
        snapshot_id,
        args.json,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => fail(args, &error),
    };
    let verdict = outcome.verdict;
    report(runtime.store.as_ref(), args, from, Ok(Produced { output: outcome.output, session }))
        .await;
    exit_on(verdict);
}
