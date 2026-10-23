//! agent-subagent-stdio — a child agent as a child process.
//!
//! The smallest backend that can be called *external*: it starts a program, and
//! the two sides speak JSON lines. It exists so that "delegate to another agent"
//! has something behind it that is not a mock — a real process, with real pipes,
//! that really has to be killed — without binding this build to any particular
//! product. A real backend for a specific agent CLI is a later task; what is
//! settled here is the shape one has to have.
//!
//! Two layers, split so that the agreement can be tested without a process:
//!
//!   * [`wire`] is the vocabulary — what the parent writes, what a child may
//!     write back, and which of those is a mistake. It has no I/O in it.
//!   * [`provider`] is the lifecycle — one process per child, a task that owns
//!     it, and the deadline, cancellation and teardown that make a child
//!     something the parent can always end.
//!
//! What a backend *is* never reaches the kernel: the kernel sees
//! [`agent_ports::subagent::SubagentProvider`], and this crate is one
//! implementation of it that the assembled binary happens to link.

#![deny(unsafe_code)]

pub mod provider;
pub mod wire;

pub use provider::{DEFAULT_CEILING, StdioSubagentProvider};
pub use wire::{ChildMessage, RequestMessage, WireError};
