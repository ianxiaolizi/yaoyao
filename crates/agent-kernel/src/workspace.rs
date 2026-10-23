//! Workspace and lease validation.
//!
//! The durable types in `agent_protocol::permissions` hold only references and
//! metadata — no process handles, connections, tokens, or raw environment. This
//! module adds the configuration validation and lease-time logic the kernel
//! needs before those values are trusted. Extension traits are used because the
//! underlying types are owned by `agent-protocol`.

use std::path::Path;

use agent_protocol::permissions::{LeaseSpec, Workspace};
use chrono::{DateTime, Utc};

/// Errors from workspace validation.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace_root must be a non-empty absolute path: {0}")]
    InvalidRoot(String),
    #[error("writable_roots entry must be a non-empty absolute path: {0}")]
    InvalidWritableRoot(String),
    #[error("external_directories entry must be a non-empty absolute path: {0}")]
    InvalidExternalDirectory(String),
}

/// Validation logic for a [`Workspace`].
pub trait WorkspaceExt {
    /// Validate that every path is a non-empty absolute path.
    fn validate(&self) -> Result<(), WorkspaceError>;
}

impl WorkspaceExt for Workspace {
    fn validate(&self) -> Result<(), WorkspaceError> {
        require_absolute_nonempty(&self.workspace_root, WorkspaceError::InvalidRoot)?;
        for root in &self.writable_roots {
            require_absolute_nonempty(root, WorkspaceError::InvalidWritableRoot)?;
        }
        for dir in &self.external_directories {
            require_absolute_nonempty(dir, WorkspaceError::InvalidExternalDirectory)?;
        }
        Ok(())
    }
}

/// Lease-time logic for a [`LeaseSpec`].
///
/// A helper, not the arbiter of whether a grant still stands. Two of the three
/// scopes end on something other than a timestamp — `allow_once` leaves no
/// standing lease at all and `allow_for_turn` ends when its turn does — so
/// "is this in effect" is a question about the session's log, answered by
/// [`crate::approval::ApprovalService`]. Nothing in the run path asks this
/// (CONTRACT.md §7 裁决 #12): an engine that judged liveness from a timestamp
/// alone would say yes about leases nobody is standing behind.
pub trait LeaseSpecExt {
    /// Whether this lease has passed its expiry time. `None` means no expiry.
    fn is_expired(&self, now: DateTime<Utc>) -> bool;
}

impl LeaseSpecExt for LeaseSpec {
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|t| t <= now)
    }
}

fn require_absolute_nonempty(
    path: &str,
    err: impl Fn(String) -> WorkspaceError,
) -> Result<(), WorkspaceError> {
    if path.is_empty() || !Path::new(path).is_absolute() {
        return Err(err(path.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::permissions::{ApprovalScope, Capability};

    fn absolute(path: &str) -> String {
        let mut cwd = std::env::current_dir().unwrap();
        cwd.push(path);
        cwd.display().to_string()
    }

    #[test]
    fn valid_workspace_passes_validation() {
        let workspace = Workspace {
            workspace_root: absolute("root"),
            writable_roots: vec![absolute("root/src")],
            external_directories: vec![],
        };
        assert!(workspace.validate().is_ok());
    }

    #[test]
    fn empty_root_fails_validation() {
        let workspace = Workspace {
            workspace_root: String::new(),
            writable_roots: vec![],
            external_directories: vec![],
        };
        assert!(matches!(workspace.validate(), Err(WorkspaceError::InvalidRoot(_))));
    }

    #[test]
    fn relative_root_fails_validation() {
        let workspace = Workspace {
            workspace_root: "relative/path".to_string(),
            writable_roots: vec![],
            external_directories: vec![],
        };
        assert!(matches!(workspace.validate(), Err(WorkspaceError::InvalidRoot(_))));
    }

    #[test]
    fn relative_writable_root_fails_validation() {
        let workspace = Workspace {
            workspace_root: absolute("root"),
            writable_roots: vec!["relative".to_string()],
            external_directories: vec![],
        };
        assert!(matches!(workspace.validate(), Err(WorkspaceError::InvalidWritableRoot(_))));
    }

    #[test]
    fn lease_expiry() {
        let now = Utc::now();
        let none = LeaseSpec {
            capability: Capability::new("fs.write"),
            scope: ApprovalScope::AllowOnce,
            expires_at: None,
        };
        assert!(!none.is_expired(now));

        let past = LeaseSpec {
            capability: Capability::new("fs.write"),
            scope: ApprovalScope::AllowOnce,
            expires_at: Some(now - chrono::Duration::seconds(1)),
        };
        assert!(past.is_expired(now));

        let future = LeaseSpec {
            capability: Capability::new("fs.write"),
            scope: ApprovalScope::AllowOnce,
            expires_at: Some(now + chrono::Duration::seconds(1)),
        };
        assert!(!future.is_expired(now));
    }

    #[test]
    fn lease_serializes_only_references() {
        // A lease is references + metadata only — never handles, tokens, or env.
        let lease = LeaseSpec {
            capability: Capability::new("fs.write"),
            scope: ApprovalScope::AllowOnce,
            expires_at: None,
        };
        let value = serde_json::to_value(&lease).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 3);
        assert!(obj.contains_key("capability"));
        assert!(obj.contains_key("scope"));
        assert!(obj.contains_key("expires_at"));
        for forbidden in ["handle", "token", "secret", "env", "process"] {
            assert!(!obj.contains_key(forbidden), "lease leaked a forbidden field: {forbidden}");
        }
    }
}
