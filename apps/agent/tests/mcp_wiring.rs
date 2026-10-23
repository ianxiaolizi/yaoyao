//! The MCP adapter's wiring, end to end: a real server process, reached by the
//! assembled binary through the same gateway every built-in tool goes through.
//!
//! The adapter's own tests check the mapping rules and the wire. What only this
//! file can check is the assembly: that a server named in a configuration file
//! is started, registered under the capability it declared, and offered to the
//! caller — and that one asking for a capability the run was never given, one
//! the profile did not enable, and one that does not answer are each *not*.
//!
//! The fake server is a real child process, not a stand-in inside this binary,
//! because a test that fakes the process boundary would not be checking the
//! part of this wiring that is easiest to get wrong.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use tempfile::TempDir;

/// The configuration variables the binary documents. Named here rather than
/// imported, as in `cli_end_to_end.rs`: an integration test cannot see the
/// binary's modules, and these are a deployment contract.
const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// A fake MCP server, an `examples/` target of the adapter's own package.
///
/// It is an example rather than a `src/bin/` binary so that `cargo build
/// --workspace` does not put a test double where product binaries go; it is
/// `cargo test` that builds examples, and that is how the gates run. A run of
/// this package alone does not, so it is built here — a test that quietly
/// skipped itself when a fixture was missing would report success for a wiring
/// it never exercised.
fn fixture_bin(name: &str) -> PathBuf {
    let agent = PathBuf::from(env!("CARGO_BIN_EXE_agent"));
    let directory = agent.parent().expect("the binary is in a directory").join("examples");
    let path = directory.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() });
    if path.exists() {
        return path;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // Build into the same target directory this test binary lives under: a
    // coverage build relocates it, and a default build would not be found here.
    let profile = directory.parent().expect("the profile directory");
    let mut build = Command::new(cargo);
    build
        .args(["build", "-p", "agent-tools-mcp", "--example", name])
        .arg("--target-dir")
        .arg(profile.parent().expect("the target directory"))
        .current_dir(env!("CARGO_MANIFEST_DIR"));
    if profile.ends_with("release") {
        build.arg("--release");
    }
    let built = build.status().expect("cargo is on the path");
    assert!(built.success(), "the {name} fixture could not be built");
    assert!(path.exists(), "{} was not built where this test looks for it", path.display());
    path
}

fn server_bin() -> PathBuf {
    fixture_bin("fake-mcp-server")
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
/// suite gives: a developer with a user configuration exported would have every
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

/// One run launched from `cwd`, which is deliberately *not* the workspace.
///
/// The asymmetry is the point: a configured relative path is relative to the
/// workspace root, so a child started from a launcher somewhere else must still
/// land inside the workspace. A run whose process working directory *was* the
/// workspace could not tell the two rules apart — which is exactly how the
/// relative-path rule went unnoticed for as long as it did.
fn ran_in(cwd: &Path, data: &Path, workspace: &Path, config: &Path, argv: &[&str]) -> Ran {
    let output = Command::new(env!("CARGO_BIN_EXE_agent"))
        .current_dir(cwd)
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

fn write_config(home: &TempDir, text: &str) -> PathBuf {
    let path = home.path().join("user.toml");
    std::fs::write(&path, text).expect("a user configuration");
    path
}

/// A user configuration with one stdio server, and whatever profile a test
/// wants composed on top of it.
///
/// `process.spawn` is the capability these servers declare, and it is the
/// honest one: reaching a stdio server means starting a program. The sandbox
/// advertises it, as it does `network.request` for the `url` servers below, so
/// a refusal here is the profile's decision and not the build's limitation —
/// the fail-closed rule doing its job rather than a wiring fault.
fn config_with(home: &TempDir, user_caps: &str, requires: &str, profile: &str) -> PathBuf {
    write_config(
        home,
        &format!(
            "capabilities = [{user_caps}]\n\
             plugins = [\"mcp.fake\"]\n\n\
             [mcp.servers.fake]\n\
             command = '{command}'\n\
             requires = [{requires}]\n\n\
             [profiles.p]\n{profile}",
            command = server_bin().display(),
        ),
    )
}

fn log_file(data: &Path, session: &str) -> PathBuf {
    data.join(session).join("events.jsonl")
}

/// A running Streamable HTTP fixture, killed when this is dropped.
struct HttpFixture {
    child: Child,
    url: String,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn http_fixture() -> HttpFixture {
    let mut child = Command::new(fixture_bin("fake-mcp-http-server"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("the fixture starts");
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut announced = String::new();
    std::io::BufReader::new(stdout)
        .read_line(&mut announced)
        .expect("the fixture announces its port");
    let address = announced
        .trim()
        .strip_prefix("listening ")
        .unwrap_or_else(|| panic!("the fixture said {announced:?}"))
        .to_string();
    HttpFixture { child, url: format!("http://{address}/mcp") }
}

/// A user configuration with one server reached by `url`, and whatever
/// `headers` line the test wants it to carry.
///
/// `network.request` is the honest declaration for a server reached over a
/// socket. `grant` is whether the profile says yes to it: off by default is not
/// the same as refused, and the tests below are the difference.
fn config_with_http_server(
    home: &TempDir,
    fixture: &HttpFixture,
    headers: &str,
    grant: &str,
) -> PathBuf {
    write_config(
        home,
        &format!(
            "capabilities = [\"network.request\"]\n\
             plugins = [\"mcp.httpfake\"]\n\n\
             [mcp.servers.httpfake]\n\
             url = \"{url}\"\n\
             {headers}\n\
             requires = [\"network.request\"]\n\n\
             [profiles.p]\n\
             {grant}\
             plugins = {{ enable = [\"mcp.httpfake\"] }}\n",
            url = fixture.url,
        ),
    )
}

#[test]
fn a_configured_server_is_discovered_and_its_tools_run_through_the_gateway() {
    let (data, workspace, home) = dirs();
    let config = config_with(
        &home,
        r#""process.spawn""#,
        r#""process.spawn""#,
        "tools = { allow = [\"process.spawn\"] }\n\
         policy = { shell = \"allow\" }\n\
         plugins = { enable = [\"mcp.fake\"] }\n",
    );

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "mcp__fake__echo", r#"{"text":"hello from the wire"}"#],
    );

    // The answer came back from the child process: the server was started, its
    // tools were listed, the call reached it under its own name, and the result
    // came back through the gateway.
    assert!(r.ok, "{}", r.err);
    assert_eq!(r.out.trim(), "hello from the wire");
}

#[test]
fn an_mcp_tool_asks_the_gateway_before_it_runs() {
    let (data, workspace, home) = dirs();
    let config = config_with(
        &home,
        r#""process.spawn""#,
        r#""process.spawn""#,
        "tools = { allow = [\"process.spawn\"] }\n\
         policy = { shell = \"ask\" }\n\
         plugins = { enable = [\"mcp.fake\"] }\n",
    );

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "mcp__fake__echo", r#"{"text":"x"}"#],
    );

    assert!(!r.ok, "an ask is not a run: {}", r.out);
    assert!(r.err.contains("approval pending"), "{}", r.err);

    // The world, not the message: the session gained an approval request naming
    // the MCP tool, which is the same record a built-in tool's question leaves.
    let events = std::fs::read_to_string(log_file(data.path(), "default"))
        .expect("the approval request was recorded");
    assert!(events.contains("approval/requested"), "{events}");
    assert!(events.contains("mcp__fake__echo"), "{events}");
}

#[test]
fn a_server_the_run_is_not_authorized_for_never_starts() {
    let (data, workspace, home) = dirs();
    // The user authorizes nothing, and the profile turns the server on anyway.
    let config =
        config_with(&home, "", r#""process.spawn""#, "plugins = { enable = [\"mcp.fake\"] }\n");

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "status"]);

    assert!(!r.ok, "the run was never authorized to begin: {}", r.out);
    assert!(r.err.contains("not authorized"), "{}", r.err);
    // Nothing happened to the session: a refusal at resolution time is a
    // decision not to run, not a run that failed halfway.
    assert!(!log_file(data.path(), "default").exists(), "no session log was opened");
}

#[test]
fn a_server_that_is_not_enabled_is_not_on_the_surface() {
    let (data, workspace, home) = dirs();
    // Defined and trusted, and the profile does not turn it on: a definition is
    // how to reach a server, and enabling it is whether this run does.
    let config = config_with(&home, r#""process.spawn""#, r#""process.spawn""#, "");

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "mcp__fake__echo", r#"{"text":"x"}"#],
    );

    assert!(!r.ok);
    assert!(r.err.contains("mcp__fake__echo"), "{}", r.err);
    // And the refusal shows the surface it is not on, so the caller can see that
    // the name is missing because nobody enabled the server rather than because
    // it is misspelled — the two look identical from the outside.
    assert!(r.err.contains("unknown tool 'mcp__fake__echo'"), "{}", r.err);
    assert!(r.err.contains("the run registers"), "{}", r.err);
    assert!(r.err.contains("fs.read"), "a builtin is on the surface: {}", r.err);
}

/// A command that leaves a mark when it is started, and never speaks MCP.
///
/// Whether the plugin's program was spawned is a fact about this machine, and
/// this is how the test below reads it. A message could say "not started" while
/// a process was running anyway; the point of the rule is that no process runs.
/// `cwd` is set so the mark is written with a bare name — nothing in the command
/// line needs quoting, and nothing lands in the working directory.
fn marker_command(cwd: &Path) -> String {
    marker_command_with_cwd(&cwd.display().to_string())
}

/// The same marker, with the working directory written into the configuration
/// exactly as given — a relative one, for the test that checks *where* a
/// configured directory lands.
fn marker_command_with_cwd(cwd: &str) -> String {
    let (shell, args) = if cfg!(windows) {
        (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            "args = ['/c', 'echo started > started.txt']",
        )
    } else {
        ("/bin/sh".to_string(), "args = ['-c', 'touch started.txt']")
    };
    format!("command = '{shell}'\n{args}\ncwd = '{cwd}'\nrequires = [\"process.spawn\"]")
}

/// Wait, briefly, for a file a child process wrote.
///
/// The plugin's program is a separate process and the run reports the server
/// unreachable the moment the handshake fails, which can be before the child has
/// finished writing its mark. This waits for the fact rather than racing it.
fn wait_for(path: &Path) -> bool {
    for _ in 0..100 {
        if path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[test]
fn a_session_log_that_cannot_be_read_starts_no_plugin() {
    let (data, workspace, home) = dirs();

    // The session's log is there and cannot be decoded: one complete line that
    // is not an event. Every read of this session fails from here on, which is
    // the state the rule is about — the one thing an unreadable log must not be
    // allowed to mean is "nothing was ever withdrawn".
    let log = log_file(data.path(), "default");
    std::fs::create_dir_all(log.parent().expect("the log has a directory")).unwrap();
    std::fs::write(&log, "this line is not an event\n").expect("a damaged log");

    let config = write_config(
        &home,
        &format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.fake\"]\n\n\
             [mcp.servers.fake]\n{}\n\n\
             [profiles.p]\n\
             tools = {{ allow = [\"process.spawn\"] }}\n\
             plugins = {{ enable = [\"mcp.fake\"] }}\n",
            marker_command(data.path())
        ),
    );

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "run", "hello"]);

    // Said, and said the right way round: this is a fact about the run, not a
    // withdrawal, so the note must not send the operator to `plugin enable`.
    assert!(r.err.contains("could not be read"), "{}", r.err);
    assert!(r.err.contains("mcp.fake"), "{}", r.err);
    assert!(r.err.contains("not started"), "{}", r.err);
    assert!(!r.err.contains("plugin enable"), "{}", r.err);

    // And the plugin's program never ran. This is the defect stated as a fact
    // about the machine rather than as a sentence: before this rule, a failed
    // read meant an empty set of withdrawals, and an empty set meant *start
    // everything the configuration enables*.
    assert!(
        !data.path().join("started.txt").exists(),
        "the plugin's program was spawned off a log nobody could read: {}",
        r.err
    );
}

#[test]
fn a_configured_working_directory_is_resolved_against_the_workspace_not_the_launcher() {
    // A relative `cwd` in a configuration file is a path in a configuration
    // file, and those are relative to the workspace root. The child would
    // otherwise inherit the launcher's directory — so this run is launched from
    // somewhere that is not the workspace, with a `sub` of its own, and the mark
    // the plugin writes is looked for in both places.
    let (data, workspace, home) = dirs();
    let launcher = TempDir::new().unwrap();
    std::fs::create_dir_all(workspace.path().join("sub")).unwrap();
    std::fs::create_dir_all(launcher.path().join("sub")).unwrap();

    let config = write_config(
        &home,
        &format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.fake\"]\n\n\
             [mcp.servers.fake]\n{}\n\n\
             [profiles.p]\n\
             tools = {{ allow = [\"process.spawn\"] }}\n\
             policy = {{ shell = \"allow\" }}\n\
             plugins = {{ enable = [\"mcp.fake\"] }}\n",
            marker_command_with_cwd("sub")
        ),
    );

    let r = ran_in(
        launcher.path(),
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "run", "hello"],
    );

    // The marker program never speaks MCP, so the server is reported and
    // skipped. What is being checked is *where* its program was started.
    assert!(r.err.contains("mcp server 'fake' is not available"), "{}", r.err);
    assert!(
        wait_for(&workspace.path().join("sub").join("started.txt")),
        "the configured relative directory did not resolve inside the workspace: {}",
        r.err
    );
    assert!(
        !launcher.path().join("sub").join("started.txt").exists(),
        "the configured directory resolved against the launcher, not the workspace: {}",
        r.err
    );
}

#[test]
fn a_configured_working_directory_outside_the_workspace_starts_no_plugin() {
    // The boundary that holds every path a tool is handed holds this one too: a
    // server cannot be launched outside the workspace by configuration alone.
    // Nothing is started, so the note is the whole of the evidence — and it is
    // the deterministic half of what the test above shows with a process.
    let (data, workspace, home) = dirs();
    let elsewhere = TempDir::new().unwrap();
    let config = write_config(
        &home,
        &format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.fake\"]\n\n\
             [mcp.servers.fake]\n{}\n\n\
             [profiles.p]\n\
             tools = {{ allow = [\"process.spawn\"] }}\n\
             policy = {{ shell = \"allow\" }}\n\
             plugins = {{ enable = [\"mcp.fake\"] }}\n",
            marker_command_with_cwd(&elsewhere.path().display().to_string())
        ),
    );

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "run", "hello"]);

    // Reported and skipped, not fatal — the run has no other server here, and a
    // misconfigured one is not a reason to refuse to start.
    assert!(r.ok, "{}", r.err);
    assert!(r.err.contains("mcp server 'fake' is not started"), "{}", r.err);
    assert!(r.err.contains("path outside workspace"), "{}", r.err);
    // The refusal says where the path would have had to be, which is the whole
    // of the remedy the operator has.
    assert!(
        r.err.contains(&workspace.path().canonicalize().unwrap().display().to_string()),
        "the refusal names the workspace root: {}",
        r.err
    );
    // And no process ran: the program would have written its mark in the
    // directory the configuration named.
    assert!(
        !elsewhere.path().join("started.txt").exists(),
        "a refused working directory still started the program: {}",
        r.err
    );
}

#[test]
fn a_server_that_does_not_answer_is_reported_and_the_run_continues() {
    let (data, workspace, home) = dirs();
    let config = write_config(
        &home,
        &format!(
            r#"capabilities = ["process.spawn"]
plugins = ["mcp.ghost"]

[mcp.servers.ghost]
command = '{}'
requires = ["process.spawn"]

[profiles.p]
tools = {{ allow = ["process.spawn"] }}
policy = {{ shell = "allow" }}
plugins = {{ enable = ["mcp.ghost"] }}
"#,
            home.path().join("no-such-program").display()
        ),
    );

    // The run itself is not refused: a third-party server being down is not a
    // reason to stop, and the model simply does not get its tools.
    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "run", "hello"]);
    assert!(r.ok, "{}", r.err);
    assert!(r.err.contains("mcp server 'ghost' is not available"), "{}", r.err);

    // And the tool genuinely is not there — a registration that never happened
    // is not a name that resolves to nothing quietly.
    let absent = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "mcp__ghost__echo", "{}"],
    );
    // `2` and not `1`: the call never left this process, so there is no answer
    // for a script to read, only a reason. Reporting the two the same way would
    // send a caller looking at stdout for something that was never put there.
    assert_eq!(
        absent.code,
        Some(2),
        "a call that never ran said {:?} ({})",
        absent.code,
        absent.err
    );
    assert!(absent.err.contains("mcp__ghost__echo"), "{}", absent.err);
}

#[test]
fn a_server_reached_by_url_is_reached_through_the_configuration() {
    let (data, workspace, home) = dirs();
    let fixture = http_fixture();
    let config = config_with_http_server(
        &home,
        &fixture,
        r#"headers = { x-yaoyao-test = "present" }"#,
        "tools = { allow = [\"network.request\"] }\n",
    );

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "mcp__httpfake__stats", "{}"],
    );

    // Reached: the server answered the handshake through the transport the
    // configuration named, so its tools were registered. A `url` that could not
    // be spoken to would have said so here instead.
    assert!(!r.err.contains("is not available"), "{}", r.err);

    // And run. Reaching an HTTP server and running its tools are two gates, and
    // this configuration opens both: the server declares `network.request`, the
    // profile's `tools.allow` grants it, and the sandbox advertises it because
    // this build really does speak HTTP. It was the third of those that was
    // missing, which is why this call used to be refused as "not provided by
    // this build's execution sandbox" — a fact about the build rather than the
    // decision a reader would have had to go and make.
    assert!(r.ok, "the configured URL server did not run the tool: {}", r.err);

    // The answer is the server's own account of what it has been sent, which no
    // client-side double could produce. `header` is the value it read off the
    // wire: the configured header, on a request that left this process.
    assert!(r.out.contains("\"header\":\"present\""), "{}", r.out);
    assert!(!r.out.contains("\"posts\":0"), "the server was never posted to: {}", r.out);
}

#[test]
fn a_url_server_missing_the_configured_header_is_not_reached() {
    let (data, workspace, home) = dirs();
    let fixture = http_fixture();
    // The same server, the same url, and the one line that made the difference
    // above left out.
    let config = config_with_http_server(&home, &fixture, "", "");

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "run", "hello"]);

    // A third-party server being unreachable is not a reason to stop the run.
    assert!(r.ok, "{}", r.err);
    assert!(r.err.contains("mcp server 'httpfake' is not available"), "{}", r.err);
    // The refusal is the server's own, and it says what it wanted — so this is
    // the configured header missing on the wire, not something else about the
    // transport.
    assert!(r.err.contains("403"), "{}", r.err);
    assert!(r.err.contains("x-yaoyao-test"), "{}", r.err);
}

#[test]
fn a_url_server_the_profile_never_granted_is_reached_but_refused() {
    let (data, workspace, home) = dirs();
    let fixture = http_fixture();
    // The server, the url and the header of the test above, with the profile's
    // one line left out. The run is authorized for the capability — that is the
    // run-level `capabilities` list, and it is what lets the server register at
    // all — but no rule grants it.
    let config =
        config_with_http_server(&home, &fixture, r#"headers = { x-yaoyao-test = "present" }"#, "");

    let r = ran(
        data.path(),
        workspace.path(),
        &config,
        &["--profile", "p", "tool", "mcp__httpfake__stats", "{}"],
    );

    assert_eq!(r.code, Some(2), "a denied call said {:?} ({})", r.code, r.err);
    assert!(r.err.contains("network.request"), "{}", r.err);
    // Refused as *ungranted*, and that word is the whole point: the build
    // provides this capability — an MCP server over `url` is why the sandbox
    // advertises it — and no rule said yes. While the sandbox did not advertise
    // it, the same configuration was refused as "not provided by this build's
    // execution sandbox", which sent a reader looking for a capability this
    // build does not have instead of a decision nobody had made.
    assert!(r.err.contains("is not granted by any policy rule"), "{}", r.err);
    assert!(!r.err.contains("not provided by this build"), "{}", r.err);
    // Refused at the gate, not after the fact: the server was reached, and it
    // was never asked to run anything.
    assert!(!r.out.contains("\"posts\":"), "the tool ran anyway: {}", r.out);
}

/// A refusal after the servers would have started leaves no server behind.
///
/// The order inside assembly is the subject (AA-07): MCP registration spawns
/// child processes, and a decision backend the adapter will not dial is a
/// refusal that belongs *before* that spawn. The proof is the session log — a
/// server that was started and registered writes `plugin/loaded`, so an
/// ordering that started first would leave that event behind even though the
/// command exited 2.
#[test]
fn a_refused_assembly_stops_before_any_server_is_started() {
    let (data, workspace, home) = dirs();
    let config = write_config(
        &home,
        &format!(
            "capabilities = [\"process.spawn\"]\n\
             plugins = [\"mcp.fake\"]\n\n\
             [mcp.servers.fake]\n\
             command = '{command}'\n\
             requires = [\"process.spawn\"]\n\n\
             [decisions.laya]\n\
             url = \"http://laya.internal:8000\"\n\n\
             [profiles.p]\n\
             plugins = {{ enable = [\"mcp.fake\"] }}\n",
            command = server_bin().display(),
        ),
    );

    let r = ran(data.path(), workspace.path(), &config, &["--profile", "p", "status"]);

    assert_eq!(r.code, Some(2), "assembly must refuse to run: {}", r.err);
    assert!(r.err.to_lowercase().contains("loopback"), "{}", r.err);

    let log = std::fs::read_to_string(log_file(data.path(), "default")).unwrap_or_default();
    assert!(
        !log.contains("plugin/loaded"),
        "a server was started and registered before the refusal: {log}"
    );
}
