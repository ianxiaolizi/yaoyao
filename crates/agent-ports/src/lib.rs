//! agent-ports: Trait seams for all pluggable capabilities.
//!
//! This crate defines the interfaces that adapters implement.
//! It depends only on agent-protocol types.

#![deny(unsafe_code)]

pub mod agent_loop;
pub mod artifact_store;
pub mod context_engine;
pub mod control;
pub mod decision;
pub mod doctor;
pub mod event_store;
pub mod execution_environment;
pub mod instructions;
pub mod live_event_bus;
pub mod memory_index;
pub mod model_provider;
pub mod model_router;
pub mod process_env;
pub mod process_launcher;
pub mod secret_provider;
pub mod subagent;
pub mod tool_gateway;
pub mod tool_provider;
