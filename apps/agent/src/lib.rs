//! The `agent` binary's own code, as a library.
//!
//! The binary is thin on purpose: what it owns besides argument handling is the
//! *assembly* — which store, which policy, which tool surface, which kernel
//! services — and that assembly has to be reachable from outside the binary,
//! because the acceptance tests are supposed to run it rather than a copy of it.
//! A binary's modules are private to it, so the assembly lives here and `main.rs`
//! calls in like any other caller.
//!
//! What stays in `main.rs` is the part that is genuinely about being a process:
//! parsing `argv`, reading the environment for the paths and the provider, and
//! turning a verdict into an exit status.

#![deny(unsafe_code)]

pub mod checks;
pub mod clock;
pub mod config;
pub mod decisions;
pub mod instructions;
pub mod plugins;
pub mod providers;
pub mod sandbox;
pub mod stack;
pub mod subagents;
