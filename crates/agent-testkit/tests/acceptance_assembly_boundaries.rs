//! Boundary test for YA-44: an acceptance suite drives the product's assembly,
//! not a copy of it.
//!
//! The three `m4`/`m5`/`m6` suites each used to wire up their own store,
//! boundary, tool surface, gateway, loop and scheduler, and describe the result
//! as "the way `apps/agent` assembles one". No test could check that claim, so
//! the product's wiring could change in any way at all and every acceptance
//! criterion would still pass — which is the one failure mode an acceptance
//! suite exists to prevent. The assembly now lives in `agent::stack::assemble`
//! (the binary calls it too), and these suites call it.
//!
//! What keeps it that way is this guard. It is a **text** guard, and says so: it
//! reads three files by path and looks for the constructors that mean "a stack
//! is being built here". A suite could still assemble a stack out of pieces it
//! spells differently (`use ... as ...`, a helper that wraps `Scheduler::new`)
//! and get past it; the probes at the bottom pin what it does and does not see,
//! and the reverse measurement recorded in the ledger — mutate `stack.rs` and
//! watch the suites go red — is the check that does not depend on reading text.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <workspace>/crates/agent-testkit.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// The suites that drive a turn, and therefore need a stack.
const SUITES: &[&str] = &[
    "m4_acceptance.rs",
    "m5_acceptance.rs",
    "m6_acceptance.rs",
    "security_regression.rs",
    "schedule_acceptance.rs",
    "memory_acceptance.rs",
];

/// The call that makes a suite a user of the product's assembly.
const ASSEMBLY: &str = "stack::assemble(";

/// Constructors that mean "this file is assembling a stack of its own".
///
/// Three of them together are what a stack is: a loop over a guarded gateway,
/// driven by a scheduler. Any one alone would be a coincidence — `m6`'s first
/// anchor builds a bare `PluginRegistry` on purpose, because its claim (the
/// authorization gate refuses before a turn exists) is about the registry and
/// not about a run.
const ASSEMBLY_MARKERS: &[&str] =
    &["Scheduler::new(", "DefaultAgentLoop::new(", "GuardedToolProvider::new("];

/// The markers a source file contains.
fn own_assembly_markers(source: &str) -> Vec<&'static str> {
    ASSEMBLY_MARKERS.iter().copied().filter(|marker| source.contains(marker)).collect()
}

#[test]
fn every_acceptance_suite_uses_the_product_assembly() {
    let root = workspace_root();
    let mut offenders = Vec::new();

    for suite in SUITES {
        let path = root.join("apps").join("agent").join("tests").join(suite);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()));

        let markers = own_assembly_markers(&source);
        if !markers.is_empty() {
            offenders.push(format!("{suite} builds its own stack: {}", markers.join(", ")));
        }
        if !source.contains(ASSEMBLY) {
            offenders.push(format!("{suite} never calls `{ASSEMBLY}`"));
        }
    }

    assert!(
        offenders.is_empty(),
        "an acceptance suite left the product's assembly behind: {offenders:?} — call \
         `agent::stack::assemble`, and take the model from `stack::Inputs.model`; a suite that \
         assembles its own stack passes whatever the product does"
    );
}

// ---------------------------------------------------------------------------
// Probes: the three suites are wired the way this guard wants, so a guard that
// answered "fine" to everything would pass and mean nothing. Feed the checker
// the shapes it has to tell apart, and pin the ones it cannot.
// ---------------------------------------------------------------------------

#[test]
fn a_stack_built_in_the_open_is_recognised() {
    let source = "let scheduler = Scheduler::new(store.clone(), loop_, 8);";
    assert_eq!(own_assembly_markers(source), vec!["Scheduler::new("]);
    assert!(!source.contains(ASSEMBLY));

    let gateway = "let g = GuardedToolProvider::new(tools, policy, approvals, guard, artifacts);";
    assert_eq!(own_assembly_markers(gateway), vec!["GuardedToolProvider::new("]);
}

#[test]
fn a_bare_registry_is_not_a_stack() {
    // The shape `m6`'s first anchor has: a registry on a store, no loop, no
    // scheduler, no gateway. Its refusal happens before a turn exists, so it is
    // not a run and this guard is not about it.
    let source = "let store = Arc::new(JsonlEventStore::new(data.path()));\n\
                  let registry = Arc::new(PluginRegistry::new(store.clone()));";
    assert!(own_assembly_markers(source).is_empty());
}

#[test]
fn the_marker_is_text_and_so_is_its_blind_spot() {
    // A name spelled differently is not seen. Pinned rather than hidden: this is
    // why the reverse measurement, not this file, is the check that counts.
    assert!(own_assembly_markers("let s = make_scheduler();").is_empty());
    assert!(own_assembly_markers("let s = kernel::scheduler::Scheduler::new(a, b, 8);").len() == 1);
    // And it cannot tell code from prose: a comment carrying the marker reads as
    // an assembly. A guard that is wrong this way is a guard that asks a reader
    // to look, which is the failure direction to prefer.
    assert_eq!(own_assembly_markers("// was: Scheduler::new(store, loop_, 8)").len(), 1);
}
