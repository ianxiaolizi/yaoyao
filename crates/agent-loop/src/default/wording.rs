//! The words the loop shows for states it only names.
//!
//! None of these decide anything: they turn a status, a mode, or a pending
//! failure into the line the model or the log reads. Kept out of `default.rs`
//! because the loop is at its line ratchet and a phrase is not step machinery.

use super::*;

/// The call to suggest when the kernel asks the model to look. Naming the
/// run's own listing tool, when it has one, is advice that can be followed;
/// "list the directory" assumes the model maps that to a call by itself.
pub(super) fn listing_hint(manifest: &ToolManifest) -> String {
    if manifest.entries.iter().any(|entry| entry.tool_id == "fs.list") {
        String::from("for example fs.list on the directory")
    } else {
        String::from("list the directory")
    }
}

/// The `reviewed` line the loop fills in for a reconcile that left it out:
/// the failed call, cut to the field's own bound rather than to 160 plus an
/// ellipsis that would fail it.
pub(super) fn reviewed_from(failure: &PendingFailure) -> String {
    format!("{}: {}", failure.tool, failure.detail)
        .chars()
        .take(Judgment::MAX_RECONCILE_CHARS)
        .collect()
}

/// What the model is told when it tries to answer past a real failure.
///
/// Written the way the failure feedback is written: it names the call, the
/// one thing to do about it, and the fields that do it — not "think again".
pub(super) fn gate_text(failure: &PendingFailure, hint: String) -> String {
    format!(
        "Kernel check: `{}` failed ({}). Make the retry now with a tool call — {hint}, or \
         read a corrected path. Only if no retry is possible, record a reconcile with \
         judgment.update, `stop` true, and why.",
        failure.tool, failure.detail
    )
}

/// The tool arguments as one bounded line for a node's claim.
///
/// The arguments are the redacted ones: the chain is durable, and a secret the
/// model copied into a call must not sit in it.
pub(super) fn summarize_arguments(arguments: &serde_json::Value) -> String {
    let text = serde_json::to_string(arguments).unwrap_or_default();
    truncate(&text, 100)
}

/// The word a tool status is shown with in an observation.
pub(super) fn status_word(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
        ToolStatus::Cancelled => "cancelled",
    }
}

/// The word a turn ending is shown with in a situation.
pub(super) fn ending_word(ending: TurnEnding) -> String {
    String::from(match ending {
        TurnEnding::Completed => "completed",
        TurnEnding::Failed => "failed",
        TurnEnding::Cancelled => "cancelled",
        TurnEnding::Interrupted => "interrupted",
        TurnEnding::Yielded => "yielded",
    })
}

/// The word a sandbox mode is shown with in a situation.
pub(super) fn sandbox_word(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::LocalProcess => "local",
        SandboxMode::WorkspaceSandbox => "app-container",
        SandboxMode::Container => "container",
    }
}
