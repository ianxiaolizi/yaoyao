//! The AppContainer cases that need a real Windows kernel to answer.
//!
//! Everything here is ignored by default: the suite needs `agent sandbox setup`
//! to have run on the machine (an elevated, one-time change) and, for the
//! network cases, a working connection. Run them with
//!
//! ```text
//! AGENT_APPCONTAINER_TEST=1 cargo test -p agent-sandbox-win -- --ignored
//! ```
//!
//! The one exception is [`a_profile_without_setup_is_refused_not_fallen_back_to`],
//! which is deterministic on any Windows machine and runs in the ordinary suite:
//! it is the guard against the fallback this backend exists to make impossible.

#![cfg(windows)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

use agent_ports::process_env::child_environment;
use agent_ports::process_launcher::{LaunchError, ProcessLauncher, SpawnRequest, StdinMode};
use agent_sandbox_win::{AppContainerLauncher, PROFILE_NAME};

/// Whether the live suite was asked for.
fn enabled() -> bool {
    std::env::var("AGENT_APPCONTAINER_TEST").ok().as_deref() == Some("1")
}

/// A spawn whose stdin stays open: the caller writes, the child reads.
#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn a_request_can_be_sent_over_stdin() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("stdin");
    let launcher = fixture.launcher(false);
    let mut request =
        command(&fixture, "$line = [Console]::In.ReadLine(); Write-Output \"got:$line\"", &[]);
    request.stdin = StdinMode::Pipe;
    let mut process = launcher.spawn(request).expect("the command starts");
    let mut stdin = process.take_stdin().expect("the pipe is kept when asked for");
    let stdout = process.take_stdout().map(|mut reader| thread::spawn(move || read(&mut *reader)));
    writeln!(stdin, "ping").expect("the request is written");
    stdin.flush().expect("the request is flushed");
    // EOF: the child must not wait for a second line.
    drop(stdin);
    let code = process.wait().expect("the process is waited on");
    let stdout = stdout.and_then(|handle| handle.join().ok()).unwrap_or_default();
    assert_eq!(code, Some(0));
    assert!(stdout.contains("got:ping"), "stdout: {stdout}");
}

/// A workspace and a sandbox root under the temp directory, named per test.
struct Fixture {
    workspace: PathBuf,
    sandbox: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("yaoyao-ac-live-{}-{name}", std::process::id()));
        let workspace = base.join("workspace");
        let sandbox = base.join("sandbox");
        std::fs::create_dir_all(&workspace).expect("create the workspace");
        Self { workspace, sandbox }
    }

    fn launcher(&self, network: bool) -> AppContainerLauncher {
        AppContainerLauncher::new(PROFILE_NAME, &self.workspace, &self.sandbox, network)
            .expect("the machine has been set up")
    }
}

/// The PowerShell the environment would use, found the same way.
fn powershell() -> String {
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("pwsh.exe");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
    Path::new(&root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
        .to_string_lossy()
        .into_owned()
}

/// The environment the layer above the launcher would build.
fn environment(extra: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    let mut parent: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (name, value) in extra {
        parent.push((OsString::from(name), OsString::from(value)));
    }
    child_environment(&parent)
}

fn command(fixture: &Fixture, script: &str, extra: &[(&str, &str)]) -> SpawnRequest {
    SpawnRequest {
        program: powershell(),
        args: vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script.to_string(),
        ],
        cwd: fixture.workspace.clone(),
        env: environment(extra),
        stdin: StdinMode::Closed,
    }
}

/// Run to completion, reading both pipes without letting a full one block the
/// child, and return (exit code, stdout, stderr).
fn run(launcher: &dyn ProcessLauncher, request: SpawnRequest) -> (Option<i32>, String, String) {
    let mut process = launcher.spawn(request).expect("the command starts");
    let stdout = process.take_stdout().map(|mut reader| thread::spawn(move || read(&mut *reader)));
    let stderr = process.take_stderr().map(|mut reader| thread::spawn(move || read(&mut *reader)));
    let code = process.wait().expect("the process is waited on");
    let stdout = stdout.and_then(|handle| handle.join().ok()).unwrap_or_default();
    let stderr = stderr.and_then(|handle| handle.join().ok()).unwrap_or_default();
    (code, stdout, stderr)
}

fn read(reader: &mut dyn Read) -> String {
    let mut text = String::new();
    let _ = reader.read_to_string(&mut text);
    text
}

#[test]
fn a_profile_without_setup_is_refused_not_fallen_back_to() {
    // A derived-but-never-set-up profile must fail construction. Falling back
    // to a local process here would run every command unconfined while the
    // caller believed otherwise.
    let fixture = Fixture::new("no-setup");
    let error = AppContainerLauncher::new(
        "yaoyao.Agent.Sandbox.NoSetupProbe",
        &fixture.workspace,
        &fixture.sandbox,
        false,
    )
    .err()
    .expect("an unconfigured profile must not construct a launcher");
    match error {
        LaunchError::BackendUnavailable(reason) => {
            assert!(reason.contains("sandbox setup"), "{reason}");
        }
        LaunchError::Io(error) => panic!("expected a refusal, got an IO error: {error}"),
    }
}

#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn a_command_runs_and_its_output_is_captured() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("echo");
    let launcher = fixture.launcher(false);
    let (code, stdout, stderr) =
        run(&launcher, command(&fixture, "Write-Output 'hello from the container'", &[]));
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("hello from the container"), "stdout: {stdout}");
}

#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn the_host_key_is_not_in_the_child_environment() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("key");
    let launcher = fixture.launcher(false);
    let sentinel = "SENTINEL-DO-NOT-LEAK-7f3a";
    let (code, stdout, stderr) = run(
        &launcher,
        command(
            &fixture,
            "Write-Output \"key=[$env:AGENT_MODEL_API_KEY]\"",
            &[("AGENT_MODEL_API_KEY", sentinel)],
        ),
    );
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stdout.contains(sentinel), "the key reached the child: {stdout}");
}

#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn the_workspace_is_writable() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("workspace-write");
    let launcher = fixture.launcher(false);
    let target = fixture.workspace.join("written-by-the-container.txt");
    let script = format!(
        "$ErrorActionPreference='Stop'; Set-Content -Path '{}' -Value 'ok'",
        target.display()
    );
    let (code, _, stderr) = run(&launcher, command(&fixture, &script, &[]));
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(std::fs::read_to_string(&target).unwrap_or_default().trim(), "ok");
}

#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn a_write_outside_the_granted_roots_is_refused_by_the_kernel() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("escape");
    let launcher = fixture.launcher(false);
    let target =
        PathBuf::from(format!(r"C:\yaoyao-appcontainer-escape-{}.txt", std::process::id()));
    let script = format!(
        "$ErrorActionPreference='Stop'; Set-Content -Path '{}' -Value 'escaped'",
        target.display()
    );
    let (code, _, _) = run(&launcher, command(&fixture, &script, &[]));
    assert_ne!(code, Some(0), "the write outside the workspace succeeded");
    assert!(!target.exists(), "the file exists outside the workspace");
}

#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1, `agent sandbox setup` and the internet"]
fn network_is_denied_by_default_and_allowed_when_asked_for() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("network");
    // The request is made by the PowerShell process itself rather than by a
    // child: `curl.exe` starts but produces nothing inside the container on the
    // machine this was measured on, which would make the probe a test of the
    // wrong boundary. `http`, not `https`, for the same reason: a TLS failure
    // could be the container being unable to read the host's certificate store.
    let probe = "try { $r = Invoke-WebRequest -Uri 'http://example.com' -UseBasicParsing \
                 -TimeoutSec 5; Write-Output \"status=$($r.StatusCode)\"; exit 0 } \
                 catch { exit 1 }";
    let denied = fixture.launcher(false);
    let (denied_code, denied_stdout, _) = run(&denied, command(&fixture, probe, &[]));
    assert_ne!(denied_code, Some(0), "a denied container reached the network: {denied_stdout}");

    let allowed = fixture.launcher(true);
    let (allowed_code, _, stderr) = run(&allowed, command(&fixture, probe, &[]));
    assert_eq!(allowed_code, Some(0), "an allowed container could not connect: {stderr}");
}

#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn killing_the_tree_ends_the_process() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("kill");
    let launcher = fixture.launcher(false);
    let mut process = launcher
        .spawn(command(&fixture, "Start-Sleep -Seconds 60", &[]))
        .expect("the command starts");
    thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(process.try_wait().expect("poll"), None, "the command should still be running");
    process.kill_tree().expect("the job is terminated");
    let code = process.wait().expect("the process is reaped");
    assert!(code.is_some(), "a terminated process reports an exit code");
}

/// The walk that makes a workspace readable from inside the container is
/// recorded, so the next process on this machine skips it.
///
/// The record itself is what the skip reads; if the first spawn stopped
/// writing one, every process would go back to walking the whole tree (about
/// two seconds on a 20k-file workspace) and nothing else would fail.
#[test]
#[ignore = "needs AGENT_APPCONTAINER_TEST=1 and `agent sandbox setup`"]
fn the_first_spawn_records_the_workspace_grant() {
    if !enabled() {
        eprintln!("skipped: set AGENT_APPCONTAINER_TEST=1");
        return;
    }
    let fixture = Fixture::new("grant-record");
    let record = fixture.sandbox.join("workspace-grant");
    assert!(!record.exists(), "nothing is recorded before a command runs");

    let launcher = fixture.launcher(false);
    let (code, _, stderr) = run(&launcher, command(&fixture, "Write-Output ok", &[]));
    assert_eq!(code, Some(0), "stderr: {stderr}");

    let text = std::fs::read_to_string(&record).expect("the grant is recorded");
    let workspace = fixture.workspace.to_string_lossy();
    // Every field the skip compares has to be there: a record missing one
    // would never match, and the walk would come back without anything going
    // red.
    assert!(text.contains("version 1\n"), "{text}");
    assert!(text.contains(&format!("workspace {workspace}\n")), "{text}");
    assert!(text.contains("sid S-1-15-2-"), "{text}");
    let machine = std::env::var("COMPUTERNAME").expect("a Windows machine has a name");
    assert!(text.contains(&format!("machine {machine}\n")), "{text}");
    assert!(text.lines().any(|line| line.starts_with("access ")), "{text}");
}
