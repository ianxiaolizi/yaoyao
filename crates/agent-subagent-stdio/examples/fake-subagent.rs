//! A stand-in for an external agent, used by the live tests.
//!
//! It encodes the wire by hand and never imports this crate's types, on purpose:
//! the tests are about whether two *independent* implementations of the protocol
//! agree, and a fixture that shared the parent's own types would agree with it by
//! construction, including where both were wrong. So the JSON below is written
//! out as text, and the request is read as a `serde_json::Value` rather than as
//! anything this crate defines.
//!
//! Its modes exist to be things a real child does: answer, answer with less than
//! it was asked for, fail after changing something, say it is still working,
//! write something the wire does not define, die quietly, or refuse to die at
//! all while a descendant of it keeps working.
//!
//! Everything it says to a person goes to stderr. Stdout is the wire.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "result".to_string());
    let argument = std::env::args().nth(2);

    // The heartbeat is the grandchild, not the child: it is started by the
    // mode that wants something to outlive the direct child, and is what makes
    // process-tree teardown observable from outside.
    if mode == "heartbeat" {
        heartbeat(&argument.expect("a heartbeat needs a file to write to"));
        return;
    }

    let request = read_request();

    match mode.as_str() {
        "result" => {
            answer(&request);
            // The child has answered and does not exit. Nothing in the parent
            // may be waiting for it to; this is the fixture's proof of that.
            sleep_forever();
        }
        "result-exit" => {
            answer(&request);
        }
        "partial" => {
            progress("read half of it");
            println!(
                r#"{{"type":"result","status":"partial","summary":"two of the three notes"}}"#
            );
        }
        "fail" => {
            stdout_line(
                r#"{"type":"result","status":"failed","message":"the notes directory is empty","side_effects":["created notes/summary.md"]}"#,
            );
        }
        "garbage" => {
            // Each of these is a different kind of disagreement, and the parent
            // has to treat every one as a violation rather than as noise.
            match argument.as_deref().unwrap_or("not-json") {
                "not-json" => stdout_line("this is not json at all"),
                "unknown-type" => {
                    stdout_line(r#"{"type":"final","status":"completed","summary":"done"}"#)
                }
                "unknown-status" => {
                    stdout_line(r#"{"type":"result","status":"cancelled","summary":"done"}"#)
                }
                "no-summary" => stdout_line(r#"{"type":"result","status":"completed"}"#),
                other => {
                    eprintln!("unknown garbage mode '{other}'");
                    std::process::exit(2);
                }
            }
            // Still running when the parent reads it: the parent must react to
            // the line, not wait for the child to give up.
            sleep_forever();
        }
        "silent-exit" => {
            eprintln!("fake-subagent: nothing to say about that");
            std::process::exit(3);
        }
        "stubborn" => {
            let path = argument.expect("the stubborn mode needs a file for its grandchild");
            start_grandchild(&path);
            // Never answers, never exits: only the parent can end this.
            sleep_forever();
        }
        other => {
            eprintln!("fake-subagent: unknown mode '{other}'");
            std::process::exit(2);
        }
    }
}

/// Read the parent's one line, and confirm it is the only one.
///
/// The protocol says stdin closes after the request. A child that never noticed
/// would still appear to work, so the fixture checks it and complains on stderr
/// — where it costs the wire nothing.
fn read_request() -> serde_json::Value {
    let stdin = std::io::stdin();
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line).expect("the request is readable");
    if read == 0 {
        eprintln!("fake-subagent: no request arrived on stdin");
        std::process::exit(2);
    }

    let mut extra = String::new();
    let more = stdin.lock().read_line(&mut extra).expect("stdin is readable");
    if more > 0 {
        eprintln!("fake-subagent: a second line arrived after the request: {extra:?}");
    }

    serde_json::from_str(&line).unwrap_or_else(|error| {
        eprintln!("fake-subagent: the request was not JSON ({error}): {}", line.trim());
        std::process::exit(2);
    })
}

/// Answer with the request it was given, so the test can check from this side
/// what actually arrived.
fn answer(request: &serde_json::Value) {
    let objective = request.get("objective").and_then(|value| value.as_str()).unwrap_or_default();
    progress("read the request");

    // Built as text rather than by serializing a struct: this file is an
    // independent implementation of the wire, and it should fail if the parent's
    // idea of the wire changes without this one changing too.
    let line = serde_json::json!({
        "type": "result",
        "status": "completed",
        "summary": objective,
        "result": request,
    });
    stdout_line(&line.to_string());
}

fn progress(message: &str) {
    let line = serde_json::json!({ "type": "progress", "message": message });
    stdout_line(&line.to_string());
}

fn stdout_line(line: &str) {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    writeln!(stdout, "{line}").expect("stdout is writable");
    stdout.flush().expect("stdout is flushable");
}

/// Start the grandchild that keeps writing, detached from this process's own
/// stdout so it can never be mistaken for part of the wire.
///
/// Waiting on it is the one thing this must not do: the grandchild exists to
/// outlive the child, and the test is whether the *parent* can reach it. A
/// fixture that reaped its own descendant would answer that question by making
/// the descendant disappear.
#[allow(clippy::zombie_processes)]
fn start_grandchild(path: &str) {
    let exe = std::env::current_exe().expect("this binary has a path");
    Command::new(exe)
        .args(["heartbeat", path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the heartbeat starts");
}

/// Write to a file forever, so that "is it still running?" is a question with an
/// answer anyone outside can read.
fn heartbeat(path: &str) {
    let path = Path::new(path);
    loop {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(b".");
            let _ = file.flush();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sleep_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
