//! The tools, checked against a repository that git itself wrote.
//!
//! The unit tests build fixtures in process, which is what the tools do too.
//! These tests are the independent check: git creates and describes the same
//! repository, and the tool's answer is compared with git's own output. A
//! divergence in libgit2's rendering — or in the parsers' idea of git's format
//! — shows up here rather than in a stuck agent.

use std::path::Path;
use std::process::Command;

use agent_ports::tool_provider::{ToolInvocation, ToolProvider};
use agent_tools_git::{GitToolProvider, StatusEntry, parse_diff, parse_status};

/// Run git in `dir`, insisting it worked. Setup and comparison only: the tool
/// under test never shells out.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} could not be run: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run git and hand back its stdout as text.
fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} could not be run: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

/// A repository with a staged rename, a modified tracked file, and an
/// untracked one — three different answers for one question.
fn repo() -> (tempfile::TempDir, GitToolProvider) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();

    git(path, &["init", "-q", "."]);
    // Without this the index stores CRLF-converted blobs and every file looks
    // wholly rewritten, which would make the counts below meaningless.
    git(path, &["config", "core.autocrlf", "false"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);

    write(path, "a.txt", "one\ntwo\nthree\n");
    write(path, "tracked.txt", "keep\n");
    git(path, &["add", "-A"]);
    git(path, &["commit", "-qm", "init"]);

    git(path, &["mv", "a.txt", "renamed.txt"]);
    write(path, "tracked.txt", "keep\nadded\n");
    write(path, "untracked.txt", "new\n");

    let provider = GitToolProvider::new(path.to_path_buf());
    (dir, provider)
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

/// The entry for `path`, wherever git happened to put it in the list.
fn entry<'a>(status: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    status["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == path)
        .unwrap_or_else(|| panic!("no entry for {path} in {status}"))
}

#[tokio::test]
async fn status_reads_a_real_repository() {
    let (_dir, provider) = repo();
    let status = call(&provider, "git.status", serde_json::json!({})).await;

    assert_eq!(status["clean"], false);

    let renamed = entry(&status, "renamed.txt");
    assert_eq!(renamed["state"], "renamed");
    assert_eq!(renamed["original"], "a.txt", "the path it came from, as a field of its own");

    let modified = entry(&status, "tracked.txt");
    assert_eq!(modified["state"], "modified");
    assert_eq!(modified["worktree"], "M");

    let untracked = entry(&status, "untracked.txt");
    assert_eq!(untracked["state"], "untracked");

    // The rename's source is a field, not an entry: a parser that treated it as
    // one would report four changes where there are three.
    assert_eq!(status["count"], 3);
}

#[tokio::test]
async fn status_matches_what_git_prints() {
    let (dir, provider) = repo();
    let status = call(&provider, "git.status", serde_json::json!({})).await;

    let expected = parse_status(&git_output(dir.path(), &["status", "--porcelain=v1", "-z"]));
    let actual: Vec<StatusEntry> = status["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| StatusEntry {
            index: e["index"].as_str().unwrap().chars().next().unwrap(),
            worktree: e["worktree"].as_str().unwrap().chars().next().unwrap(),
            path: e["path"].as_str().unwrap().to_string(),
            original: e["original"].as_str().map(str::to_string),
        })
        .collect();

    // Both sides in path order: git's own ordering is not the claim here.
    let mut expected = expected;
    let mut actual = actual;
    expected.sort_by(|a, b| a.path.cmp(&b.path));
    actual.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn the_worktree_diff_reads_a_real_repository() {
    let (_dir, provider) = repo();
    let diff = call(&provider, "git.diff", serde_json::json!({})).await;

    // The rename is staged, so the worktree diff is the one edited file.
    assert_eq!(diff["file_count"], 1);
    let file = &diff["files"][0];
    assert_eq!(file["file"], "tracked.txt");
    assert_eq!(file["status"], "modified");
    assert_eq!(file["additions"], 1);
    assert_eq!(file["deletions"], 0);
    assert!(file["patch"].as_str().unwrap().contains("+added"), "{}", file["patch"]);
}

#[tokio::test]
async fn the_diff_matches_what_git_prints() {
    let (dir, provider) = repo();
    for (args, staged) in [
        (vec!["diff", "--no-color", "--no-ext-diff", "--unified=3"], false),
        (vec!["diff", "--no-color", "--no-ext-diff", "--unified=3", "--cached"], true),
    ] {
        let diff = call(&provider, "git.diff", serde_json::json!({ "staged": staged })).await;
        let expected = parse_diff(&git_output(dir.path(), &args));

        let actual_files = diff["files"].as_array().unwrap();
        assert_eq!(actual_files.len(), expected.len(), "staged={staged}: {diff}");
        for (actual, expected) in actual_files.iter().zip(expected.iter()) {
            assert_eq!(actual["file"], expected.file, "staged={staged}");
            assert_eq!(actual["status"], expected.status, "staged={staged}");
            assert_eq!(actual["additions"], expected.additions, "staged={staged}");
            assert_eq!(actual["deletions"], expected.deletions, "staged={staged}");
        }
    }
}

#[tokio::test]
async fn a_file_without_a_trailing_newline_matches_git() {
    // libgit2 hands the "no newline" marker back as content with no origin of
    // its own, and carries the newline that ends the previous line in it. A
    // renderer that adds a marker back prints `-a\` where git prints `-a`.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "-q", "."]);
    git(path, &["config", "core.autocrlf", "false"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("f.txt"), "a").unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-qm", "init"]);
    std::fs::write(path.join("f.txt"), "b").unwrap();

    let provider = GitToolProvider::new(path.to_path_buf());
    let diff = call(&provider, "git.diff", serde_json::json!({})).await;
    let expected =
        git_output(path, &["-c", "core.quotepath=false", "diff", "--no-color", "--unified=3"]);

    // `parse_diff` joins lines, so the listing never ends in a newline; compare
    // through the same reader the tool uses.
    assert_eq!(diff["files"][0]["patch"], parse_diff(&expected)[0].patch, "{diff}");
}

#[tokio::test]
async fn the_staged_diff_shows_a_pure_rename() {
    let (_dir, provider) = repo();
    let diff = call(&provider, "git.diff", serde_json::json!({ "staged": true })).await;

    assert_eq!(diff["file_count"], 1);
    let file = &diff["files"][0];
    // A rename with no edits has no hunks at all, so a parser that only emits a
    // file when it sees `@@` would report no changes where git reports one.
    assert_eq!(file["file"], "renamed.txt");
    assert_eq!(file["status"], "renamed");
    assert_eq!(file["additions"], 0);
    assert_eq!(file["deletions"], 0);
}

#[tokio::test]
async fn a_path_narrows_the_diff_to_that_file() {
    let (_dir, provider) = repo();
    let diff = call(&provider, "git.diff", serde_json::json!({ "path": "untracked.txt" })).await;
    assert_eq!(diff["file_count"], 0, "an untracked file is not in a diff: {diff}");

    let tracked = call(&provider, "git.diff", serde_json::json!({ "path": "tracked.txt" })).await;
    assert_eq!(tracked["file_count"], 1);
    assert_eq!(tracked["files"][0]["file"], "tracked.txt");
}

#[tokio::test]
async fn a_directory_path_narrows_the_diff_to_that_directory() {
    // git treats a directory pathspec as "everything below it". libgit2's
    // pathspec matching has to agree, or `path: "src"` would silently list
    // nothing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "-q", "."]);
    git(path, &["config", "core.autocrlf", "false"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    std::fs::create_dir(path.join("src")).unwrap();
    write(path, "src/lib.rs", "one\n");
    write(path, "top.txt", "one\n");
    git(path, &["add", "-A"]);
    git(path, &["commit", "-qm", "init"]);
    write(path, "src/lib.rs", "one\ntwo\n");
    write(path, "top.txt", "one\ntwo\n");

    let provider = GitToolProvider::new(path.to_path_buf());
    let diff = call(&provider, "git.diff", serde_json::json!({ "path": "src" })).await;
    assert_eq!(diff["file_count"], 1, "{diff}");
    assert_eq!(diff["files"][0]["file"], "src/lib.rs");
}
