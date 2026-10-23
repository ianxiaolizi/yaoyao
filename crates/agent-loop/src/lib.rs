//! agent-loop: the default AgentLoop implementation.
//!
//! The loop decides how a turn advances through model and tool calls. It does
//! not touch the EventStore; it returns durable events for the Scheduler to
//! append. Future loop strategies (plan-execute, research, ...) can replace it.

#![deny(unsafe_code)]

mod default;

pub use default::DefaultAgentLoop;
