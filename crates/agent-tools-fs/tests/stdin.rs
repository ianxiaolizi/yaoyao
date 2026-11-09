//! A closed stdin must be EOF, not the terminal the agent itself inherited.
//!
//! This is observable: the probe command reads stdin before it writes its
//! result. If `StdinMode::Closed` inherits a real console, the probe waits
//! forever; if it receives a closed/null handle, it finishes immediately.

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use agent_ports::process_launcher::{ProcessLauncher, SpawnRequest, StdinMode};
use agent_tools_fs::StdProcessLauncher;

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

fn probe() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        let program = Path::new(&root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        (
            program.to_string_lossy().into_owned(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                "[Console]::In.ReadToEnd() | Out-Null; [Console]::Out.Write('closed')".to_string(),
            ],
        )
    }
    #[cfg(not(windows))]
    {
        ("/bin/sh".to_string(), vec!["-c".to_string(), "cat >/dev/null; printf closed".to_string()])
    }
}

#[test]
fn a_closed_stdin_is_eof_not_the_parent_terminal() {
    let (program, args) = probe();
    let request = SpawnRequest {
        program,
        args,
        cwd: std::env::current_dir().expect("current directory"),
        env: std::env::vars_os().collect(),
        stdin: StdinMode::Closed,
    };
    let mut child = StdProcessLauncher.spawn(request).expect("start the stdin probe");
    let mut stdout = child.take_stdout().expect("the probe's stdout");
    let mut stderr = child.take_stderr().expect("the probe's stderr");
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let code = loop {
        if let Some(code) = child.try_wait().expect("probe status") {
            break code;
        }
        if Instant::now() >= deadline {
            let _ = child.kill_tree();
            let _ = child.wait();
            panic!("a child with closed stdin waited for the parent terminal");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    let mut err = String::new();
    stdout.read_to_string(&mut out).expect("probe stdout text");
    stderr.read_to_string(&mut err).expect("probe stderr text");
    assert_eq!(code, 0, "stdout: {out:?}; stderr: {err:?}");
    assert_eq!(out, "closed", "stderr: {err:?}");
}
