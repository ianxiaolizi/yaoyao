//! The port between the permission chain and whatever can actually copy a
//! directory.
//!
//! The kernel knows when a recovery point is needed — a call is about to run
//! that may change files — and it knows nothing about where those files are or
//! how to store them. This trait is the seam: the kernel states the scope and
//! the reason, an adapter takes the bytes, and the refusal that comes back is
//! the difference between a call that runs recoverably and one that does not
//! run at all.
//!
//! What an implementation must not do:
//!
//! * **Downgrade a refusal.** Every error here stops the call. An implementation
//!   that cannot take a complete snapshot must say so; "snapshotted what it
//!   could" is exactly the state a caller cannot tell from success.
//! * **Follow a reparse point.** A junction or symlink in scope is a path out of
//!   the workspace wearing the workspace's name. Refusing is the only answer
//!   that keeps the snapshot's own boundary meaningful.
//! * **Trust its own output.** A restore reads a manifest written by an earlier
//!   process; a manifest, an artifact or an entry that no longer matches the
//!   digest it was stored under must be refused, not repaired.

use agent_protocol::workspace_snapshot::{
    ChangeManifest, RollbackReport, SnapshotManifest, WorkspaceSnapshotScope,
};

/// One capture request: what to snapshot, and the facts to bind it to.
///
/// The session, task and action digest are carried through into the manifest so
/// a snapshot can be answered for from the log alone — which action it was
/// taken for, in which session, at which task revision.
#[derive(Debug, Clone)]
pub struct SnapshotRequest {
    pub session_id: String,
    pub task_id: String,
    pub action_digest: String,
    pub scope: WorkspaceSnapshotScope,
}

/// Why a capture, a diff or a restore did not happen.
///
/// Variants name what the caller has to change: the budget is configuration,
/// the reparse point is the workspace's layout, and the corrupt manifest is the
/// store's integrity. None of them is recoverable by retrying the same call
/// unchanged.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The capture would exceed a configured bound. Refused rather than
    /// truncated: a partial snapshot is a recovery point that lies.
    #[error(
        "the recovery point would exceed its budget ({reason}); the action needs a narrower scope"
    )]
    OverBudget { reason: String },
    /// A path in scope is a junction or a symlink. Copying through it would
    /// capture — and later restore — bytes outside the workspace.
    #[error("'{path}' is a reparse point; a snapshot does not follow one out of the workspace")]
    ReparsePoint { path: String },
    /// The snapshot could not be written.
    #[error("the recovery point could not be stored: {reason}")]
    Unwritable { reason: String },
    /// No snapshot with that id is known to this data directory.
    #[error("no workspace snapshot '{snapshot_id}' is recorded here")]
    NotFound { snapshot_id: String },
    /// The stored manifest does not match the digest it was recorded under.
    #[error("snapshot '{snapshot_id}' cannot be restored: {reason}")]
    CorruptManifest { snapshot_id: String, reason: String },
    /// A path would land outside the workspace if the restore followed it now.
    #[error("'{path}' no longer resolves inside the workspace: {reason}")]
    Escaped { path: String, reason: String },
    /// The workspace could not be read or written.
    #[error("workspace I/O failed at '{path}': {reason}")]
    Io { path: String, reason: String },
}

/// Takes recovery points, compares them against the workspace, and puts the
/// workspace back.
#[async_trait::async_trait]
pub trait WorkspaceSnapshotter: Send + Sync {
    /// Capture `request.scope` and write a manifest.
    async fn capture(&self, request: SnapshotRequest) -> Result<SnapshotManifest, SnapshotError>;

    /// What changed since `manifest` was taken.
    ///
    /// A diff that cannot read a path in scope reports `complete: false` rather
    /// than quietly omitting it: the change list is an audit record, and an
    /// audit record that silently drops what it could not read is worse than
    /// one that says it is incomplete.
    async fn diff(&self, manifest: &SnapshotManifest) -> Result<ChangeManifest, SnapshotError>;

    /// Put the workspace back to `snapshot_id`.
    ///
    /// Idempotent: a second restore of an already-restored snapshot changes
    /// nothing and succeeds. A path the restore cannot safely touch is reported
    /// in `skipped`, and the report is then `complete: false`.
    async fn restore(&self, snapshot_id: &str) -> Result<RollbackReport, SnapshotError>;
}
