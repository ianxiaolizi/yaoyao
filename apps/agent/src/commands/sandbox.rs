//! `doctor` and `sandbox`: the two commands that ask about the machine rather
//! than about a session.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_cli::args::Args;
use agent_cli::run_doctor;
use agent_kernel::doctor::KernelDoctor;
use agent_protocol::ids::SchemaVersion;
use agent_store_jsonl::JsonlEventStore;

use agent::checks::{
    BuildHealthCheck, ExecutionSandboxCheck, ProfileHealthCheck, SessionHealthCheck,
};

use crate::support::{data_dir, fail, print, workspace_root};
use crate::{EXIT_DID_NOT_RUN, exit_on, usage};

pub(crate) async fn doctor(args: &Args) {
    let schema_version = SchemaVersion::current();
    let data_dir = PathBuf::from(data_dir());

    // Zero wait, because a session another process is driving has no evidence
    // to offer right now: waiting for the writer would make `doctor` hang behind
    // a long turn, and judging the log mid-append would call a live writer's
    // torn line a crash. The store is shared by the session checks so a log is
    // replayed once, and `log_tail` is registered first because it reports the
    // repair that replay performs.
    let store = Arc::new(JsonlEventStore::new(&data_dir).with_lease_wait(Duration::ZERO));
    let doctor = KernelDoctor::new(vec![
        Box::new(BuildHealthCheck::new(schema_version.clone())),
        Box::new(SessionHealthCheck::log_tail(store.clone(), data_dir.clone())),
        Box::new(SessionHealthCheck::seq_contiguity(store.clone(), data_dir.clone())),
        Box::new(SessionHealthCheck::open_entities(store.clone(), data_dir.clone())),
        Box::new(SessionHealthCheck::checkpoint_consistency(store, data_dir)),
        Box::new(ProfileHealthCheck::new(workspace_root(), args.clone())),
        Box::new(ExecutionSandboxCheck::from_environment()),
    ])
    .expect("the doctor's check IDs are unique");
    let outcome = match run_doctor(&doctor, schema_version, args.json).await {
        Ok(outcome) => outcome,
        Err(e) => fail(args, &e),
    };
    print(&outcome.output);
    // The verdict goes to the exit status, not into the report: a check that
    // reports "unhealthy" and exits `0` is a check nobody can act on (§3 C3.4).
    exit_on(outcome.verdict);
}

/// `sandbox setup|status`: the machine-level half of the AppContainer backend.
///
/// Not a session command: it reads no workspace configuration, opens no store
/// and writes no event. `setup` changes the machine once, from an elevated
/// prompt; `status` only reads, and is what the doctor and a caller who wants to
/// gate on readiness use.
pub(crate) fn sandbox_command(args: &Args) {
    match args.command_args().first().map(String::as_str) {
        Some("setup") => sandbox_setup(args),
        Some("status") => sandbox_status(args),
        Some(other) => {
            eprintln!("unknown sandbox subcommand '{other}'");
            usage();
            std::process::exit(EXIT_DID_NOT_RUN);
        }
        None => {
            eprintln!(
                "Usage: agent sandbox setup [--readonly <path>]...\n       agent sandbox status"
            );
            std::process::exit(EXIT_DID_NOT_RUN);
        }
    }
}

/// `sandbox setup`: idempotent, and refuses rather than half-succeeds.
fn sandbox_setup(args: &Args) {
    let readonly = sandbox_readonly_paths(args);
    let report = agent_sandbox_win::setup(agent_sandbox_win::PROFILE_NAME, &readonly)
        .unwrap_or_else(|e| fail(args, &e.to_string()));
    if args.json {
        match agent_cli::json_output(&serde_json::json!({
            "profile": report.profile,
            "sid": report.sid,
            "created": report.created,
            "nul_device": report.nul_granted,
            "read_execute": report.granted,
        })) {
            Ok(text) => println!("{text}"),
            Err(e) => fail(args, &e.to_string()),
        }
        return;
    }
    println!(
        "execution sandbox: profile={} sid={} created={} nul_device={}",
        report.profile,
        report.sid,
        if report.created { "yes" } else { "no" },
        if report.nul_granted { "granted" } else { "missing" }
    );
    for path in &report.granted {
        println!("granted read+execute: {path}");
    }
}

/// `sandbox status`: says whether the machine is ready, and is not ready when it
/// is not.
fn sandbox_status(args: &Args) {
    let status = agent_sandbox_win::status(agent_sandbox_win::PROFILE_NAME)
        .unwrap_or_else(|e| fail(args, &e.to_string()));
    if args.json {
        match agent_cli::json_output(&serde_json::json!({
            "profile": status.profile,
            "sid": status.sid,
            "nul_device": status.nul_granted,
        })) {
            Ok(text) => println!("{text}"),
            Err(e) => fail(args, &e.to_string()),
        }
    } else {
        println!(
            "execution sandbox: profile={} sid={} nul_device={}",
            status.profile,
            status.sid,
            if status.nul_granted { "granted" } else { "missing" }
        );
    }
    if !status.nul_granted {
        eprintln!(
            "the AppContainer setup is not in place: the null-device grant is missing. \
             Windows resets that device's security descriptor at every boot, so the grant \
             must be re-applied after a reboot: run `agent sandbox setup` from an elevated \
             prompt"
        );
        std::process::exit(EXIT_DID_NOT_RUN);
    }
}

/// The `--readonly` paths a setup run names, which must exist.
fn sandbox_readonly_paths(args: &Args) -> Vec<PathBuf> {
    let words = args.command_args();
    let mut paths = Vec::new();
    let mut index = 1; // after `setup`
    while let Some(word) = words.get(index) {
        if word == "--readonly" {
            let Some(value) = words.get(index + 1) else {
                fail(args, "--readonly needs a path");
            };
            paths.push(PathBuf::from(value));
            index += 2;
        } else if let Some(value) = word.strip_prefix("--readonly=") {
            if value.is_empty() {
                fail(args, "--readonly needs a path");
            }
            paths.push(PathBuf::from(value));
            index += 1;
        } else {
            fail(args, &format!("unexpected argument '{word}'"));
        }
    }
    paths
}
