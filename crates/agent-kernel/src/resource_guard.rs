//! ResourceGuard — file path boundary enforcement.
//!
//! Reads and writes are confined to the workspace roots. A path is canonicalized
//! (resolving `.`/`..` and symlinks) before comparison, so a `..` escape or a
//! symlink pointing outside the workspace is rejected. Paths outside the
//! workspace are only reachable through explicitly configured external
//! directories, and writes are additionally confined to the writable roots.
//!
//! A relative path is relative to the **workspace root**, not to the process's
//! working directory. The tool layer already opens files that way; the check has
//! to agree with it, or a path can pass here and fail at the filesystem.

use std::path::{Path, PathBuf};

use agent_protocol::permissions::Workspace;

/// Enforces the file boundary described by a [`Workspace`].
#[derive(Debug, Clone)]
pub struct ResourceGuard {
    workspace_root: PathBuf,
    writable_roots: Vec<PathBuf>,
    external_directories: Vec<PathBuf>,
}

impl ResourceGuard {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace_root: canonicalize_or_raw(&workspace.workspace_root),
            writable_roots: workspace
                .writable_roots
                .iter()
                .map(|r| canonicalize_or_raw(r))
                .collect(),
            external_directories: workspace
                .external_directories
                .iter()
                .map(|r| canonicalize_or_raw(r))
                .collect(),
        }
    }

    /// Whether `path` is inside the workspace (read or write), rejecting paths
    /// outside it and writes outside the writable roots.
    ///
    /// A relative `path` is taken relative to the workspace root, which is what
    /// the caller that will actually open the file does. Resolving it against
    /// the process cwd instead would make the answer depend on where the agent
    /// binary happened to be launched from.
    pub fn check(&self, path: &str, write: bool) -> Result<(), ResourceGuardError> {
        let canonical = canonicalize_for_check(&self.anchor(path))?;

        if is_within(&canonical, &self.workspace_root) {
            if write && !self.writable_allows(&canonical) {
                return Err(ResourceGuardError::NotWritable {
                    path: display(&canonical),
                    writable_roots: self.writable_roots.iter().map(|r| display(r)).collect(),
                });
            }
            return Ok(());
        }

        if self.reaches_an_external_directory(&canonical) {
            return Ok(());
        }

        Err(self.outside(&canonical))
    }

    /// The absolute path a configured path names, once the boundary has admitted
    /// it.
    ///
    /// A path written in a configuration is relative to the **workspace root**,
    /// never to the process's working directory — the same rule [`check`] applies
    /// to a path a tool was handed. Where `check` answers "may this be touched",
    /// this hands back the path that was admitted, so a caller about to *use* it
    /// (a child process's working directory, say) uses the very path the guard
    /// agreed to rather than its own spelling of it — the same reason the tool
    /// layer opens the canonical path it checked.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ResourceGuardError> {
        let canonical = canonicalize_for_check(&self.anchor(path))?;
        if is_within(&canonical, &self.workspace_root)
            || self.reaches_an_external_directory(&canonical)
        {
            Ok(canonical)
        } else {
            Err(self.outside(&canonical))
        }
    }

    fn reaches_an_external_directory(&self, canonical: &Path) -> bool {
        self.external_directories.iter().any(|ext| is_within(canonical, ext))
    }

    fn outside(&self, canonical: &Path) -> ResourceGuardError {
        ResourceGuardError::PathOutsideWorkspace {
            path: display(canonical),
            root: display(&self.workspace_root),
        }
    }

    fn writable_allows(&self, path: &Path) -> bool {
        if self.writable_roots.is_empty() {
            true // no explicit writable roots -> the whole workspace is writable
        } else {
            self.writable_roots.iter().any(|r| is_within(path, r))
        }
    }

    /// Attach a relative path to the workspace root. Absolute paths are left
    /// alone: the boundary check is what decides whether they are allowed, not
    /// this function.
    fn anchor(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() { p.to_path_buf() } else { self.workspace_root.join(p) }
    }
}

/// A prefix check on already-canonicalized absolute paths.
fn is_within(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn canonicalize_or_raw(p: &str) -> PathBuf {
    Path::new(p).canonicalize().unwrap_or_else(|_| PathBuf::from(p))
}

/// Canonicalize a path, tolerating names that do not exist yet.
///
/// The leaf of a write target usually does not exist, and a directory above it
/// may not either: `fs.write` to `reports/summary.txt` in a workspace without
/// `reports` was refused here as an IO error and surfaced to the model as a
/// boundary violation — the tool never ran (2026-10-21 real use). The deepest
/// ancestor the kernel can resolve is canonicalized and the missing names are
/// re-attached verbatim.
///
/// Re-attaching is sound only because every name in that tail is a plain one:
/// [`Path::file_name`] yields `None` for a tail that ends in `..`, so such a
/// path is refused as invalid instead of being folded lexically without the
/// kernel. The caller still compares the result against the roots, and the
/// resolved base is the kernel's spelling, not the caller's.
fn canonicalize_for_check(path: &Path) -> Result<PathBuf, ResourceGuardError> {
    if let Ok(c) = path.canonicalize() {
        return Ok(c);
    }
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = path;
    loop {
        let Some(name) = ancestor.file_name() else {
            return Err(ResourceGuardError::InvalidPath(display(path)));
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return Err(ResourceGuardError::InvalidPath(display(path)));
        };
        ancestor = parent;
        if let Ok(base) = ancestor.canonicalize() {
            let mut resolved = base;
            for name in missing.iter().rev() {
                resolved.push(name);
            }
            return Ok(resolved);
        }
    }
}

/// Errors from ResourceGuard checks.
#[derive(Debug, thiserror::Error)]
pub enum ResourceGuardError {
    /// The path, and the root that would have admitted it.
    ///
    /// Both, because they answer different halves of what a refusal raises. The
    /// path is what was asked for; the root is where it would have had to be —
    /// and that is the whole of the remedy this check can offer, since a path
    /// inside the workspace is one it admits by construction. The guard has held
    /// the root all along; printing only the path left a reader who needs it to
    /// go and find out what the workspace even was.
    #[error("path outside workspace: {path} (the workspace root is {root})")]
    PathOutsideWorkspace { path: String, root: String },
    /// The path, and the roots that *are* writable.
    ///
    /// The sibling refusal above quotes the workspace root because that is what
    /// would have admitted the path. This one is the opposite case — the path is
    /// already inside the workspace and was refused by the writable-root filter
    /// on top of it — so quoting the workspace root would name something that
    /// was never the obstacle. The writable roots are the whole remedy, and this
    /// refusal can only be reached when there is at least one of them (with none
    /// configured, the workspace is writable throughout), so the list is never
    /// empty here.
    #[error("path not writable: {path} (writable roots: {})", .writable_roots.join(", "))]
    NotWritable { path: String, writable_roots: Vec<String> },
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn root_guard(dir: &tempfile::TempDir) -> (ResourceGuard, PathBuf) {
        let root = dir.path().to_path_buf();
        let workspace = Workspace {
            workspace_root: root.display().to_string(),
            writable_roots: vec![],
            external_directories: vec![],
        };
        (ResourceGuard::new(workspace), root)
    }

    #[test]
    fn a_relative_path_is_anchored_to_the_workspace_root_not_the_process_cwd() {
        // The tool layer opens `a.txt` relative to the workspace root, so this
        // check has to read it the same way. It used to canonicalize the raw
        // string, which made the answer depend on where the process was
        // launched from — and with a workspace that was not the cwd, every
        // relative path failed.
        let dir = tempfile::tempdir().unwrap();
        let (guard, root) = root_guard(&dir);
        fs::write(root.join("a.txt"), "x").unwrap();

        // The premise: the process is not sitting in the workspace, so nothing
        // here can pass by accident.
        assert_ne!(std::env::current_dir().unwrap(), root);

        assert!(guard.check("a.txt", false).is_ok());
        assert!(guard.check("a.txt", true).is_ok());
        // A write target that does not exist yet: the leaf is missing, the
        // directory it will live in is not.
        assert!(guard.check("new.txt", true).is_ok());

        // Anchoring to the root must not turn an escape into a pass.
        assert!(matches!(
            guard.check("../escape.txt", false),
            Err(ResourceGuardError::PathOutsideWorkspace { .. })
        ));
    }

    #[test]
    fn a_write_into_a_directory_that_does_not_exist_yet_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, _) = root_guard(&dir);

        // Neither `reports` nor `reports/deep` exists. The file backend
        // creates the directories it advertises creating, so the guard has to
        // admit the path; refusing it read to the model as a policy denial.
        assert!(guard.check("reports/deep/summary.txt", true).is_ok());
    }

    #[test]
    fn a_missing_path_outside_the_workspace_is_still_denied() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, _) = root_guard(&dir);
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("not-yet").join("f.txt");

        // Both the leaf and its directory are missing, so the existing
        // ancestor is what the check compares against the root.
        assert!(matches!(
            guard.check(path.to_str().unwrap(), true),
            Err(ResourceGuardError::PathOutsideWorkspace { .. })
        ));
    }

    #[test]
    fn path_inside_workspace_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, root) = root_guard(&dir);
        let file = root.join("a.txt");
        fs::write(&file, "x").unwrap();
        assert!(guard.check(file.to_str().unwrap(), false).is_ok());
        assert!(guard.check(file.to_str().unwrap(), true).is_ok());
    }

    #[test]
    fn path_outside_workspace_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, _) = root_guard(&dir);
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("b.txt");
        fs::write(&file, "x").unwrap();
        assert!(matches!(
            guard.check(file.to_str().unwrap(), false),
            Err(ResourceGuardError::PathOutsideWorkspace { .. })
        ));
    }

    #[test]
    fn a_refused_path_says_where_the_workspace_is() {
        // The two facts a refusal raises are what was asked for and where it
        // would have had to be. The guard holds the root and the message left it
        // out, so a reader had to go and find out what the workspace even was
        // before they could act on the refusal.
        let dir = tempfile::tempdir().unwrap();
        let (guard, root) = root_guard(&dir);
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("b.txt");
        fs::write(&file, "x").unwrap();

        let message = guard.check(file.to_str().unwrap(), false).unwrap_err().to_string();
        assert!(message.contains("path outside workspace"), "{message}");
        // Canonical, because that is the form the guard compares in — and so the
        // form it can honestly claim is the boundary.
        let root = root.canonicalize().unwrap();
        assert!(message.contains(&root.display().to_string()), "{message}");
    }

    #[test]
    fn dotdot_escape_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, root) = root_guard(&dir);
        let sibling = root.parent().unwrap().join("yaoyao-sibling.txt");
        fs::write(&sibling, "secret").unwrap();
        let escape = root.join("..").join("yaoyao-sibling.txt");
        assert!(matches!(
            guard.check(escape.to_str().unwrap(), false),
            Err(ResourceGuardError::PathOutsideWorkspace { .. })
        ));
    }

    #[test]
    fn write_outside_writable_roots_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ro = root.join("ro");
        let rw = root.join("rw");
        fs::create_dir_all(&ro).unwrap();
        fs::create_dir_all(&rw).unwrap();
        let workspace = Workspace {
            workspace_root: root.display().to_string(),
            writable_roots: vec![rw.display().to_string()],
            external_directories: vec![],
        };
        let guard = ResourceGuard::new(workspace);

        let read_only_file = ro.join("a.txt");
        fs::write(&read_only_file, "x").unwrap();
        assert!(guard.check(read_only_file.to_str().unwrap(), false).is_ok());
        let refused = guard.check(read_only_file.to_str().unwrap(), true).unwrap_err();
        assert!(matches!(refused, ResourceGuardError::NotWritable { .. }), "{refused:?}");
        // The refusal has to name the boundary that stopped it. This one was
        // refused by the writable-root filter, so the root that explains it is
        // the writable one — the workspace root is where the path already is,
        // and naming it would answer a question nobody asked.
        let message = refused.to_string();
        let writable = fs::canonicalize(&rw).unwrap().display().to_string();
        assert!(message.contains(&writable), "the writable root is named: {message}");
        assert!(message.contains("writable roots"), "{message}");

        let writable_file = rw.join("b.txt");
        assert!(guard.check(writable_file.to_str().unwrap(), true).is_ok());
    }

    #[test]
    fn external_directory_is_allowed_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let (_, root) = root_guard(&dir);
        let ext = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            workspace_root: root.display().to_string(),
            writable_roots: vec![],
            external_directories: vec![ext.path().display().to_string()],
        };
        let guard = ResourceGuard::new(workspace);
        let file = ext.path().join("c.txt");
        fs::write(&file, "x").unwrap();
        assert!(guard.check(file.to_str().unwrap(), false).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_denied() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let (guard, root) = root_guard(&dir);
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "secret").unwrap();

        let link = root.join("link.txt");
        symlink(&secret, &link).unwrap();

        // canonicalize resolves the symlink to the outside file -> denied.
        assert!(matches!(
            guard.check(link.to_str().unwrap(), false),
            Err(ResourceGuardError::PathOutsideWorkspace { .. })
        ));
    }

    #[test]
    fn a_relative_configured_path_resolves_against_the_workspace_root() {
        // A path written in a configuration is relative to the workspace root,
        // not to wherever the process happens to run: a child started with this
        // as its working directory would otherwise inherit the launcher's
        // directory, which is the whole defect this method exists to close.
        let dir = tempfile::tempdir().unwrap();
        let (guard, root) = root_guard(&dir);
        fs::create_dir_all(root.join("sub")).unwrap();

        // The premise: the process is not sitting in the workspace, so nothing
        // here resolves to the right place by accident.
        assert_ne!(std::env::current_dir().unwrap(), root);

        let canonical_root = root.canonicalize().unwrap();
        let resolved = guard.resolve("sub").unwrap();
        assert_eq!(resolved, canonical_root.join("sub"));
        assert!(resolved.starts_with(&canonical_root));
    }

    #[test]
    fn a_configured_path_that_escapes_the_workspace_does_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, _) = root_guard(&dir);
        assert!(matches!(
            guard.resolve("../outside"),
            Err(ResourceGuardError::PathOutsideWorkspace { .. })
        ));

        // An absolute path outside is refused the same way as a relative one
        // that climbs out: the anchored spelling is not what is refused, the
        // place is.
        let outside = tempfile::tempdir().unwrap();
        assert!(matches!(
            guard.resolve(outside.path().to_str().unwrap()),
            Err(ResourceGuardError::PathOutsideWorkspace { .. })
        ));
    }

    #[test]
    fn an_absolute_configured_path_inside_the_workspace_resolves_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let (guard, root) = root_guard(&dir);
        fs::create_dir_all(root.join("sub")).unwrap();
        let real = root.join("sub").canonicalize().unwrap();
        assert_eq!(guard.resolve(real.to_str().unwrap()).unwrap(), real);
    }
}
