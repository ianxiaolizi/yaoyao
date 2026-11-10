//! The filesystem half of the recovery point: copy the bytes, compare the
//! workspace, put it back.
//!
//! A manifest is a description; this adapter is what makes it true. Three
//! operations, one rule between them: **a snapshot that cannot be complete is
//! refused**. A capture that quietly skipped a directory would make a manifest
//! that reads like a promise the rollback cannot keep, and a rollback that
//! quietly skipped a path would make "restored" a word nobody can rely on.
//!
//! File contents live in the artifact store, addressed by their SHA-256, so a
//! snapshot costs what its unique bytes cost and a restore can verify that what
//! it is about to write is what was captured. The manifest itself is stored the
//! same way and located through a small index file, one per snapshot, so that
//! `agent rollback <snapshot_id>` can answer from the id alone.
//!
//! What this adapter deliberately does **not** do: decide whether a call is
//! worth a snapshot (the permission chain decides), record events (the kernel
//! writes them, so the log has one writer per fact), or follow a reparse point.
//! A junction or a symlink anywhere in scope is a refusal — that is the
//! boundary the workspace guard enforces at write time, and a snapshot that
//! wandered through it would capture files the guard would never let the call
//! touch.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent_ports::artifact_store::{ArtifactStore, ArtifactStoreError};
use agent_ports::workspace_snapshot::{SnapshotError, SnapshotRequest, WorkspaceSnapshotter};
use agent_protocol::ids::SnapshotId;
use agent_protocol::permissions::ArtifactRef;
use agent_protocol::workspace_snapshot::{
    ChangeManifest, RollbackReport, RollbackSkip, SnapshotChange, SnapshotChangeKind,
    SnapshotEntry, SnapshotEntryKind, SnapshotManifest, UNRECOVERABLE_ALWAYS,
    WorkspaceSnapshotScope,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How large a recovery point may get before it is refused.
///
/// A budget rather than a target: the point of the number is to fail *before* a
/// copy of the whole workspace turns one write into a disk-full incident. A
/// caller that genuinely needs more raises the limit where the snapshotter is
/// built, and the raise is visible in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self { max_entries: 4096, max_total_bytes: 64 * 1024 * 1024 }
    }
}

/// A snapshotter over one workspace root, storing blobs in an artifact store.
pub struct FsWorkspaceSnapshotter {
    workspace_root: PathBuf,
    artifacts: Arc<dyn ArtifactStore>,
    index_dir: PathBuf,
    limits: SnapshotLimits,
}

/// Where a snapshot's manifest lives, by id.
///
/// Deliberately small: the index names the manifest artifact and the digest it
/// was recorded under, and nothing else. A restore that reads a different
/// manifest than the one this names — because the file was edited, replaced or
/// truncated — is refused before a single byte reaches the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotIndex {
    snapshot_id: String,
    manifest_hash: String,
    manifest_artifact: ArtifactRef,
    session_id: String,
}

impl FsWorkspaceSnapshotter {
    pub fn new(
        workspace_root: impl Into<PathBuf>,
        artifacts: Arc<dyn ArtifactStore>,
        index_dir: impl Into<PathBuf>,
    ) -> Self {
        let root: PathBuf = workspace_root.into();
        Self {
            workspace_root: std::fs::canonicalize(&root).unwrap_or(root),
            artifacts,
            index_dir: index_dir.into(),
            limits: SnapshotLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: SnapshotLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The paths a scope names, plus the ones it excludes.
    ///
    /// A root scope excludes what [`UNRECOVERABLE_ALWAYS`] names and says so:
    /// the exclusion is written into the manifest rather than left for a reader
    /// to infer from what is absent, because "absent" is also what a failed
    /// capture looks like.
    fn targets(
        &self,
        scope: &WorkspaceSnapshotScope,
        unrecoverable: &mut Vec<String>,
    ) -> Result<Vec<PathBuf>, SnapshotError> {
        match scope {
            WorkspaceSnapshotScope::Paths { paths } => Ok(paths
                .iter()
                .map(|path| self.anchor(path))
                .filter(|path| {
                    // Checked before anything canonicalizes: canonicalizing a
                    // link answers where it points, and where a junction points
                    // is exactly the question this refuses to ask.
                    if is_reparse_point(path) {
                        return true; // refused by `capture`, which can say why
                    }
                    let inside = self.inside_root(path);
                    if !inside {
                        unrecoverable.push(format!("{}: outside the workspace", path.display()));
                    }
                    inside
                })
                .collect()),
            WorkspaceSnapshotScope::WritableRoots { roots } => {
                for name in UNRECOVERABLE_ALWAYS {
                    unrecoverable.push(format!("{name}: excluded from every snapshot"));
                }
                Ok(roots.iter().map(|root| self.anchor(root)).collect())
            }
        }
    }

    fn anchor(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() { path.to_path_buf() } else { self.workspace_root.join(path) }
    }

    fn inside_root(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        canonical.starts_with(&self.workspace_root)
    }

    /// The workspace-relative spelling a manifest entry uses.
    fn relative_key(&self, path: &Path) -> Result<String, SnapshotError> {
        let relative =
            path.strip_prefix(&self.workspace_root).map_err(|_| SnapshotError::Escaped {
                path: path.display().to_string(),
                reason: "it is outside the workspace root".to_string(),
            })?;
        Ok(relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"))
    }

    /// Store one file and return its entry.
    fn capture_file(
        &self,
        path: &Path,
        entries: &mut Vec<SnapshotEntry>,
        total_bytes: &mut u64,
    ) -> Result<(), SnapshotError> {
        if is_reparse_point(path) {
            return Err(SnapshotError::ReparsePoint { path: path.display().to_string() });
        }
        let bytes = std::fs::read(path).map_err(|error| SnapshotError::Io {
            path: path.display().to_string(),
            reason: error.to_string(),
        })?;
        let size = bytes.len() as u64;
        if entries.len() >= self.limits.max_entries {
            return Err(SnapshotError::OverBudget {
                reason: format!(
                    "more than {} entries under '{}'",
                    self.limits.max_entries,
                    self.workspace_root.display()
                ),
            });
        }
        if (*total_bytes).saturating_add(size) > self.limits.max_total_bytes {
            return Err(SnapshotError::OverBudget {
                reason: format!("more than {} bytes", self.limits.max_total_bytes),
            });
        }
        let artifact = self
            .artifacts
            .put(&bytes, "application/octet-stream")
            .map_err(|error| SnapshotError::Unwritable { reason: error.to_string() })?;
        *total_bytes += size;
        entries.push(SnapshotEntry {
            relative_path: self.relative_key(path)?,
            kind: SnapshotEntryKind::File,
            size,
            sha256: Some(artifact.hash.clone()),
            artifact: Some(artifact),
        });
        Ok(())
    }

    /// Every file under `root`, refusing what cannot be captured faithfully.
    fn walk(
        &self,
        root: &Path,
        entries: &mut Vec<SnapshotEntry>,
        total_bytes: &mut u64,
    ) -> Result<(), SnapshotError> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(next) = stack.pop() {
            let metadata = match std::fs::symlink_metadata(&next) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(SnapshotError::Io {
                        path: next.display().to_string(),
                        reason: error.to_string(),
                    });
                }
            };
            if is_reparse_point(&next) {
                return Err(SnapshotError::ReparsePoint { path: next.display().to_string() });
            }
            if entries.len() >= self.limits.max_entries && metadata.is_dir() {
                return Err(SnapshotError::OverBudget {
                    reason: format!("more than {} entries", self.limits.max_entries),
                });
            }
            if metadata.is_dir() {
                let listing = std::fs::read_dir(&next).map_err(|error| SnapshotError::Io {
                    path: next.display().to_string(),
                    reason: error.to_string(),
                })?;
                for entry in listing {
                    let entry = entry.map_err(|error| SnapshotError::Io {
                        path: next.display().to_string(),
                        reason: error.to_string(),
                    })?;
                    let name = entry.file_name();
                    if UNRECOVERABLE_ALWAYS.iter().any(|excluded| name == *excluded) {
                        continue;
                    }
                    stack.push(entry.path());
                }
                continue;
            }
            self.capture_file(&next, entries, total_bytes)?;
        }
        Ok(())
    }

    fn index_path(&self, snapshot_id: &str) -> PathBuf {
        self.index_dir.join(format!("{snapshot_id}.json"))
    }

    fn write_index(&self, index: &SnapshotIndex) -> Result<(), SnapshotError> {
        std::fs::create_dir_all(&self.index_dir).map_err(|error| SnapshotError::Unwritable {
            reason: format!("cannot create {}: {error}", self.index_dir.display()),
        })?;
        let bytes = serde_json::to_vec_pretty(index)
            .map_err(|error| SnapshotError::Unwritable { reason: error.to_string() })?;
        std::fs::write(self.index_path(&index.snapshot_id), bytes)
            .map_err(|error| SnapshotError::Unwritable { reason: error.to_string() })
    }

    fn read_manifest(&self, snapshot_id: &str) -> Result<SnapshotManifest, SnapshotError> {
        let path = self.index_path(snapshot_id);
        let text = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                SnapshotError::NotFound { snapshot_id: snapshot_id.to_string() }
            } else {
                SnapshotError::Io { path: path.display().to_string(), reason: error.to_string() }
            }
        })?;
        let index: SnapshotIndex =
            serde_json::from_slice(&text).map_err(|error| SnapshotError::CorruptManifest {
                snapshot_id: snapshot_id.to_string(),
                reason: format!("its index is not readable: {error}"),
            })?;
        if index.snapshot_id != snapshot_id {
            return Err(SnapshotError::CorruptManifest {
                snapshot_id: snapshot_id.to_string(),
                reason: format!("its index names snapshot '{}'", index.snapshot_id),
            });
        }
        let bytes = self.artifacts.get(&index.manifest_artifact).map_err(|error| {
            let reason = match error {
                ArtifactStoreError::NotFound(_) => {
                    "the stored manifest artifact is gone".to_string()
                }
                ArtifactStoreError::ContentMismatch { expected, actual } => format!(
                    "the stored manifest does not match its content ({expected} vs {actual})"
                ),
                other => other.to_string(),
            };
            SnapshotError::CorruptManifest { snapshot_id: snapshot_id.to_string(), reason }
        })?;
        if sha256_hex(&bytes) != index.manifest_artifact.hash {
            return Err(SnapshotError::CorruptManifest {
                snapshot_id: snapshot_id.to_string(),
                reason: "the stored manifest no longer hashes to the reference in its index"
                    .to_string(),
            });
        }
        let manifest: SnapshotManifest =
            serde_json::from_slice(&bytes).map_err(|error| SnapshotError::CorruptManifest {
                snapshot_id: snapshot_id.to_string(),
                reason: format!("its manifest is not readable: {error}"),
            })?;
        if manifest.manifest_hash != index.manifest_hash
            || manifest.manifest_hash
                != SnapshotManifest::digest_scope(&manifest.scope, &manifest.entries)
        {
            return Err(SnapshotError::CorruptManifest {
                snapshot_id: snapshot_id.to_string(),
                reason: "its manifest does not match the digest it was recorded under".to_string(),
            });
        }
        Ok(manifest)
    }

    /// The destination a manifest entry names, once the workspace admits it.
    ///
    /// Refused rather than repaired: a path that is absolute, climbs out with
    /// `..`, or whose ancestors now resolve outside the root (a directory
    /// replaced by a junction after the snapshot) is exactly the case a
    /// rollback must not write through.
    fn destination(&self, relative: &str) -> Result<PathBuf, String> {
        let path = Path::new(relative);
        if path.is_absolute() {
            return Err("the manifest names an absolute path".to_string());
        }
        if path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        }) {
            return Err("the path climbs out of the workspace".to_string());
        }
        let mut current = self.workspace_root.clone();
        for component in path.components() {
            current.push(component);
            if !current.exists() {
                continue;
            }
            if is_reparse_point(&current) {
                return Err(format!("'{}' is a reparse point now", current.display()));
            }
            let canonical = std::fs::canonicalize(&current).map_err(|error| error.to_string())?;
            if !canonical.starts_with(&self.workspace_root) {
                return Err(format!("'{}' resolves outside the workspace now", current.display()));
            }
        }
        Ok(self.workspace_root.join(path))
    }

    fn read_entry_bytes(&self, entry: &SnapshotEntry) -> Result<Vec<u8>, SnapshotError> {
        let artifact = entry.artifact.clone().ok_or_else(|| SnapshotError::CorruptManifest {
            snapshot_id: String::new(),
            reason: format!("entry '{}' names no artifact", entry.relative_path),
        })?;
        let bytes =
            self.artifacts.get(&artifact).map_err(|error| SnapshotError::CorruptManifest {
                snapshot_id: String::new(),
                reason: format!("artifact for '{}' is unreadable: {error}", entry.relative_path),
            })?;
        match &entry.sha256 {
            Some(expected) if *expected != sha256_hex(&bytes) => {
                Err(SnapshotError::CorruptManifest {
                    snapshot_id: String::new(),
                    reason: format!(
                        "artifact for '{}' does not match the entry's digest",
                        entry.relative_path
                    ),
                })
            }
            _ => Ok(bytes),
        }
    }
}

#[async_trait::async_trait]
impl WorkspaceSnapshotter for FsWorkspaceSnapshotter {
    async fn capture(&self, request: SnapshotRequest) -> Result<SnapshotManifest, SnapshotError> {
        let mut unrecoverable = Vec::new();
        let targets = self.targets(&request.scope, &mut unrecoverable)?;
        let mut entries = Vec::new();
        let mut total_bytes = 0u64;
        for target in targets {
            match std::fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() => {
                    self.walk(&target, &mut entries, &mut total_bytes)?;
                }
                Ok(_) => self.capture_file(&target, &mut entries, &mut total_bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Captured as absence: a rollback of this call removes what
                    // the call creates there.
                    entries.push(SnapshotEntry {
                        relative_path: self.relative_key(&target)?,
                        kind: SnapshotEntryKind::Missing,
                        size: 0,
                        sha256: None,
                        artifact: None,
                    });
                }
                Err(error) => {
                    return Err(SnapshotError::Io {
                        path: target.display().to_string(),
                        reason: error.to_string(),
                    });
                }
            }
        }
        entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        unrecoverable.sort();
        unrecoverable.dedup();

        let manifest = SnapshotManifest {
            snapshot_id: SnapshotId::new(),
            session_id: request.session_id,
            task_id: request.task_id,
            action_digest: request.action_digest,
            manifest_hash: SnapshotManifest::digest_scope(&request.scope, &entries),
            scope: request.scope,
            entries,
            unrecoverable_scope: unrecoverable.clone(),
            total_bytes,
            complete: unrecoverable.is_empty(),
            created_at: Utc::now(),
        };
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|error| SnapshotError::Unwritable { reason: error.to_string() })?;
        let artifact = self
            .artifacts
            .put(&bytes, "application/json")
            .map_err(|error| SnapshotError::Unwritable { reason: error.to_string() })?;
        self.write_index(&SnapshotIndex {
            snapshot_id: manifest.snapshot_id.to_string(),
            manifest_hash: manifest.manifest_hash.clone(),
            manifest_artifact: artifact,
            session_id: manifest.session_id.clone(),
        })?;
        Ok(manifest)
    }

    async fn diff(&self, manifest: &SnapshotManifest) -> Result<ChangeManifest, SnapshotError> {
        let mut changes = Vec::new();
        let mut complete = true;
        let mut recorded: std::collections::BTreeSet<String> = Default::default();
        for entry in &manifest.entries {
            recorded.insert(entry.relative_path.clone());
            let path = self.workspace_root.join(&entry.relative_path);
            let current = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let after_sha256 = current.as_ref().map(|bytes| sha256_hex(bytes));
            let after_size = current.as_ref().map(|bytes| bytes.len() as u64).unwrap_or(0);
            match (entry.kind, current.as_ref()) {
                (SnapshotEntryKind::File, None) => changes.push(SnapshotChange {
                    relative_path: entry.relative_path.clone(),
                    kind: SnapshotChangeKind::Removed,
                    before_size: entry.size,
                    after_size: 0,
                    before_sha256: entry.sha256.clone(),
                    after_sha256: None,
                }),
                (SnapshotEntryKind::Missing, Some(_)) => changes.push(SnapshotChange {
                    relative_path: entry.relative_path.clone(),
                    kind: SnapshotChangeKind::Added,
                    before_size: 0,
                    after_size,
                    before_sha256: None,
                    after_sha256,
                }),
                (SnapshotEntryKind::File, Some(_)) if after_sha256 != entry.sha256 => {
                    changes.push(SnapshotChange {
                        relative_path: entry.relative_path.clone(),
                        kind: SnapshotChangeKind::Modified,
                        before_size: entry.size,
                        after_size,
                        before_sha256: entry.sha256.clone(),
                        after_sha256,
                    })
                }
                _ => {}
            }
        }
        // A root scope can also gain files the capture never saw. They are part
        // of "what this call changed", and a rollback removes them.
        if let WorkspaceSnapshotScope::WritableRoots { roots } = &manifest.scope {
            let mut found = Vec::new();
            let mut total = 0u64;
            for root in roots {
                if self.walk(&self.anchor(root), &mut found, &mut total).is_err() {
                    complete = false;
                }
            }
            for entry in found {
                if recorded.contains(&entry.relative_path) {
                    continue;
                }
                changes.push(SnapshotChange {
                    relative_path: entry.relative_path,
                    kind: SnapshotChangeKind::Added,
                    before_size: 0,
                    after_size: entry.size,
                    before_sha256: None,
                    after_sha256: entry.sha256,
                });
            }
        }
        changes.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        Ok(ChangeManifest {
            snapshot_id: manifest.snapshot_id.clone(),
            session_id: manifest.session_id.clone(),
            task_id: manifest.task_id.clone(),
            action_digest: manifest.action_digest.clone(),
            changes_hash: ChangeManifest::digest_changes(&changes),
            changes,
            complete,
            created_at: Utc::now(),
        })
    }

    async fn restore(&self, snapshot_id: &str) -> Result<RollbackReport, SnapshotError> {
        let manifest = self.read_manifest(snapshot_id)?;
        let mut report = RollbackReport {
            snapshot_id: manifest.snapshot_id.clone(),
            restored: Vec::new(),
            removed: Vec::new(),
            skipped: Vec::new(),
            unrecoverable_scope: manifest.unrecoverable_scope.clone(),
            complete: true,
            created_at: Utc::now(),
        };
        let mut recorded: std::collections::BTreeSet<String> = Default::default();

        for entry in &manifest.entries {
            recorded.insert(entry.relative_path.clone());
            let destination = match self.destination(&entry.relative_path) {
                Ok(destination) => destination,
                Err(reason) => {
                    report
                        .skipped
                        .push(RollbackSkip { relative_path: entry.relative_path.clone(), reason });
                    continue;
                }
            };
            match entry.kind {
                SnapshotEntryKind::File => {
                    let bytes = self.read_entry_bytes(entry).map_err(|error| match error {
                        SnapshotError::CorruptManifest { reason, .. } => {
                            SnapshotError::CorruptManifest {
                                snapshot_id: snapshot_id.to_string(),
                                reason,
                            }
                        }
                        other => other,
                    })?;
                    let current = std::fs::read(&destination).ok();
                    if current.as_deref() == Some(bytes.as_slice()) {
                        // Already in the captured state: a second rollback is a
                        // no-op, not a rewrite.
                        continue;
                    }
                    if let Some(parent) = destination.parent() {
                        // A parent created between snapshot and restore is
                        // inside the root: the loop above admitted it.
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&destination, &bytes).map_err(|error| SnapshotError::Io {
                        path: destination.display().to_string(),
                        reason: error.to_string(),
                    })?;
                    report.restored.push(entry.relative_path.clone());
                }
                SnapshotEntryKind::Missing => {
                    if destination.exists() {
                        remove_path(&destination).map_err(|error| SnapshotError::Io {
                            path: destination.display().to_string(),
                            reason: error.to_string(),
                        })?;
                        report.removed.push(entry.relative_path.clone());
                    }
                }
            }
        }

        // Files a call created inside a rooted scope are not in the manifest;
        // "removed" is the other half of putting the workspace back.
        if let WorkspaceSnapshotScope::WritableRoots { roots } = &manifest.scope {
            let mut found = Vec::new();
            let mut total = 0u64;
            for root in roots {
                if self.walk(&self.anchor(root), &mut found, &mut total).is_err() {
                    report.complete = false;
                }
            }
            for entry in found {
                if entry.kind != SnapshotEntryKind::File || recorded.contains(&entry.relative_path)
                {
                    continue;
                }
                let destination = match self.destination(&entry.relative_path) {
                    Ok(destination) => destination,
                    Err(reason) => {
                        report.skipped.push(RollbackSkip {
                            relative_path: entry.relative_path.clone(),
                            reason,
                        });
                        continue;
                    }
                };
                remove_path(&destination).map_err(|error| SnapshotError::Io {
                    path: destination.display().to_string(),
                    reason: error.to_string(),
                })?;
                report.removed.push(entry.relative_path);
            }
        }

        report.removed.sort();
        report.complete = report.complete
            && report.skipped.is_empty()
            && manifest.complete
            && manifest.unrecoverable_scope.is_empty();
        Ok(report)
    }
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        // Never follow a link out: remove the link itself.
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::remove_file(path),
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether `path` is a symlink, a junction or another reparse point.
///
/// One answer for both platforms where it matters: on Windows a junction is a
/// reparse point the ordinary `is_symlink` answer does not always cover, and a
/// directory replaced by one after a snapshot is exactly how a restore gets
/// talked into writing outside the workspace.
pub(crate) fn is_reparse_point(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}
