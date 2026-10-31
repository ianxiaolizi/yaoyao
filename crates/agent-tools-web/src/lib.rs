//! agent-tools-web: the `web.request` tool.
//!
//! This is an adapter crate like the fs and git tools: it implements
//! [`agent_ports::tool_provider::ToolProvider`] and depends only on
//! `agent-ports` and `agent-protocol`. The address guard lives beside the
//! provider rather than behind a port, because it is a property of this tool
//! and of nothing else — a second HTTP tool would share it by importing it, not
//! by the kernel carrying it.

#![deny(unsafe_code)]

pub mod guard;
mod provider;

pub use guard::{Addresses, GuardError, Target, is_public};
pub use provider::{
    MAX_RESPONSE_BYTES, MAX_WEB_TIMEOUT_MS, WEB_TIMEOUT_MS, WEB_TOOL_NAME, WebToolProvider,
};
