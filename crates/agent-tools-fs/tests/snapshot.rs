//! Behaviour of the recovery point: capture, compare, restore.
//!
//! Every test drives the adapter through its port, not through its helpers —
//! what is asserted is what a caller of [`WorkspaceSnapshotter`] would see.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_ports::artifact_store::{ArtifactStore, ArtifactStoreError};
use agent_ports::workspace_snapshot::{SnapshotError, SnapshotRequest, WorkspaceSnapshotter};
use agent_protocol::permissions::ArtifactRef;
use agent_protocol::workspace_snapshot::{
    ChangeManifest, SnapshotChangeKind, SnapshotManifest, WorkspaceSnapshotScope,
};
use agent_tools_fs::{FsWorkspaceSnapshotter, SnapshotLimits};
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An artifact store that keeps blobs in memory, with the real store's
/// content-addressing answer: the name is the SHA-256, and a blob whose bytes
/// no longer hash to its name is refused.
#[derive(Default)]
struct MemoryArtifacts {
    blobs: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryArtifacts {
    fn corrupt(&self, hash: &str) {
        self.blobs.lock().unwrap().insert(hash.to_string(), b"tampered".to_vec());
    }
}

impl ArtifactStore for MemoryArtifacts {
    fn put(&self, bytes: &[u8], mime_type: &str) -> Result<ArtifactRef, ArtifactStoreError> {
        let hash = sha256_hex(bytes);
        self.blobs.lock().unwrap().insert(hash.clone(), bytes.to_vec());
        Ok(ArtifactRef { hash, mime_type: mime_type.to_string(), size: bytes.len() as u64 })
    }

    fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>, ArtifactStoreError> {
        let blobs = self.blobs.lock().unwrap();
        let Some(bytes) = blobs.get(&artifact.hash) else {
            return Err(ArtifactStoreError::NotFound(artifact.hash.clone()));
        };
        let actual = sha256_hex(bytes);
        if actual != artifact.hash {
            return Err(ArtifactStoreError::ContentMismatch {
                expected: artifact.hash.clone(),
                actual,
            });
        }
        Ok(bytes.clone())
    }
}

struct Harness {
    workspace: tempfile::TempDir,
    index: tempfile::TempDir,
    artifacts: Arc<MemoryArtifacts>,
    snapshots: FsWorkspaceSnapshotter,
}

fn harness() -> Harness {
    let workspace = tempfile::tempdir().unwrap();
    let index = tempfile::tempdir().unwrap();
    let artifacts = Arc::new(MemoryArtifacts::default());
    let snapshots = FsWorkspaceSnapshotter::new(
        workspace.path(),
        artifacts.clone() as Arc<dyn ArtifactStore>,
        index.path(),
    );
    Harness { workspace, index, artifacts, snapshots }
}

impl Harness {
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.workspace.path().join(name)
    }

    fn index_path(&self, snapshot_id: &str) -> std::path::PathBuf {
        self.index.path().join(format!("{snapshot_id}.json"))
    }
}

fn request(scope: WorkspaceSnapshotScope) -> SnapshotRequest {
    SnapshotRequest {
        session_id: "s".to_string(),
        task_id: "manual:s".to_string(),
        action_digest: "d".to_string(),
        scope,
    }
}

fn paths(paths: &[&str]) -> WorkspaceSnapshotScope {
    WorkspaceSnapshotScope::Paths { paths: paths.iter().map(|p| p.to_string()).collect() }
}

fn rooted() -> WorkspaceSnapshotScope {
    WorkspaceSnapshotScope::WritableRoots { roots: vec![".".to_string()] }
}

async fn capture(
    snapshots: &FsWorkspaceSnapshotter,
    scope: WorkspaceSnapshotScope,
) -> SnapshotManifest {
    snapshots.capture(request(scope)).await.expect("capture succeeds")
}

#[cfg(unix)]
fn link_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn link_file(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[tokio::test]
async fn a_changed_file_rolls_back_to_its_exact_bytes() {
    let h = harness();
    std::fs::write(h.path("a.txt"), "one").unwrap();
    let manifest = capture(&h.snapshots, paths(&["a.txt"])).await;
    std::fs::write(h.path("a.txt"), "two, longer").unwrap();

    let report = h.snapshots.restore(&manifest.snapshot_id.to_string()).await.unwrap();
    assert_eq!(std::fs::read(h.path("a.txt")).unwrap(), b"one");
    assert_eq!(report.restored, vec!["a.txt".to_string()]);
    assert!(report.removed.is_empty());
    assert!(report.complete);
}

#[tokio::test]
async fn an_added_file_is_removed_on_rollback() {
    let h = harness();
    std::fs::write(h.path("keep.txt"), "keep").unwrap();
    let manifest = capture(&h.snapshots, rooted()).await;
    std::fs::write(h.path("new.txt"), "new").unwrap();

    let report = h.snapshots.restore(&manifest.snapshot_id.to_string()).await.unwrap();
    assert!(!h.path("new.txt").exists());
    assert_eq!(std::fs::read(h.path("keep.txt")).unwrap(), b"keep");
    assert_eq!(report.removed, vec!["new.txt".to_string()]);
}

#[tokio::test]
async fn a_deleted_file_is_restored() {
    let h = harness();
    std::fs::write(h.path("a.txt"), "one").unwrap();
    let manifest = capture(&h.snapshots, paths(&["a.txt"])).await;
    std::fs::remove_file(h.path("a.txt")).unwrap();

    let report = h.snapshots.restore(&manifest.snapshot_id.to_string()).await.unwrap();
    assert_eq!(std::fs::read(h.path("a.txt")).unwrap(), b"one");
    assert_eq!(report.restored, vec!["a.txt".to_string()]);
}

#[tokio::test]
async fn a_snapshot_cannot_include_a_reparse_point() {
    let h = harness();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    let link = h.path("link.txt");
    link_file(&outside.path().join("secret.txt"), &link)
        .expect("this test needs to create a symlink; run it where one can be created");

    let refused = h.snapshots.capture(request(paths(&["link.txt"]))).await;
    assert!(
        matches!(refused, Err(SnapshotError::ReparsePoint { .. })),
        "a symlink was captured: {refused:?}"
    );
}

#[tokio::test]
async fn a_snapshot_over_budget_is_refused_rather_than_truncated() {
    let h = harness();
    std::fs::write(h.path("a.txt"), "one").unwrap();
    std::fs::write(h.path("b.txt"), "two").unwrap();
    let snapshots = FsWorkspaceSnapshotter::new(
        h.workspace.path(),
        h.artifacts.clone() as Arc<dyn ArtifactStore>,
        h.index.path(),
    )
    .with_limits(SnapshotLimits { max_entries: 1, max_total_bytes: 1024 });

    let refused = snapshots.capture(request(rooted())).await;
    assert!(matches!(refused, Err(SnapshotError::OverBudget { .. })), "{refused:?}");
}

#[tokio::test]
async fn a_corrupt_snapshot_manifest_cannot_restore() {
    let h = harness();
    std::fs::write(h.path("a.txt"), "one").unwrap();
    let manifest = capture(&h.snapshots, paths(&["a.txt"])).await;
    // The manifest artifact is content-addressed: replacing its bytes is what a
    // damaged store looks like from the outside.
    let text = std::fs::read(h.index_path(&manifest.snapshot_id.to_string())).unwrap();
    let index: serde_json::Value = serde_json::from_slice(&text).unwrap();
    let hash = index["manifest_artifact"]["hash"].as_str().expect("index names a manifest");
    h.artifacts.corrupt(hash);

    let refused = h.snapshots.restore(&manifest.snapshot_id.to_string()).await;
    assert!(matches!(refused, Err(SnapshotError::CorruptManifest { .. })), "{refused:?}");
    assert_eq!(std::fs::read(h.path("a.txt")).unwrap(), b"one");
}

#[tokio::test]
async fn rollback_refuses_a_path_that_escaped_after_freeze() {
    let h = harness();
    std::fs::write(h.path("a.txt"), "one").unwrap();
    let manifest = capture(&h.snapshots, paths(&["a.txt"])).await;
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("outside.txt");
    std::fs::write(&outside_file, "outside").unwrap();
    std::fs::remove_file(h.path("a.txt")).unwrap();
    link_file(&outside_file, &h.path("a.txt")).expect("this test needs to create a symlink");

    let report = h.snapshots.restore(&manifest.snapshot_id.to_string()).await.unwrap();
    assert_eq!(std::fs::read(&outside_file).unwrap(), b"outside");
    assert!(report.restored.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(!report.complete);
}

#[tokio::test]
async fn a_rollback_is_idempotent() {
    let h = harness();
    std::fs::write(h.path("a.txt"), "one").unwrap();
    let manifest = capture(&h.snapshots, paths(&["a.txt"])).await;
    std::fs::write(h.path("a.txt"), "two").unwrap();

    let first = h.snapshots.restore(&manifest.snapshot_id.to_string()).await.unwrap();
    let second = h.snapshots.restore(&manifest.snapshot_id.to_string()).await.unwrap();
    assert_eq!(std::fs::read(h.path("a.txt")).unwrap(), b"one");
    assert_eq!(first.restored, vec!["a.txt".to_string()]);
    assert!(second.restored.is_empty());
    assert!(second.complete);
}

#[tokio::test]
async fn a_change_is_compared_against_the_snapshot_not_remembered() {
    let h = harness();
    std::fs::write(h.path("a.txt"), "one").unwrap();
    std::fs::write(h.path("gone.txt"), "gone").unwrap();
    let manifest = capture(&h.snapshots, rooted()).await;
    std::fs::write(h.path("a.txt"), "two").unwrap();
    std::fs::remove_file(h.path("gone.txt")).unwrap();
    std::fs::write(h.path("new.txt"), "new").unwrap();

    let changes: ChangeManifest = h.snapshots.diff(&manifest).await.unwrap();
    let kinds: Vec<(String, SnapshotChangeKind)> =
        changes.changes.iter().map(|change| (change.relative_path.clone(), change.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            ("a.txt".to_string(), SnapshotChangeKind::Modified),
            ("gone.txt".to_string(), SnapshotChangeKind::Removed),
            ("new.txt".to_string(), SnapshotChangeKind::Added),
        ]
    );
    assert!(changes.complete);
}

#[tokio::test]
async fn the_data_directory_is_excluded_and_said_so() {
    let h = harness();
    std::fs::create_dir_all(h.path(".agent-data")).unwrap();
    std::fs::write(h.path(".agent-data/log.jsonl"), "{}").unwrap();
    std::fs::write(h.path("a.txt"), "one").unwrap();

    let manifest = capture(&h.snapshots, rooted()).await;
    assert!(manifest.entries.iter().all(|entry| !entry.relative_path.starts_with(".agent-data")));
    assert!(manifest.unrecoverable_scope.iter().any(|item| item.starts_with(".agent-data")));
    assert!(!manifest.complete, "an excluded scope is not a complete snapshot");
}
