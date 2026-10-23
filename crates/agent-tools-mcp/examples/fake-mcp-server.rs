//! A minimal MCP server over stdio, used as a test fixture.
//!
//! It speaks newline-delimited JSON-RPC 2.0 — the stdio framing MCP specifies —
//! and implements only the methods the adapter's tests need: `initialize`,
//! `tools/list`, `tools/call`, and `ping`. It is deliberately hand-rolled rather
//! than built on the SDK's server half: a test double should not be able to
//! agree with the code under test by sharing its bugs, and the point of the
//! end-to-end test is to check the *client* against the wire, not against itself.
//!
//! The tool set is chosen to exercise the adapter's rules end to end:
//!
//!   * `echo` is read-only and idempotent (annotation classification);
//!   * `note.write` is destructive;
//!   * `fetch` is open-world;
//!   * `a b` and `a_b` collide after sanitizing (disambiguation);
//!   * `weird name!` needs its illegal characters replaced;
//!   * calling `grow` adds a tool and emits `notifications/tools/list_changed`,
//!     which is how the list-changed pump is driven from a real notification;
//!   * calling `exit` ends the process mid-call, so a test can watch the client
//!     survive a server that is truly gone — the case no transport-level
//!     re-initialization can paper over.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut grown = false;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or_default();

        // Notifications carry no id and get no reply.
        let Some(id) = id else {
            continue;
        };

        let (result, extra) = handle(method, &message, &mut grown);
        let response = match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        };
        writeln!(stdout, "{response}").ok();
        if let Some(notification) = extra {
            writeln!(stdout, "{notification}").ok();
        }
        stdout.flush().ok();
    }
}

type Handled = (Result<Value, (i64, String)>, Option<Value>);

fn handle(method: &str, message: &Value, grown: &mut bool) -> Handled {
    match method {
        "initialize" => {
            let requested = message
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or("2025-11-25");
            (
                Ok(json!({
                    "protocolVersion": requested,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "fake-mcp-server", "version": "0.1.0" },
                })),
                None,
            )
        }
        "ping" => (Ok(json!({})), None),
        "tools/list" => (Ok(json!({ "tools": tool_list(*grown) })), None),
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or(json!({}));
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

            if name == "grow" {
                *grown = true;
                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                });
                return (Ok(text_result("grew")), Some(notification));
            }

            // Ends the process without answering, so the caller sees the pipe
            // close mid-call — a server that dies while a request is in flight,
            // which is the failure a client cannot recover by re-initializing.
            if name == "exit" {
                std::process::exit(0);
            }

            let text = match name {
                "echo" => arguments.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                "note.write" => format!(
                    "wrote {}",
                    arguments.get("path").and_then(Value::as_str).unwrap_or("?")
                ),
                "fetch" | "weird name!" | "a b" | "a_b" => format!("{name} ran"),
                other => return (Err((-32602, format!("unknown tool: {other}"))), None),
            };
            (Ok(text_result(&text)), None)
        }
        other => (Err((-32601, format!("method not found: {other}"))), None),
    }
}

fn text_result(text: &str) -> Value {
    json!({ "content": [ { "type": "text", "text": text } ], "isError": false })
}

fn tool_list(grown: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "echo",
            "description": "Echo the given text back",
            "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "note.write",
            "description": "Write a note",
            "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } } },
            "annotations": { "destructiveHint": true },
        }),
        json!({
            "name": "fetch",
            "description": "Fetch a URL",
            "inputSchema": { "type": "object", "properties": { "url": { "type": "string" } } },
            "annotations": { "openWorldHint": true },
        }),
        json!({
            "name": "grow",
            "description": "Add a tool to the server",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "exit",
            "description": "End the server process",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "a b",
            "description": "First of two names that sanitize the same",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "a_b",
            "description": "Second of two names that sanitize the same",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "weird name!",
            "description": "A name with characters that are not legal here",
            "inputSchema": { "type": "object", "properties": {} },
        }),
    ];
    if grown {
        tools.push(json!({
            "name": "grown",
            "description": "A tool that appeared after a list_changed notification",
            "inputSchema": { "type": "object", "properties": {} },
        }));
    }
    tools
}
