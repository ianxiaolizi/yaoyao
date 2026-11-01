//! Workspace snapshots: the recovery point a call that may change files is
//! made under, and the record of what it changed.
//!
//! Three questions, each answered by one shape:
//!
//! * **What was captured?** [`SnapshotManifest`] — the scope, every entry with
//!   its content digest and the artifact that holds its bytes, and the byte
//!   total. A manifest is content-addressed data, so a reader that trusts it
//!   must first check that it still hashes to its own digest.
//! * **What changed?** [`ChangeManifest`] — the entries that appeared,
//!   disappeared or differ, computed by comparing the manifest against the
//!   filesystem after the call.
//! * **What did rollback do?** [`RollbackReport`] — what was put back, what was
//!   removed, and what was left alone with the reason.
//!
//! This module deliberately holds no I/O: taking a snapshot, storing a blob or
//! writing a file is the adapter's job. It also holds no policy: which actions
//! get a snapshot is decided beside the permission chain, and whether one may
//! go over budget is the snapshotter's refusal.
//!
//! `unrecoverable_scope` is the honest half of the promise. A snapshot that
//! skipped something says so here, and rollback repeats it, so nobody reads
//! "restored" as "the workspace is exactly as it was".

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::digest;
use crate::ids::SnapshotId;
use crate::permissions::ArtifactRef;

/// What one snapshot covers.
///
/// Two shapes, because the two callers know different things. A tool call names
/// the files it will touch, and that is all that has to be recoverable. A whole
/// workspace action (a shell command, a destructive call) names no path, and
/// only the writable roots are a true bound on what it may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorkspaceSnapshotScope {
    /// Exactly these files, named relative to the workspace root (or
    /// absolutely, if the caller wrote an absolute path).
    Paths { paths: Vec<String> },
    /// Every file under these roots, excluding [`UNRECOVERABLE_ALWAYS`].
    WritableRoots { roots: Vec<String> },
}

/// Paths a snapshot never covers, and says so.
///
/// The data directory holds the session log, the artifact store and the plugin
/// state: copying it inside itself is how a snapshot becomes the largest thing
/// on the disk, and restoring it would rewrite the very log that records the
/// snapshot.
pub const UNRECOVERABLE_ALWAYS: &[&str] = &[".agent-data"];

/// What one manifest entry was, at capture time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotEntryKind {
    /// A regular file whose bytes were stored.
    #[default]
    File,
    /// A path that did not exist. Captured so rollback can remove what a call
    /// created where nothing was.
    Missing,
}

/// One file in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// The path relative to the workspace root, with forward slashes: the
    /// spelling a manifest has to survive a move of the workspace.
    pub relative_path: String,
    pub kind: SnapshotEntryKind,
    pub size: u64,
    /// SHA-256 of the bytes when this is a file. The artifact's own name in a
    /// content-addressed store, repeated here so a reader can check an entry
    /// without resolving the store's layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Where the bytes live. `None` for a missing path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}

/// Everything one capture recorded.
///
/// `complete: false` means the capture was bounded — an exclusion, a skipped
/// entry, a scope the snapshotter could not walk — and the manifest is not a
/// full picture of the workspace. A caller may still use it; it may not claim
/// the workspace is recoverable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub snapshot_id: SnapshotId,
    pub session_id: String,
    pub task_id: String,
    /// The action this snapshot was taken for, by the digest the decision is
    /// recorded under.
    pub action_digest: String,
    pub scope: WorkspaceSnapshotScope,
    /// Sorted by `relative_path`, so the digest does not depend on walk order.
    pub entries: Vec<SnapshotEntry>,
    /// What the snapshot does not cover, in the operator's words.
    #[serde(default)]
    pub unrecoverable_scope: Vec<String>,
    /// Digest over the fields above that decide what a restore would do.
    pub manifest_hash: String,
    pub total_bytes: u64,
    pub complete: bool,
    pub created_at: DateTime<Utc>,
}

impl SnapshotManifest {
    /// The digest over the fields a restore acts on.
    ///
    /// One producer, so the digest written at capture and the one recomputed at
    /// restore cannot be computed from different fields.
    pub fn digest_scope(scope: &WorkspaceSnapshotScope, entries: &[SnapshotEntry]) -> String {
        digest::json(&serde_json::json!({
            "scope": scope,
            "entries": entries,
        }))
    }
}

/// How a path differs from what the snapshot recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotChangeKind {
    /// Nothing was there at capture; something is now.
    Added,
    /// Bytes differ from what was captured.
    Modified,
    /// Captured, and gone now.
    Removed,
}

/// One entry of a change manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChange {
    pub relative_path: String,
    pub kind: SnapshotChangeKind,
    pub before_size: u64,
    pub after_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sha256: Option<String>,
}

/// What a call changed, as compared against the snapshot taken before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeManifest {
    pub snapshot_id: SnapshotId,
    pub session_id: String,
    pub task_id: String,
    pub action_digest: String,
    pub changes: Vec<SnapshotChange>,
    pub changes_hash: String,
    /// `false` when the comparison was bounded — the diff could not read every
    /// path in scope — and the list is therefore not the whole truth.
    pub complete: bool,
    pub created_at: DateTime<Utc>,
}

impl ChangeManifest {
    /// The digest over the change list.
    pub fn digest_changes(changes: &[SnapshotChange]) -> String {
        digest::json(&serde_json::json!({ "changes": changes }))
    }
}

/// One path a rollback did not put back, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackSkip {
    pub relative_path: String,
    pub reason: String,
}

/// What one rollback did.
///
/// `complete` is the promise: `true` means every entry in the manifest was
/// handled and nothing was skipped. A person reading "restored 12" without it
/// would have to check the log to learn whether twelve was all of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackReport {
    pub snapshot_id: SnapshotId,
    /// Paths whose bytes were put back.
    pub restored: Vec<String>,
    /// Paths removed because they did not exist at capture time.
    pub removed: Vec<String>,
    /// Paths the rollback refused to touch.
    #[serde(default)]
    pub skipped: Vec<RollbackSkip>,
    /// Repeats the manifest's exclusions: what a rollback could not cover.
    #[serde(default)]
    pub unrecoverable_scope: Vec<String>,
    pub complete: bool,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> SnapshotEntry {
        SnapshotEntry {
            relative_path: path.to_string(),
            kind: SnapshotEntryKind::File,
            size,
            sha256: Some(format!("{:064x}", size)),
            artifact: None,
        }
    }

    #[test]
    fn the_snapshot_digest_covers_scope_and_entries_and_nothing_else() {
        let scope = WorkspaceSnapshotScope::Paths { paths: vec!["a.txt".to_string()] };
        let entries = vec![entry("a.txt", 3)];
        let first = SnapshotManifest::digest_scope(&scope, &entries);

        let other = WorkspaceSnapshotScope::Paths { paths: vec!["b.txt".to_string()] };
        assert_ne!(first, SnapshotManifest::digest_scope(&other, &entries));
        assert_ne!(first, SnapshotManifest::digest_scope(&scope, &[entry("a.txt", 4)]));
        assert_eq!(first, SnapshotManifest::digest_scope(&scope, &entries));
    }

    #[test]
    fn a_manifest_survives_a_round_trip_through_json() {
        let manifest = SnapshotManifest {
            snapshot_id: SnapshotId::new(),
            session_id: "s".to_string(),
            task_id: "manual:s".to_string(),
            action_digest: "d".to_string(),
            scope: WorkspaceSnapshotScope::WritableRoots { roots: vec![".".to_string()] },
            entries: vec![entry("a.txt", 1)],
            unrecoverable_scope: vec![".agent-data".to_string()],
            manifest_hash: "h".to_string(),
            total_bytes: 1,
            complete: true,
            created_at: Utc::now(),
        };
        let text = serde_json::to_string(&manifest).expect("serializes");
        let back: SnapshotManifest = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(back, manifest);
        assert_eq!(
            SnapshotManifest::digest_scope(&back.scope, &back.entries),
            SnapshotManifest::digest_scope(&manifest.scope, &manifest.entries)
        );
    }

    #[test]
    fn the_change_digest_changes_with_the_list_and_not_with_its_order_of_construction() {
        let one = SnapshotChange {
            relative_path: "a.txt".to_string(),
            kind: SnapshotChangeKind::Modified,
            before_size: 1,
            after_size: 2,
            before_sha256: None,
            after_sha256: None,
        };
        let two = SnapshotChange { relative_path: "b.txt".to_string(), ..one.clone() };
        assert_eq!(
            ChangeManifest::digest_changes(&[one.clone(), two.clone()]),
            ChangeManifest::digest_changes(&[one.clone(), two.clone()])
        );
        assert_ne!(
            ChangeManifest::digest_changes(std::slice::from_ref(&one)),
            ChangeManifest::digest_changes(&[two])
        );
    }
}
