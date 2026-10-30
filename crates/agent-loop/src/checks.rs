//! What counts as a check, and how a result is quoted back at the model.
//!
//! `verdict/criterion-observed` says an answer rests on a check that ran, so
//! "was that a check?" has to be answerable from the call itself: the command
//! it carries, and whether that command names a test or a verification. An
//! `fs.read tests/…` has no command and is only reading about a check
//! (CONTRACT §7 #82), which is why this distinction lives in one place rather
//! than being re-derived at each gate.

use agent_protocol::model::ToolCall;

use crate::context_layers::truncate;

/// The first line of a tool result, bounded — what the gate quotes back.
pub(crate) fn first_line(text: &str) -> String {
    truncate(text.lines().next().unwrap_or_default(), 200)
}

/// The command a call carries, when the call is one that runs a command.
///
/// This is the difference between looking at a check and running one. Only a
/// call that carries a command can observe a criterion; a tool whose arguments
/// name a file (`fs.read tests/…`) has no command and therefore cannot.
pub(crate) fn check_command(call: &ToolCall) -> Option<String> {
    ["command", "cmd", "script"]
        .iter()
        .find_map(|key| call.arguments.get(*key).and_then(|value| value.as_str()))
        .map(str::to_string)
}

/// Whether a command is a check: it names a test or a verification.
pub(crate) fn is_check_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    ["test", "check", "verify", "测试", "检查", "验证"].iter().any(|word| lower.contains(*word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall { tool_call_id: "call-1".to_string(), tool_name: name.to_string(), arguments }
    }

    #[test]
    fn only_a_call_that_runs_a_command_carries_one() {
        assert_eq!(
            check_command(&call("shell.run", serde_json::json!({ "command": "cargo test" }))),
            Some("cargo test".to_string())
        );
        assert_eq!(
            check_command(&call("shell.run", serde_json::json!({ "script": "pytest" }))),
            Some("pytest".to_string())
        );
        // Reading a test file is not running the test: there is no command.
        assert_eq!(
            check_command(&call("fs.read", serde_json::json!({ "path": "tests/spec.rs" }))),
            None
        );
    }

    #[test]
    fn a_check_is_named_in_either_language() {
        for command in ["cargo test", "npm run check", "python verify.py", "跑一下测试", "检查"]
        {
            assert!(is_check_command(command), "{command} names a check");
        }
        assert!(!is_check_command("ls -la"));
    }
}
