//! Help is a successful read-only answer, not an unknown command.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent")).args(args).output().expect("run the agent binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn top_level_help_flags_are_successful_answers() {
    for args in [&["--help"][..], &["-h"][..], &["help"][..]] {
        let output = run(args);
        assert!(
            output.status.success(),
            "args {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout(&output).contains("Usage: agent <command> [options]"), "args {args:?}");
    }
}

#[test]
fn command_help_names_the_command_without_running_it() {
    for (args, expected) in [
        (&["help", "run"][..], "Usage: agent run <text>"),
        (&["run", "--help"][..], "Usage: agent run <text>"),
        (&["status", "-h"][..], "Usage: agent status"),
    ] {
        let output = run(args);
        assert!(
            output.status.success(),
            "args {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout(&output).contains(expected), "args {args:?}: {}", stdout(&output));
    }
}
