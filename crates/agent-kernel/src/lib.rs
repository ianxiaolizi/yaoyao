//! agent-kernel: Inbox, Scheduler, Turn/Run/Step, Approval orchestration.
//!
//! The kernel consumes only agent-protocol types and agent-ports traits.
//! It never depends on concrete adapters.

#![deny(unsafe_code)]

pub mod approval;
pub mod doctor;
pub mod fork;
pub mod guarded;
pub mod inbox;
pub mod judgment;
pub mod lifecycle;
pub mod live_bus;
pub mod model_selection;
pub mod plan;
pub mod plugin;
pub mod policy;
pub mod reason;
pub mod recovery;
pub mod resource_guard;
pub mod resume;
pub mod schedule;
pub mod scheduler;
pub mod session_archive;
pub mod session_lifecycle;
pub mod subagent;
pub mod tool_schema;
pub mod tools;
pub mod workspace;
