//! Binary entry point for the `agent-cli` library.
//!
//! This is not the product entry point: `agent-cli` is a library of command
//! runners, and `apps/agent` is the binary that assembles them. Run on its own
//! it has no store, no profile and no model, so it refuses instead of
//! pretending it could run — a usage error, which exits `2` (nothing ran).

#![deny(unsafe_code)]

fn main() {
    eprintln!("Use the `agent` binary (apps/agent) instead of agent-cli directly.");
    std::process::exit(2);
}
