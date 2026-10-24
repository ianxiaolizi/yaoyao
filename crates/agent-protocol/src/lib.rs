//! agent-protocol: IDs, Events, DTOs, and schema versioning.
//!
//! This crate is the protocol foundation — it has zero business dependencies
//! and defines the stable surface that all other crates consume. That is also
//! why every module declared here is reached from outside the crate: a `pub mod`
//! nothing uses is a claim about this build's vocabulary that nothing backs. The
//! request face is `InboxItem`, not a separate command type — `commands` used to
//! be a second, invented one (YA-39). Guarded by
//! `crates/agent-testkit/tests/protocol_module_surface.rs`.

#![deny(unsafe_code)]

pub mod compaction;
pub mod decision;
pub mod diagnostics;
pub mod digest;
pub mod events;
pub mod failures;
pub mod ids;
pub mod inbox;
pub mod instructions;
pub mod judgment;
pub mod ledger;
pub mod model;
pub mod permissions;
pub mod plan;
pub mod plugin;
pub mod profile;
pub mod reason;
pub mod redactor;
pub mod tool;
pub mod usertext;
pub mod versioning;
