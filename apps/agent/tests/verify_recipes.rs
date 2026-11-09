//! Verifier recipes, end to end: the real binary, the real user layer, the
//! real program it starts.
//!
//! The unit tests in `commands/verify.rs` expand a recipe through a table
//! handed to them. These spawn the assembled binary and let it read
//! `AGENT_USER_CONFIG` the way a person's shell would, because the placement of
//! that file is the invariant: a recipe is a check a *person* wrote, and the
//! workspace layer — the one the work being graded can edit — must not be able
//! to define one.
//!
//! Each test gets its own data, workspace, and configuration directories, so a
//! developer's exported configuration cannot reach these runs and these runs
//! cannot reach each other.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

fn agent(data: &Path, workspace: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent"));
    command
        .env("AGENT_DATA_DIR", data)
        .env("AGENT_WORKSPACE_DIR", workspace)
        .env("AGENT_MODEL_PROVIDER", "echo")
        .env_remove(ENV_USER_CONFIG)
        .env_remove(ENV_MANAGED_POLICY)
        .env_remove("AGENT_SANDBOX");
    command
}

struct Ran {
    code: Option<i32>,
    out: String,
    err: String,
}

fn ran(data: &Path, workspace: &Path, env: &[(&str, String)], argv: &[&str]) -> Ran {
    let mut command = agent(data, workspace);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.args(argv).output().expect("the binary starts");
    Ran {
        code: output.status.code(),
        out: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        err: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    }
}

fn dirs() -> (TempDir, TempDir, TempDir) {
    (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap())
}

fn write_config(home: &TempDir, name: &str, text: &str) -> String {
    let path = home.path().join(name);
    std::fs::write(&path, text).expect("a configuration");
    path.display().to_string()
}

fn write_project_config(workspace: &Path, text: &str) {
    let dir = workspace.join(".agent");
    std::fs::create_dir_all(&dir).expect("a project directory");
    std::fs::write(dir.join("config.toml"), text).expect("a project configuration");
}

/// A program that exits with `code` and nothing else.
///
/// The check runs a real process, so the test names one every platform has:
/// `cmd` on Windows, `sh` elsewhere. It reads no file and prints nothing, which
/// keeps the test about the recipe plumbing rather than about a toolchain.
fn exit_command(code: i32) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        ("cmd".to_string(), vec!["/C".to_string(), format!("exit {code}")])
    }
    #[cfg(not(windows))]
    {
        ("sh".to_string(), vec!["-c".to_string(), format!("exit {code}")])
    }
}

fn recipe_toml(name: &str, code: i32) -> String {
    let (program, args) = exit_command(code);
    let args: Vec<String> = args.iter().map(|arg| format!("{arg:?}")).collect();
    format!("[verify.recipes.{name}]\nprogram = {program:?}\nargs = [{}]\n", args.join(", "))
}

fn user_env(path: &str) -> Vec<(&'static str, String)> {
    vec![(ENV_USER_CONFIG, path.to_string())]
}

/// The chain's events, as one JSON object per line.
fn chain(out: &str) -> Vec<serde_json::Value> {
    out.lines()
        .filter(|line| line.trim_start().starts_with('{'))
        .map(|line| serde_json::from_str(line).expect("a chain event"))
        .collect()
}

fn recorded(events: &[serde_json::Value]) -> &serde_json::Value {
    events
        .iter()
        .find(|event| event["type"] == "verification/recorded")
        .expect("the check was recorded")
}

#[test]
fn a_recipe_that_passes_records_evidence_and_exits_zero() {
    let (data, workspace, config) = dirs();
    let path = write_config(&config, "user.toml", &recipe_toml("ok", 0));

    let r = ran(
        data.path(),
        workspace.path(),
        &user_env(&path),
        &["verify", "--recipe", "ok", "--target", "artifact.html", "--events"],
    );

    assert_eq!(r.code, Some(0), "stderr: {}", r.err);
    let events = chain(&r.out);
    let evidence = recorded(&events);
    assert_eq!(evidence["payload"]["status"], "passed");
    assert_eq!(evidence["payload"]["exit_code"], 0);
    assert_eq!(
        evidence["payload"]["args"].as_array().expect("args").len(),
        3,
        "the target is appended when the recipe names no placeholder: {}",
        evidence["payload"]["args"]
    );
    assert!(
        evidence["payload"]["target"].as_str().unwrap_or_default().contains("artifact.html"),
        "the record says what it checked: {}",
        evidence["payload"]["target"]
    );
}

#[test]
fn a_recipe_that_fails_is_recorded_as_failed_and_exits_one() {
    let (data, workspace, config) = dirs();
    let path = write_config(&config, "user.toml", &recipe_toml("broken", 3));

    let r = ran(
        data.path(),
        workspace.path(),
        &user_env(&path),
        &["verify", "--recipe", "broken", "--events"],
    );

    assert_eq!(r.code, Some(1), "a check that ran and did not pass is an answer: {}", r.err);
    let events = chain(&r.out);
    let evidence = recorded(&events);
    assert_eq!(evidence["payload"]["status"], "failed");
    assert_eq!(evidence["payload"]["exit_code"], 3);
}

#[test]
fn a_recipe_that_reports_did_not_run_exits_two_and_records_an_error() {
    let (data, workspace, config) = dirs();
    let path = write_config(&config, "user.toml", &recipe_toml("not-run", 2));

    let r = ran(
        data.path(),
        workspace.path(),
        &user_env(&path),
        &["verify", "--recipe", "not-run", "--events"],
    );

    assert_eq!(r.code, Some(2), "a check that did not run is not an answer: {}", r.out);
    let events = chain(&r.out);
    let evidence = recorded(&events);
    assert_eq!(evidence["payload"]["status"], "error");
    assert_eq!(evidence["payload"]["exit_code"], 2);
    assert!(
        evidence["payload"]["reason"].as_str().unwrap_or_default().contains("did not run"),
        "{}",
        evidence["payload"]
    );
}

#[test]
fn an_unknown_recipe_exits_two_and_names_the_ones_that_exist() {
    let (data, workspace, config) = dirs();
    let mut text = recipe_toml("ok", 0);
    text.push_str(&recipe_toml("video", 0));
    let path = write_config(&config, "user.toml", &text);

    let r = ran(data.path(), workspace.path(), &user_env(&path), &["verify", "--recipe", "web"]);

    assert_eq!(r.code, Some(2), "nothing ran: {}", r.out);
    assert!(r.err.contains("no recipe 'web'"), "{}", r.err);
    assert!(r.err.contains("ok"), "the known names are listed: {}", r.err);
    assert!(r.err.contains("video"), "the known names are listed: {}", r.err);
}

#[test]
fn a_user_config_inside_the_workspace_is_refused() {
    let (data, workspace, _config) = dirs();
    let path = workspace.path().join("user.toml");
    std::fs::write(&path, recipe_toml("ok", 0)).expect("the in-workspace user config");
    let path = path.display().to_string();
    let env = user_env(&path);

    let verify =
        ran(data.path(), workspace.path(), &env, &["verify", "--recipe", "ok", "--events"]);
    assert_eq!(verify.code, Some(2), "the recipe must not run: {}", verify.out);
    assert!(verify.err.contains("inside the model-writable workspace"), "{}", verify.err);

    // The same path is refused before any command assembles a run, not only
    // by the verify command.
    let run = ran(data.path(), workspace.path(), &env, &["run", "hello"]);
    assert_eq!(run.code, Some(2), "{}", run.out);
    assert!(run.err.contains("inside the model-writable workspace"), "{}", run.err);
}

#[test]
fn a_project_may_not_define_the_check_its_work_is_graded_by() {
    // The user layer is empty, so `web` exists only if the workspace could
    // define it. It must not: the command refuses the name rather than running
    // a program the repository named.
    let (data, workspace, config) = dirs();
    let path = write_config(&config, "user.toml", "");
    write_project_config(workspace.path(), &recipe_toml("web", 0));

    let r = ran(data.path(), workspace.path(), &user_env(&path), &["verify", "--recipe", "web"]);

    assert_eq!(r.code, Some(2), "the workspace recipe must not run: {}", r.out);
    assert!(r.err.contains("no recipe 'web'"), "{}", r.err);

    // And the section is refused outright when the configuration is loaded, so
    // a project that writes one learns about it on its next command.
    let loaded = ran(data.path(), workspace.path(), &user_env(&path), &["run", "hello"]);
    assert_eq!(loaded.code, Some(2), "the project layer is read and refused: {}", loaded.out);
    assert!(loaded.err.contains("unknown field"), "the refusal names the field: {}", loaded.err);
}

#[test]
fn a_command_line_flag_overrides_the_recipe_it_was_run_with() {
    let (data, workspace, config) = dirs();
    let path = write_config(&config, "user.toml", &recipe_toml("ok", 0));

    let r = ran(
        data.path(),
        workspace.path(),
        &user_env(&path),
        &["verify", "--recipe", "ok", "--timeout-ms", "0"],
    );

    assert_eq!(r.code, Some(2), "zero is a check that never starts: {}", r.out);
    assert!(r.err.contains("positive"), "{}", r.err);
}
