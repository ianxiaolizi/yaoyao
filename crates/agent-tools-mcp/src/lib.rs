//! MCP tool adapter — an MCP server's tools as a `ToolProvider`.
//!
//! The point of this crate is that adding an MCP server is a configuration
//! change and nothing else: the tools it offers enter the same surface as the
//! built-in families, and therefore pass through the same `GuardedToolProvider`
//! and the same `PolicyEngine`. Nothing here decides whether a call may proceed;
//! that decision belongs to the kernel, and the value of routing MCP through the
//! kernel rather than around it is that there is exactly one place it is made.
//!
//! Two layers, split so the deciding can be tested without a server:
//!
//!   * [`naming`] and [`provider`] turn a server's tool list into definitions,
//!     classify side effects, keep a last-good set, and page in reconnections.
//!     They are exercised against a fake [`McpConnection`].
//!   * [`rmcp_client`] is the only file that names the `rmcp` SDK; it opens real
//!     sessions over stdio and Streamable HTTP.

#![deny(unsafe_code)]

pub mod connection;
pub mod naming;
pub mod provider;
pub mod rmcp_client;

pub use connection::{McpAnnotations, McpConnection, McpConnector, McpError, McpTool};
pub use naming::MAX_TOOL_NAME_LEN;
pub use provider::{Backoff, DEFAULT_TOOL_CALL_TIMEOUT_MS, McpToolProvider};
pub use rmcp_client::{HttpSpec, PINNED_PROTOCOL_VERSION, RmcpConnector, StdioSpec, TransportSpec};
