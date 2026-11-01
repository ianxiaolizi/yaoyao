//! What a machine check observed, and the decision a completion claim gets.
//!
//! A passing check is a fact about a moment. The session status module
//! ([`crate::status`]) can say a run finished and an answer was delivered; it
//! cannot say *which* check said so, what that check ran, or whether anything
//! has changed the work since. That is what this module adds: one
//! [`Evidence`] record per check, carrying the command that ran, the exit code,
//! the digests of what it printed, the revision of the task it covered, and who
//! ran it.
//!
//! [`ClaimVerdict`] is the kernel's answer to "may this evidence support a
//! completion claim?". The names are deliberately verbs of refusal —
//! [`ClaimVerdict::Missing`], [`ClaimVerdict::Stale`],
//! [`ClaimVerdict::VerifierError`], [`ClaimVerdict::IntegrityChanged`],
//! [`ClaimVerdict::ExitMasked`] — because the interesting case is the one where
//! a finished-looking run may not be called complete. Only
//! [`ClaimVerdict::Accepted`] is a yes.
//!
//! The derivation lives in `agent_kernel::completion`; like [`crate::status`]
//! this module states what the words mean and nothing about how a session's log
//! answers them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What ran a check.
///
/// The distinction is the one the 2026-11-01 incident exposed: a criterion
/// routed through the agent's own shell can be spoofed by that shell (`… | tail`
/// returns the pipe's status), while a structured verifier starts the program
/// directly and has no shell to lie to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// The operator's completion criterion, run through the tool gateway.
    ///
    /// Kept as a kind of its own rather than merged into `verifier` because a
    /// criterion's exit status is the status of a shell command, and a shell
    /// command can end the pipeline rather than the program.
    #[default]
    CompletionCriterion,
    /// A structured check: a program, its arguments, and its real exit status.
    Verifier,
    /// A structured check re-run from the frozen copy of the verification
    /// apparatus, after the working copy changed.
    Reviewer,
    /// A kind this build has no name for. Read, recorded, never accepted: an
    /// evidence record whose meaning is unknown is not a reason to claim
    /// anything.
    #[serde(other)]
    Unknown,
}

impl EvidenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompletionCriterion => "completion_criterion",
            Self::Verifier => "verifier",
            Self::Reviewer => "reviewer",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this kind runs the program directly, with no shell in between.
    pub fn is_structured(self) -> bool {
        matches!(self, Self::Verifier | Self::Reviewer)
    }
}

/// What one run of a check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// The check ran and said yes.
    Passed,
    /// The check ran and said no.
    Failed,
    /// The check itself could not run: the program is missing, it timed out, or
    /// it could not be read. Not a fact about the task, and never a pass.
    Error,
    /// A status this build has no name for.
    #[serde(other)]
    #[default]
    Unknown,
}

impl EvidenceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }
}

/// One check's observation, durable beside the event that recorded it.
///
/// Every optional field is optional for a reason: an older criterion event
/// carries a target but no exit code, and a check that could not start carries
/// neither. A reader must be able to tell "zero" from "not recorded", so the
/// absent case is `None` rather than a sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Stable name of this check within the session, derived from where it was
    /// recorded so replaying a log rebuilds the same id.
    pub check_id: String,
    /// The revision of the task this check covered. Freshness is "the revision
    /// has not moved since", not "the clock has not moved".
    pub task_revision: u64,
    pub kind: EvidenceKind,
    pub status: EvidenceStatus,
    /// The command or URL that was checked, already redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The program and arguments, when the check was structured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The program's own exit status. Absent for a criterion that ran through
    /// a shell: the gateway reports success, not a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_sha256: Option<String>,
    /// The number of tests the check's own summary reported, when it carried a
    /// readable count. Absent for a criterion, whose gateway output is prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_passed: Option<u64>,
    /// Artifacts the check produced or named, by their store references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    /// Digest of the environment the check ran in (program identity, relevant
    /// variables), so two checks of the same command can be told apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_hash: Option<String>,
    /// Who ran the check: `completion_criterion`, `verifier`, or a model label.
    pub verifier_identity: String,
    /// The executable or process that ran it, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_model: Option<String>,
    /// Digest of the context handed to a model reviewer, when a model was part
    /// of the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_context_hash: Option<String>,
    /// Digest of the verification apparatus (tests, benchmarks, scoring rules)
    /// as it stood when the check ran. Compared with the frozen manifest to
    /// notice that the thing being graded was edited (CONTRACT §7 #171).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_manifest_hash: Option<String>,
    /// What authorized the check: `profile.completion`, `operator`, `kernel`.
    pub authorization_source: String,
}

impl Evidence {
    /// A check that could not run, recorded so the refusal has a name.
    pub fn error(check_id: String, task_revision: u64, verifier_identity: String) -> Self {
        Self {
            check_id,
            task_revision,
            kind: EvidenceKind::Verifier,
            status: EvidenceStatus::Error,
            target: None,
            program: None,
            args: Vec::new(),
            cwd: None,
            exit_code: None,
            stdout_sha256: None,
            stderr_sha256: None,
            tests_passed: None,
            artifact_refs: Vec::new(),
            started_at: None,
            finished_at: None,
            environment_hash: None,
            verifier_identity,
            verifier_process: None,
            verifier_model: None,
            verifier_context_hash: None,
            input_manifest_hash: None,
            authorization_source: "kernel".to_string(),
        }
    }
}

/// The kernel's answer to a completion claim.
///
/// `Accepted` carries the evidence that supported it, so the caller that writes
/// the claim does not have to read the log again to name what supported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimVerdict {
    Accepted(Evidence),
    /// No check was ever recorded for this session.
    Missing,
    /// A check exists, but the task revision moved after it. The check still
    /// happened; it no longer covers what is on the table.
    Stale {
        evidence: Evidence,
        revision: u64,
    },
    /// The check ran and did not say yes — the criterion did not pass, or the
    /// record carries a word this build cannot read as one.
    Failed {
        evidence: Evidence,
    },
    /// The check itself could not run.
    VerifierError {
        evidence: Evidence,
    },
    /// The verification apparatus changed after the manifest was frozen: the
    /// graded thing was edited, so a pass from the working copy cannot stand.
    IntegrityChanged {
        evidence: Evidence,
        expected: String,
        found: Option<String>,
    },
    /// A criterion's target contains a shell operator that ends the pipeline
    /// with a different status than the program it wraps.
    ExitMasked {
        evidence: Evidence,
        operator: String,
    },
}

impl ClaimVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted(_) => "accepted",
            Self::Missing => "missing",
            Self::Stale { .. } => "stale",
            Self::Failed { .. } => "failed",
            Self::VerifierError { .. } => "verifier_error",
            Self::IntegrityChanged { .. } => "integrity_changed",
            Self::ExitMasked { .. } => "exit_masked",
        }
    }

    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }

    pub fn evidence(&self) -> Option<&Evidence> {
        match self {
            Self::Accepted(evidence)
            | Self::Stale { evidence, .. }
            | Self::Failed { evidence }
            | Self::VerifierError { evidence }
            | Self::IntegrityChanged { evidence, .. }
            | Self::ExitMasked { evidence, .. } => Some(evidence),
            Self::Missing => None,
        }
    }
}

/// The shell operators that make a command's reported status belong to the
/// pipeline rather than to the program being checked.
///
/// Deliberately conservative: only forms that *end* the pipeline and therefore
/// replace its status are listed. `2>&1` does not mask a status, and neither
/// does a redirection to a file; flagging them would make the detector cry wolf
/// on ordinary commands and teach a reader to ignore it.
pub const MASKING_OPERATORS: &[&str] =
    &["| tail", "| head", "| cat", "|| true", "; true", "& true"];

/// The first masking operator in `command`, if it has one.
///
/// Matching is on the command text rather than on a parsed pipeline because a
/// criterion is a shell command, and the point is to catch the common shape
/// (`node --test … | tail -5`) before a pass is believed. A command that hides a
/// masking operator from plain text matching is outside what this check can
/// promise; the structured verifier is the answer for those (CONTRACT §7 #170).
pub fn masking_operator(command: &str) -> Option<String> {
    let normalized: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    let joined = normalized.join(" ");
    MASKING_OPERATORS
        .iter()
        .find(|operator| joined.contains(**operator))
        .map(|op| (*op).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passed() -> Evidence {
        Evidence {
            check_id: "completion/3".to_string(),
            task_revision: 2,
            kind: EvidenceKind::CompletionCriterion,
            status: EvidenceStatus::Passed,
            target: Some("python check.py".to_string()),
            program: None,
            args: Vec::new(),
            cwd: None,
            exit_code: None,
            stdout_sha256: None,
            stderr_sha256: None,
            tests_passed: None,
            artifact_refs: Vec::new(),
            started_at: None,
            finished_at: None,
            environment_hash: None,
            verifier_identity: "completion_criterion".to_string(),
            verifier_process: None,
            verifier_model: None,
            verifier_context_hash: None,
            input_manifest_hash: None,
            authorization_source: "profile.completion".to_string(),
        }
    }

    #[test]
    fn an_evidence_record_round_trips_and_keeps_its_absent_fields_absent() {
        let evidence = passed();
        let text = serde_json::to_string(&evidence).unwrap();
        assert!(!text.contains("exit_code"), "a missing code must not become a number: {text}");
        let back: Evidence = serde_json::from_str(&text).unwrap();
        assert_eq!(back, evidence);
    }

    #[test]
    fn an_unknown_kind_is_data_not_an_error() {
        let value = serde_json::json!({
            "check_id": "x",
            "task_revision": 0,
            "kind": "something_this_build_does_not_know",
            "status": "passed",
            "verifier_identity": "v",
            "authorization_source": "operator",
        });
        let evidence: Evidence = serde_json::from_value(value).unwrap();
        assert_eq!(evidence.kind, EvidenceKind::Unknown);
        assert!(!evidence.kind.is_structured(), "an unknown check is never treated as structured");
    }

    #[test]
    fn the_wire_names_match_the_text_names() {
        for (value, wire) in [
            (EvidenceKind::CompletionCriterion, "completion_criterion"),
            (EvidenceKind::Verifier, "verifier"),
            (EvidenceKind::Reviewer, "reviewer"),
            (EvidenceKind::Unknown, "unknown"),
        ] {
            assert_eq!(value.as_str(), wire);
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        for (value, wire) in [
            (EvidenceStatus::Passed, "passed"),
            (EvidenceStatus::Failed, "failed"),
            (EvidenceStatus::Error, "error"),
            (EvidenceStatus::Unknown, "unknown"),
        ] {
            assert_eq!(value.as_str(), wire);
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
    }

    #[test]
    fn a_pipeline_that_ends_the_status_is_named() {
        assert_eq!(masking_operator("node --test | tail -5").as_deref(), Some("| tail"));
        assert_eq!(masking_operator("cargo   test   |  head").as_deref(), Some("| head"));
        assert_eq!(masking_operator("make check; true").as_deref(), Some("; true"));
        assert_eq!(masking_operator("run || true").as_deref(), Some("|| true"));
    }

    #[test]
    fn an_ordinary_command_is_not_masked() {
        assert_eq!(masking_operator("cargo test --workspace"), None);
        assert_eq!(masking_operator("cargo test 2>&1 > out.log"), None);
        assert_eq!(masking_operator("tail -f log.txt"), None, "a file name is not a pipeline");
    }
}
