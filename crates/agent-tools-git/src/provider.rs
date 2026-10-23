//! GitToolProvider — read-only git inspection, as structured results.
//!
//! Two tools, and deliberately no generic one. `git.run <args>` would make the
//! whitelist of permitted subcommands a runtime check that has to keep up with
//! what the model passes; two tools whose argv is fixed have no such gap,
//! because nothing the model sends can choose the subcommand at all. What the
//! model *can* choose is a path and whether to look at the index, and both are
//! arguments to the reader rather than commands.
//!
//! The repository is read in process with libgit2 instead of spawning `git`.
//! Under the AppContainer backend git cannot obtain a working directory while
//! it starts — its own startup dies with EACCES — and no grant on the workspace
//! or its ancestors changes that, while this process already holds a working
//! directory and reads the same repository fine. Reading in process also means
//! no hooks, no filters, no external diff drivers and no credential helper can
//! run, which is what "read-only" should have meant all along.
//!
//! libgit2 does not print porcelain or a patch by itself in a form worth
//! parsing, so its structures are rendered into the same machine text git
//! prints and handed to [`parse_status`] / [`parse_diff`]. The JSON shape is
//! therefore the same as before, and the parsers stay the one place that knows
//! the text format.
//!
//! Committing and pushing are deliberately absent: when they arrive they will
//! need their own capabilities rather than being an extra string on this one.
//!
//! `git.diff`'s `path` has two readers: the repository treats it as a pathspec
//! relative to the **repository** root, and the gateway's resource guard treats
//! the same string as a path relative to the **workspace** root. The two agree
//! exactly when the workspace root is the repository root, which is the layout
//! this project assumes — a run starts at the top of the repository it works
//! on. A workspace nested inside a repository, or a repository nested inside a
//! workspace, would give the string two meanings; that layout is unsupported
//! and `CONTRACT.md` §7 records it as a declared boundary rather than a check
//! that pretends to handle it.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::permissions::Capability;
use serde_json::json;

use crate::parse::{StatusEntry, parse_diff, parse_status};

/// How long a status or diff may take. The work runs on a worker thread and
/// this is how long the call waits for it: libgit2 has no cancellation point,
/// so a pathological repository can outlive the wait, but the tool returns
/// instead of hanging the run.
pub const GIT_TIMEOUT_MS: u64 = 15_000;

/// The same output cap the execution backends apply, so a huge diff cannot
/// turn one tool result into the whole context window.
const OUTPUT_CAP_BYTES: usize = 1024 * 1024;

/// Read-only git tools, reading the repository at `repository_root` in process.
pub struct GitToolProvider {
    repository_root: PathBuf,
}

impl GitToolProvider {
    pub fn new(repository_root: impl Into<PathBuf>) -> Self {
        Self { repository_root: repository_root.into() }
    }

    /// Run one inspection on a worker thread and wait at most
    /// [`GIT_TIMEOUT_MS`] for it.
    ///
    /// A failure of the repository read is a failure of the call, not content:
    /// "not a git repository" is the answer to the question, and handing it
    /// back as an empty listing would say "no changes" instead.
    fn inspect<F>(&self, tool: &str, work: F) -> Result<(String, bool), ToolProviderError>
    where
        F: FnOnce() -> Result<(String, bool), String> + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("git-inspection".to_string())
            .spawn(move || {
                // The receiver may already be gone after a timeout; there is
                // nobody left to tell, and the thread ends on its own.
                let _ = tx.send(work());
            })
            .map_err(|e| failed(tool, &format!("could not start the repository reader: {e}")))?;

        match rx.recv_timeout(Duration::from_millis(GIT_TIMEOUT_MS)) {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(message)) => Err(failed(tool, &message)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(failed(
                tool,
                &format!("reading the repository did not finish within {GIT_TIMEOUT_MS} ms"),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(failed(tool, "the repository reader stopped without a result"))
            }
        }
    }

    fn status(&self) -> Result<ToolResult, ToolProviderError> {
        let root = self.repository_root.clone();
        let (raw, truncated) = self.inspect("git.status", move || status_text(&root))?;
        let entries = parse_status(&raw);
        completed(
            truncated,
            json!({
                "clean": entries.is_empty(),
                "count": entries.len(),
                "entries": entries.iter().map(StatusEntry::to_json).collect::<Vec<_>>(),
            }),
        )
    }

    fn diff(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let staged = invocation.arguments.get("staged").and_then(|v| v.as_bool()).unwrap_or(false);
        let path = invocation.arguments.get("path").and_then(|v| v.as_str()).map(str::to_string);
        let root = self.repository_root.clone();
        let (raw, truncated) = self.inspect("git.diff", move || diff_text(&root, staged, path))?;
        let files = parse_diff(&raw);
        let additions: u32 = files.iter().map(|f| f.additions).sum();
        let deletions: u32 = files.iter().map(|f| f.deletions).sum();
        completed(
            truncated,
            json!({
                "files": files.iter().map(|f| f.to_json()).collect::<Vec<_>>(),
                "file_count": files.len(),
                "additions": additions,
                "deletions": deletions,
            }),
        )
    }
}

/// Render the repository's status as `git status --porcelain=v1 -z` would.
///
/// Entries are sorted by path because that is the order git prints them in,
/// and a caller comparing two listings should not have to sort first. A rename
/// or copy contributes the *new* path as the entry and the old one as the
/// following NUL field, which is exactly what the parser expects.
fn status_text(root: &Path) -> Result<(String, bool), String> {
    let repository = open(root)?;
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(false)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true)
        .include_ignored(false);
    let statuses = repository.statuses(Some(&mut options)).map_err(|e| git_error("status", &e))?;

    let mut entries: Vec<(char, char, String, Option<String>)> = Vec::new();
    for entry in statuses.iter() {
        let status = entry.status();
        let (index, worktree) = columns(status);

        // libgit2 reports the *old* path for a renamed entry; git reports the
        // new one as the entry and the old one as a separate field. The delta
        // is the only place both paths appear together.
        let mut path = String::from_utf8_lossy(entry.path_bytes()).into_owned();
        let mut original = None;
        if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            let delta = if matches!(index, 'R' | 'C') {
                entry.head_to_index()
            } else {
                entry.index_to_workdir()
            };
            if let Some(delta) = delta {
                let old =
                    String::from_utf8_lossy(delta.old_file().path_bytes().unwrap_or_default())
                        .into_owned();
                let new =
                    String::from_utf8_lossy(delta.new_file().path_bytes().unwrap_or_default())
                        .into_owned();
                if !new.is_empty() {
                    path = new;
                }
                if !old.is_empty() {
                    original = Some(old);
                }
            }
        }

        entries.push((index, worktree, path, original));
    }
    entries.sort_by(|a, b| a.2.as_bytes().cmp(b.2.as_bytes()));

    let mut raw = String::new();
    for (index, worktree, path, original) in entries {
        raw.push(index);
        raw.push(worktree);
        raw.push(' ');
        raw.push_str(&path);
        raw.push('\0');
        if let Some(original) = original {
            raw.push_str(&original);
            raw.push('\0');
        }
    }
    Ok(cap(raw, false))
}

/// The two porcelain columns for one libgit2 status word.
///
/// A conflicted path is reported as `UU`. Git distinguishes the conflict kinds
/// (`AA`, `DD`, `DU`, `UD`) by looking at the index stages; the state word the
/// tool exposes is `conflicted` for all of them, and reproducing the two raw
/// columns would mean walking the index a second time for a detail no caller
/// acts on differently. `CONTRACT.md` records the difference.
fn columns(status: git2::Status) -> (char, char) {
    use git2::Status as S;

    if status.contains(S::CONFLICTED) {
        return ('U', 'U');
    }
    // An untracked path is `??` in porcelain, not ` ?`: both columns carry
    // the question mark.
    if status.contains(S::WT_NEW)
        && !status.intersects(
            S::INDEX_NEW
                | S::INDEX_MODIFIED
                | S::INDEX_DELETED
                | S::INDEX_RENAMED
                | S::INDEX_TYPECHANGE,
        )
    {
        return ('?', '?');
    }
    let index = if status.contains(S::INDEX_NEW) {
        'A'
    } else if status.contains(S::INDEX_MODIFIED) {
        'M'
    } else if status.contains(S::INDEX_DELETED) {
        'D'
    } else if status.contains(S::INDEX_RENAMED) {
        'R'
    } else if status.contains(S::INDEX_TYPECHANGE) {
        'T'
    } else {
        ' '
    };
    let worktree = if status.contains(S::WT_NEW) {
        '?'
    } else if status.contains(S::WT_MODIFIED) {
        'M'
    } else if status.contains(S::WT_DELETED) {
        'D'
    } else if status.contains(S::WT_RENAMED) {
        'R'
    } else if status.contains(S::WT_TYPECHANGE) {
        'T'
    } else {
        ' '
    };
    (index, worktree)
}

/// Render a unified diff of the worktree (or, with `staged`, of the index
/// against `HEAD`) with three lines of context, as `git diff` would.
///
/// Libgit2 hands each line back without the marker that tells a reader what it
/// is; the markers are restored here so the text is the one [`parse_diff`]
/// already reads. Header and binary lines carry their whole text and no
/// marker, which matches what git prints.
fn diff_text(root: &Path, staged: bool, path: Option<String>) -> Result<(String, bool), String> {
    let repository = open(root)?;
    let mut options = git2::DiffOptions::new();
    options
        .context_lines(3)
        .include_untracked(false)
        .recurse_untracked_dirs(false)
        .include_ignored(false);
    if let Some(path) = path.as_deref() {
        options.pathspec(path);
    }

    let mut diff = if staged {
        let head_tree = match repository.head() {
            Ok(head) => Some(head.peel_to_tree().map_err(|e| git_error("resolving HEAD", &e))?),
            // An unborn branch has nothing to compare the index with; the
            // empty tree is what `git diff --cached` uses in that case.
            Err(e)
                if matches!(
                    e.code(),
                    git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
                ) =>
            {
                None
            }
            Err(e) => return Err(git_error("resolving HEAD", &e)),
        };
        let index = repository.index().map_err(|e| git_error("opening the index", &e))?;
        repository
            .diff_tree_to_index(head_tree.as_ref(), Some(&index), Some(&mut options))
            .map_err(|e| git_error("diffing the index", &e))?
    } else {
        repository
            .diff_index_to_workdir(None, Some(&mut options))
            .map_err(|e| git_error("diffing the worktree", &e))?
    };

    let mut find = git2::DiffFindOptions::new();
    find.renames(true);
    diff.find_similar(Some(&mut find)).map_err(|e| git_error("finding renames", &e))?;

    let mut out = String::new();
    let mut stopped = false;
    diff.print(git2::DiffFormat::Patch, |_, _, line| {
        // The callback cannot stop the walk: libgit2 reports a false return
        // as an error rather than as "enough". Keep draining and stop
        // collecting, so memory stays capped even when the diff is huge.
        if out.len() >= OUTPUT_CAP_BYTES {
            stopped = true;
            return true;
        }
        match line.origin() {
            // File and hunk headers arrive complete, marker included.
            'F' | 'H' => out.push_str(&String::from_utf8_lossy(line.content())),
            '+' | '-' | ' ' => {
                out.push(line.origin());
                out.push_str(&String::from_utf8_lossy(line.content()));
            }
            // `\ No newline at end of file` — libgit2 hands back the leading
            // newline that ends the previous line plus the marker itself, so
            // the content goes in untouched. Adding a backslash here would
            // print two.
            '<' | '>' | '=' => out.push_str(&String::from_utf8_lossy(line.content())),
            // Binary deltas ("Binary files a/x and b/x differ") carry their
            // whole line and no marker.
            _ => out.push_str(&String::from_utf8_lossy(line.content())),
        }
        true
    })
    .map_err(|e| git_error("rendering the diff", &e))?;

    Ok(cap(out, stopped))
}

/// Open the repository whose root is `root`, naming the failure in the terms
/// the model needs: "there is no repository here" and "no changes" must not
/// look alike.
///
/// The workspace root is also the repository root, or the call is refused. A
/// workspace nested inside a repository would give `path` two meanings — the
/// tool would read it relative to the repository, the gateway's guard relative
/// to the workspace — so the layout is checked rather than declared. Failing
/// closed here costs one comparison and turns a silently wrong listing into a
/// message that names both roots.
fn open(root: &Path) -> Result<git2::Repository, String> {
    let repository = git2::Repository::discover(root).map_err(|e| {
        if e.code() == git2::ErrorCode::NotFound {
            format!("not a git repository: {}", root.display())
        } else {
            format!("cannot open the repository at {}: {}", root.display(), e.message())
        }
    })?;

    let workdir =
        repository.workdir().ok_or_else(|| format!("not a working tree: {}", root.display()))?;
    let same_root = match (std::fs::canonicalize(workdir), std::fs::canonicalize(root)) {
        (Ok(workdir), Ok(root)) => workdir == root,
        // If either path cannot be canonicalized the comparison did not
        // happen, and "not the same" is the safe answer.
        _ => false,
    };
    if !same_root {
        return Err(format!(
            "unsupported layout: the repository root is {} but the workspace root is {}; \
             the git tools require them to be the same directory",
            workdir.display(),
            root.display()
        ));
    }
    Ok(repository)
}

fn git_error(action: &str, e: &git2::Error) -> String {
    format!("{action} failed: {}", e.message())
}

/// Cut `text` to the output cap at a character boundary, and say whether
/// anything was dropped. `stopped` says the producer already gave up at the
/// cap and there is certainly more.
fn cap(mut text: String, stopped: bool) -> (String, bool) {
    if text.len() <= OUTPUT_CAP_BYTES {
        // A producer that stopped early stopped *because* it had more to give.
        return (text, stopped);
    }
    let mut end = OUTPUT_CAP_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

/// A completed result that does not claim to be complete when it is not.
///
/// The text was cut at 1 MiB, so the listing here can be missing entries or
/// the last file's patch; reporting `truncated: false` would turn "we did not
/// see all of it" into "there was no more of it" — the one distinction the
/// flag exists for.
fn completed(truncated: bool, body: serde_json::Value) -> Result<ToolResult, ToolProviderError> {
    let mut body = body;
    if truncated {
        body["note"] = json!(
            "the repository listing exceeded the 1 MiB cap and was truncated; this listing is partial"
        );
    }
    let result = ToolResult::completed(json_of(body));
    Ok(if truncated { result.truncated() } else { result })
}

fn json_of(value: serde_json::Value) -> String {
    // Pretty rather than compact: the diff patch is escaped either way, and
    // the structure is what a reader is looking at.
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
}

fn failed(tool: &str, message: &str) -> ToolProviderError {
    ToolProviderError::Failed { tool: tool.to_string(), message: message.to_string() }
}

#[async_trait::async_trait]
impl ToolProvider for GitToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "git.status".to_string(),
                description: "List what has changed in the repository: `entries` holds one \
                              object per path with `path`, `state` (untracked / modified / \
                              added / deleted / renamed / copied / conflicted / typechange), \
                              the raw `index` and `worktree` columns, and `original` for a \
                              rename or copy. Read-only."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Read,
                capability_requirements: vec![Capability::new(Capability::GIT_STATUS)],
                idempotent: true,
                timeout_ms: Some(GIT_TIMEOUT_MS),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "git.diff".to_string(),
                description: "Show the current changes as one `files` entry per file, each \
                              with its `patch`, `additions`, `deletions`, and `status` \
                              (added / deleted / modified / renamed). Diffs the worktree \
                              against the index by default; set `staged` to diff what is \
                              staged instead, and `path` to narrow it to one path. Read-only."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Read,
                capability_requirements: vec![Capability::new(Capability::GIT_STATUS)],
                idempotent: true,
                timeout_ms: Some(GIT_TIMEOUT_MS),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "staged": { "type": "boolean" },
                        "path": { "type": "string" }
                    }
                }),
            },
        ]
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        match tool_name {
            "git.status" | "git.diff" => Some(ToolSideEffect::Read),
            _ => None,
        }
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        match invocation.tool_name.as_str() {
            "git.status" => self.status(),
            "git.diff" => self.diff(invocation),
            other => Err(ToolProviderError::UnknownTool {
                tool: other.to_string(),
                available: self.definitions().into_iter().map(|d| d.name).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A repository built in process, so these tests do not need `git` on the
    /// machine — which is the whole point of the backend they cover.
    struct Fixture {
        _dir: tempfile::TempDir,
        repo: git2::Repository,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = git2::Repository::init(dir.path()).unwrap();
            // A developer's global config may turn on CRLF conversion; without
            // this pin every fixture file would look wholly rewritten.
            repo.config().unwrap().set_bool("core.autocrlf", false).unwrap();
            Self { _dir: dir, repo }
        }

        fn path(&self) -> &Path {
            self._dir.path()
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.path().join(name), contents).unwrap();
        }

        fn commit(&self, message: &str) {
            let mut index = self.repo.index().unwrap();
            index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = self.repo.find_tree(tree_id).unwrap();
            let signature = git2::Signature::now("Test", "test@example.com").unwrap();
            let parents = match self.repo.head() {
                Ok(head) => vec![head.peel_to_commit().unwrap()],
                Err(_) => Vec::new(),
            };
            let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
            self.repo
                .commit(Some("HEAD"), &signature, &signature, message, &tree, &parent_refs)
                .unwrap();
        }

        fn stage(&self, name: &str) {
            let mut index = self.repo.index().unwrap();
            index.add_path(Path::new(name)).unwrap();
            index.write().unwrap();
        }

        fn provider(&self) -> GitToolProvider {
            GitToolProvider::new(self.path())
        }
    }

    async fn call(
        provider: &GitToolProvider,
        tool: &str,
        args: serde_json::Value,
    ) -> serde_json::Value {
        let result = provider
            .invoke(&ToolInvocation::proposed(tool, args))
            .await
            .unwrap_or_else(|e| panic!("{tool} failed: {e}"));
        serde_json::from_str(&result.content).expect("the tool returns JSON")
    }

    #[tokio::test]
    async fn status_is_returned_as_entries() {
        let fixture = Fixture::new();
        fixture.write("tracked.txt", "keep\n");
        fixture.commit("init");
        fixture.write("tracked.txt", "keep\nchanged\n");
        fixture.write("notes.md", "new\n");

        let status = call(&fixture.provider(), "git.status", serde_json::json!({})).await;

        assert_eq!(status["clean"], false);
        assert_eq!(status["count"], 2);
        // Sorted by path, as git prints it.
        assert_eq!(status["entries"][0]["path"], "notes.md");
        assert_eq!(status["entries"][0]["state"], "untracked");
        assert_eq!(status["entries"][1]["path"], "tracked.txt");
        assert_eq!(status["entries"][1]["state"], "modified");
    }

    #[tokio::test]
    async fn a_clean_repository_says_so() {
        let fixture = Fixture::new();
        fixture.write("tracked.txt", "keep\n");
        fixture.commit("init");

        let status = call(&fixture.provider(), "git.status", serde_json::json!({})).await;
        assert_eq!(status["clean"], true);
        assert_eq!(status["count"], 0);
    }

    #[tokio::test]
    async fn a_rename_names_both_paths() {
        let fixture = Fixture::new();
        fixture.write("old.txt", "one\ntwo\nthree\n");
        fixture.commit("init");
        std::fs::rename(fixture.path().join("old.txt"), fixture.path().join("new.txt")).unwrap();
        let mut index = fixture.repo.index().unwrap();
        index.remove_path(Path::new("old.txt")).unwrap();
        index.add_path(Path::new("new.txt")).unwrap();
        index.write().unwrap();

        let status = call(&fixture.provider(), "git.status", serde_json::json!({})).await;
        assert_eq!(status["count"], 1, "{status}");
        let entry = &status["entries"][0];
        // git reports the new path as the entry and the old one as a field.
        assert_eq!(entry["path"], "new.txt");
        assert_eq!(entry["state"], "renamed");
        assert_eq!(entry["original"], "old.txt");
    }

    #[tokio::test]
    async fn a_diff_comes_back_per_file_with_its_counts() {
        let fixture = Fixture::new();
        fixture.write("notes.txt", "one\ntwo\nthree\n");
        fixture.commit("init");
        fixture.write("notes.txt", "one\nTWO\nthree\n");

        let diff = call(&fixture.provider(), "git.diff", serde_json::json!({})).await;

        assert_eq!(diff["file_count"], 1);
        assert_eq!(diff["files"][0]["file"], "notes.txt");
        assert_eq!(diff["files"][0]["status"], "modified");
        assert_eq!(diff["files"][0]["additions"], 1);
        assert_eq!(diff["files"][0]["deletions"], 1);
        assert!(diff["files"][0]["patch"].as_str().unwrap().contains("+TWO"));
        assert_eq!(diff["additions"], 1);
        assert_eq!(diff["deletions"], 1);
    }

    #[tokio::test]
    async fn staged_and_path_narrow_the_diff() {
        let fixture = Fixture::new();
        fixture.write("tracked.txt", "keep\n");
        fixture.write("other.txt", "same\n");
        fixture.commit("init");
        fixture.write("tracked.txt", "keep\nstaged\n");
        fixture.stage("tracked.txt");
        fixture.write("other.txt", "same\nworktree\n");

        let worktree = call(&fixture.provider(), "git.diff", serde_json::json!({})).await;
        assert_eq!(worktree["file_count"], 1);
        assert_eq!(worktree["files"][0]["file"], "other.txt");

        let staged =
            call(&fixture.provider(), "git.diff", serde_json::json!({ "staged": true })).await;
        assert_eq!(staged["file_count"], 1);
        assert_eq!(staged["files"][0]["file"], "tracked.txt");

        let narrowed = call(
            &fixture.provider(),
            "git.diff",
            serde_json::json!({ "staged": true, "path": "tracked.txt" }),
        )
        .await;
        assert_eq!(narrowed["file_count"], 1);

        let elsewhere = call(
            &fixture.provider(),
            "git.diff",
            serde_json::json!({ "staged": true, "path": "other.txt" }),
        )
        .await;
        assert_eq!(elsewhere["file_count"], 0, "the path narrows to one file: {elsewhere}");
    }

    #[tokio::test]
    async fn a_staged_rename_is_a_rename_with_no_line_changes() {
        let fixture = Fixture::new();
        fixture.write("old.txt", "one\ntwo\nthree\n");
        fixture.commit("init");
        std::fs::rename(fixture.path().join("old.txt"), fixture.path().join("new.txt")).unwrap();
        let mut index = fixture.repo.index().unwrap();
        index.remove_path(Path::new("old.txt")).unwrap();
        index.add_path(Path::new("new.txt")).unwrap();
        index.write().unwrap();

        let diff =
            call(&fixture.provider(), "git.diff", serde_json::json!({ "staged": true })).await;
        assert_eq!(diff["file_count"], 1, "{diff}");
        assert_eq!(diff["files"][0]["file"], "new.txt");
        assert_eq!(diff["files"][0]["status"], "renamed");
        assert_eq!(diff["files"][0]["additions"], 0);
        assert_eq!(diff["files"][0]["deletions"], 0);
    }

    #[tokio::test]
    async fn a_missing_repository_is_a_failed_call_not_an_empty_status() {
        let dir = tempfile::tempdir().unwrap();
        let err = GitToolProvider::new(dir.path())
            .invoke(&ToolInvocation::proposed("git.status", serde_json::json!({})))
            .await
            .unwrap_err();

        // The model gets an explanation rather than an empty status list:
        // "no changes" and "there is no repository" must not look alike.
        assert!(err.to_string().contains("not a git repository"), "{err}");
    }

    #[tokio::test]
    async fn a_workspace_nested_inside_a_repository_is_refused() {
        // `path` would mean one thing to the repository and another to the
        // gateway's guard; the two only agree when the roots coincide.
        let fixture = Fixture::new();
        fixture.write("tracked.txt", "keep\n");
        fixture.commit("init");
        let nested = fixture.path().join("sub");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("file.txt"), "new\n").unwrap();

        let err = GitToolProvider::new(&nested)
            .invoke(&ToolInvocation::proposed("git.status", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unsupported layout"), "{err}");
        assert!(err.to_string().contains("workspace root"), "{err}");
    }

    #[tokio::test]
    async fn a_listing_over_the_cap_is_reported_as_partial() {
        // Every line changes, so the patch is about twice the file's size and
        // comfortably over 1 MiB. Without the flag this listing reads exactly
        // like a complete one.
        let fixture = Fixture::new();
        let original: String = (0..60_000).map(|n| format!("line {n:05}\n")).collect();
        fixture.write("big.txt", &original);
        fixture.commit("init");
        let changed: String = (0..60_000).map(|n| format!("changed {n:05}\n")).collect();
        fixture.write("big.txt", &changed);

        let result = fixture
            .provider()
            .invoke(&ToolInvocation::proposed("git.diff", serde_json::json!({})))
            .await
            .unwrap();

        assert!(result.truncated, "a patch over 1 MiB is not a complete listing");
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(
            parsed["note"].as_str().is_some_and(|note| note.contains("partial")),
            "{}",
            result.content
        );

        // And a small diff carries no note, so the marker means something.
        fixture.write("big.txt", &format!("{original}extra\n"));
        let result = fixture
            .provider()
            .invoke(&ToolInvocation::proposed("git.diff", serde_json::json!({})))
            .await
            .unwrap();
        assert!(!result.truncated);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.get("note").is_none());
    }

    #[test]
    fn the_cap_cuts_at_a_character_boundary_and_says_so() {
        let (text, truncated) = cap("x".repeat(OUTPUT_CAP_BYTES + 10), false);
        assert!(truncated);
        assert_eq!(text.len(), OUTPUT_CAP_BYTES);

        // A multi-byte character straddling the cap is dropped whole rather
        // than split, because the result has to remain a valid String.
        let mut text = "a".repeat(OUTPUT_CAP_BYTES - 1);
        text.push('中');
        let (text, truncated) = cap(text, false);
        assert!(truncated);
        assert_eq!(text.len(), OUTPUT_CAP_BYTES - 1);
    }

    #[tokio::test]
    async fn the_git_tools_are_read_only_and_named_for_what_they_do() {
        let fixture = Fixture::new();
        let provider = fixture.provider();
        let defs = provider.definitions();

        assert_eq!(defs.len(), 2);
        for def in &defs {
            assert_eq!(def.side_effect, ToolSideEffect::Read);
            assert_eq!(def.capability_requirements, vec![Capability::new(Capability::GIT_STATUS)]);
            assert!(def.idempotent);
            assert!(def.description.len() > 40, "a description the model cannot reason with");
        }
        assert_eq!(provider.side_effect("git.status"), Some(ToolSideEffect::Read));
        assert_eq!(provider.side_effect("git.push"), None, "no write tool is declared here");
    }

    #[tokio::test]
    async fn an_unknown_git_tool_is_rejected() {
        let fixture = Fixture::new();
        let err = fixture
            .provider()
            .invoke(&ToolInvocation::proposed("git.commit", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolProviderError::UnknownTool { .. }));
    }
}
