//! Acceptance against a *third-party* MCP server: somebody else's program, on
//! the real wire, through the assembled binary.
//!
//! Every other MCP test in this workspace drives a fixture this repository
//! wrote, so "the adapter is right" has only ever been checked against our own
//! assumptions about the protocol. This file drives
//! `@modelcontextprotocol/server-filesystem` — an official reference
//! implementation, written by people who never read this code — through a real
//! `npx`, a real `node`, and a real child process.
//!
//! It skips when it cannot run: no `npx`, or no registry to fetch the package
//! from. A skip is not a pass, so it says why on stderr. The other half of that
//! honesty is what makes the skip safe — once `npx` and the registry are both
//! there, nothing below is allowed to be lenient, because a test that quietly
//! gave up would report success for a wiring it never exercised.
//!
//! The version is pinned inside the package string. A later release that
//! changes its tool surface is a new fact to check, not a regression here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The exact third-party artifact this acceptance is against.
const PACKAGE: &str = "@modelcontextprotocol/server-filesystem@2026.8.31";

/// How long the registry may take to answer before this test gives up on it.
///
/// Bounded here rather than left to npm's own timeouts: a registry that accepts
/// the connection and then stalls would turn this file's honest skip into a
/// hung test run, which is worse than either outcome it is trying to tell
/// apart.
const FETCH_DEADLINE: Duration = Duration::from_secs(60);

/// The configuration variables the binary documents. Named here rather than
/// imported, as the sibling suites do: an integration test cannot see the
/// binary's modules, and these two are a deployment contract.
const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// `name` from this platform's `PATH`.
///
/// On Windows both `npx` and `npm` are shell scripts, and the `.cmd` beside
/// each is the thing a process can start: Rust's `Command` resolves `PATH` but
/// not `PATHEXT`, so asking for `npx` there fails with "program not found"
/// while a terminal runs it fine. Nothing in the adapter is involved — it
/// starts whatever the configuration names — so this is the configuration's
/// fact to spell, and it is spelled here.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

fn npx() -> Option<PathBuf> {
    on_path(if cfg!(windows) { "npx.cmd" } else { "npx" })
}

/// Whether the pinned package can be fetched at all.
///
/// Asked of the registry directly rather than inferred from a failure below,
/// because two outcomes have to stay apart: "this machine is offline" is a
/// skip, and "the server started and we could not speak to it" is the failure
/// this file exists to catch.
fn package_is_fetchable() -> bool {
    let Some(npm) = on_path(if cfg!(windows) { "npm.cmd" } else { "npm" }) else {
        return false;
    };
    let Ok(mut child) = Command::new(npm)
        .args(["view", PACKAGE, "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let until = Instant::now() + FETCH_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            // Out of time, or the wait itself failed. Either way the answer is
            // "not fetchable", and the child is not left behind.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// `npx`, or a stated reason this test cannot run.
fn npx_or_skip() -> Option<PathBuf> {
    let Some(npx) = npx() else {
        eprintln!("[skipped] no `npx` on PATH, so {PACKAGE} cannot be run");
        return None;
    };
    if !package_is_fetchable() {
        eprintln!("[skipped] the npm registry is not reachable, so {PACKAGE} cannot be fetched");
        return None;
    }
    Some(npx)
}

/// What one run of the binary produced.
struct Ran {
    ok: bool,
    /// The status a script reads without parsing anything. `None` only when the
    /// process was killed by a signal, which nothing here does on purpose.
    code: Option<i32>,
    out: String,
    err: String,
}

fn dirs() -> (TempDir, TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap())
}

/// One run, with a user configuration and nothing else in the environment.
///
/// The configuration variables are cleared first for the reason the sibling
/// suites give: a developer with a user configuration exported would have every
/// run here quietly execute under it.
fn ran(data: &Path, workspace: &Path, config: &Path, argv: &[&str]) -> Ran {
    let output = Command::new(env!("CARGO_BIN_EXE_agent"))
        .env("AGENT_DATA_DIR", data)
        .env("AGENT_WORKSPACE_DIR", workspace)
        .env("AGENT_MODEL_PROVIDER", "echo")
        .env(ENV_USER_CONFIG, config)
        .env_remove(ENV_MANAGED_POLICY)
        .args(argv)
        .output()
        .expect("the binary starts");
    Ran {
        ok: output.status.success(),
        code: output.status.code(),
        out: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        err: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

/// The one directory the third-party server is allowed to see: a subdirectory
/// of the workspace, not the whole of it.
///
/// The two are deliberately different, and the difference is what makes the
/// refusal test below mean what it says. The workspace is what *this* build's
/// path guard admits; the server's root is what the *server* admits. A file
/// outside both would be refused by our guard before the tool ever ran, and a
/// test asserting "the server refused" would then be asserting something that
/// never happened.
fn server_root(workspace: &TempDir) -> PathBuf {
    let root = workspace.path().join("allowed");
    std::fs::create_dir(&root).expect("a directory for the server to be allowed");
    root
}

/// A user configuration whose one server is the third-party package, allowed to
/// see exactly `allowed`.
///
/// `process.spawn` is the capability a stdio server honestly requires — the
/// same declaration `mcp_wiring.rs` makes for its own fixture: reaching a stdio
/// server means starting a program. The timeout is the one the configuration
/// language spells, and it is generous on purpose: the first run on a cold
/// cache unpacks the package before node ever starts serving.
fn config_for(home: &TempDir, npx: &Path, allowed: &Path) -> PathBuf {
    let path = home.path().join("user.toml");
    std::fs::write(
        &path,
        format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.files\"]\n\n\
             [mcp.servers.files]\n\
             command = '{command}'\n\
             args = [\"-y\", \"{package}\", '{allowed}']\n\
             requires = [\"process.spawn\"]\n\
             timeout = \"120s\"\n\n\
             [profiles.p]\n\
             tools = {{ allow = [\"process.spawn\"] }}\n\
             policy = {{ shell = \"allow\" }}\n\
             plugins = {{ enable = [\"mcp.files\"] }}\n",
            command = npx.display(),
            package = PACKAGE,
            allowed = allowed.display(),
        ),
    )
    .expect("a user configuration");
    path
}

/// The arguments for a `read_text_file` call, built as JSON rather than
/// interpolated: a Windows path is full of backslashes, and those are JSON
/// escapes before they are path separators.
fn read_args(path: &Path) -> String {
    serde_json::json!({ "path": path.display().to_string() }).to_string()
}

#[test]
fn a_real_third_party_server_is_reached_and_reads_a_file() {
    let Some(npx) = npx_or_skip() else { return };
    let (data, workspace, home) = dirs();
    let root = server_root(&workspace);
    let config = config_for(&home, &npx, &root);
    let notes = root.join("notes.txt");
    std::fs::write(&notes, "rhubarb\n").expect("a file for the server to read");

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "mcp__files__read_text_file", &read_args(&notes)],
    );

    // Answering at all means the whole chain held: `npx` started, the package's
    // `initialize` handshake was answered over the protocol version this
    // adapter pins, `tools/list` came back with a surface, the server's own
    // name was namespaced to `mcp__files__read_text_file`, and a call under
    // that name reached the child and came back through the gateway.
    assert!(r.ok, "the run did not finish: {}", r.err);
    assert!(r.out.contains("rhubarb"), "read_text_file answered {:?} (stderr {})", r.out, r.err);
}

#[test]
fn a_refusal_from_a_real_server_arrives_as_a_failed_result() {
    let Some(npx) = npx_or_skip() else { return };
    let (data, workspace, home) = dirs();
    let root = server_root(&workspace);
    let config = config_for(&home, &npx, &root);
    // A real file inside the workspace — which is what this build's path guard
    // admits — and outside the directory the server was allowed. So the refusal
    // has nowhere to come from but the server.
    let outside = workspace.path().join("outside.txt");
    std::fs::write(&outside, "not mine\n").expect("a file the server may not read");

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "--json", "tool", "mcp__files__read_text_file", &read_args(&outside)],
    );

    let result: serde_json::Value = serde_json::from_str(&r.out).unwrap_or_else(|e| {
        panic!("`tool --json` did not print one object ({e}): out {:?} err {:?}", r.out, r.err)
    });

    // The server throws, its SDK turns the throw into `isError: true`, and the
    // adapter has to read that as a failed run rather than an empty one. A
    // result reading "completed, no content" would be this build reporting
    // success for a refusal — the one answer a caller must never get here.
    assert_eq!(result["status"], "failed", "{result}");
    let content = result["content"].as_str().unwrap_or_default();
    assert!(content.contains("Access denied"), "the refusal said {content:?}");

    // And the command says so, not only the payload: a script that reads `$?`
    // learns the same thing as one that parses the answer. The result is still
    // printed — what the exit status adds is that nobody has to read it to find
    // out which kind of answer this was. It is `1` exactly, not merely non-zero,
    // because that is the whole distinction the status carries: the call ran and
    // this is its answer, as opposed to the `2` a call that never ran gets.
    assert_eq!(r.code, Some(1), "a failed tool call said {:?}, printing {}", r.code, r.out);
}
