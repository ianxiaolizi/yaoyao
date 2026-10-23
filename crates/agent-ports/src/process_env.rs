//! Which environment variables a child process may inherit.
//!
//! A spawned command inherits the agent process's environment by default, and
//! that environment is where the agent keeps its own credentials
//! (`AGENT_MODEL_API_KEY`) and where the operator's machine keeps theirs. A
//! command that can read the whole environment can put a key into its output,
//! and a tool result is durable: the secret would then be in the event log, in
//! the model's next request, and in every export of the session.
//!
//! The rule here is an **allowlist**, not a denylist. A denylist has to guess
//! every name a secret might be stored under and is wrong the first time it
//! misses one; an allowlist passes only what a build tool has a documented
//! reason to read. The deny check below is defense in depth — it exists so that
//! a future allowlist addition cannot quietly reintroduce a secret-shaped name.
//!
//! Two things are never passed even when the caller configured them explicitly:
//! the agent's own `AGENT_*` variables (they name the run's data directory, its
//! model key and its sandbox, none of which a tool may see) and
//! `RUSTUP_HOME`/`CARGO_HOME`, whose directories contain credentials that a
//! child has no business reading.

use std::ffi::OsString;

/// Variables a child may inherit on Windows. Names are compared
/// case-insensitively, which is how Windows treats them.
#[cfg(windows)]
const WINDOWS_ALLOWED: &[&str] = &[
    "PATH",
    "PATHEXT",
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "COMSPEC",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "OS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    "NUMBER_OF_PROCESSORS",
    "USERNAME",
    "USERDOMAIN",
    "HOME",
    "LANG",
    "LC_ALL",
];

/// Variables a child may inherit everywhere else. Names are compared exactly,
/// because that is how a POSIX environment treats them.
#[cfg(not(windows))]
const UNIX_ALLOWED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TZ",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TERM",
];

/// Windows resolves a bare command against the current directory even when it
/// is not on `PATH`, which turns "run `cargo`" in a writable workspace into
/// "run whatever `cargo.exe` someone committed". This variable disables that
/// search. It is set on every child whether or not the parent had it.
#[cfg(windows)]
const NO_CWD_EXE_SEARCH: &str = "NoDefaultCurrentDirectoryInExePath";

/// The environment for a child of this process.
pub fn child_environment(parent: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    child_environment_with(parent, &[])
}

/// The environment for a child, plus the variables the caller configured for it.
///
/// Configured variables are the author's explicit choice and are passed through
/// as given — an MCP server that needs a token is a server the operator meant to
/// give one. The one exception is the agent's own `AGENT_*` namespace: a
/// configured child has the same reason not to see the run's model key as an
/// implicit one.
pub fn child_environment_with(
    parent: &[(OsString, OsString)],
    explicit: &[(OsString, OsString)],
) -> Vec<(OsString, OsString)> {
    let mut kept: Vec<(OsString, OsString)> =
        parent.iter().filter(|(name, _)| inheritable(name)).cloned().collect();

    for (name, value) in explicit {
        if is_agent_variable(name) {
            continue;
        }
        // A later entry wins, the way `Command::env` calls stack.
        match kept.iter_mut().find(|(existing, _)| same_name(existing, name)) {
            Some(slot) => slot.1 = value.clone(),
            None => kept.push((name.clone(), value.clone())),
        }
    }

    #[cfg(windows)]
    {
        let name = OsString::from(NO_CWD_EXE_SEARCH);
        match kept.iter_mut().find(|(existing, _)| same_name(existing, &name)) {
            Some(slot) => slot.1 = OsString::from("1"),
            None => kept.push((name, OsString::from("1"))),
        }
    }

    kept.sort_by_key(|(name, _)| normalised(name));
    kept
}

/// Whether a name is on the allowlist.
fn inheritable(name: &OsString) -> bool {
    if is_agent_variable(name) || is_secret_shaped(name) {
        return false;
    }
    #[cfg(windows)]
    {
        let upper = normalised(name);
        WINDOWS_ALLOWED.iter().any(|allowed| upper == *allowed)
    }
    #[cfg(not(windows))]
    {
        let raw = name.to_string_lossy();
        UNIX_ALLOWED.iter().any(|allowed| raw == *allowed)
    }
}

/// The agent's own namespace, which no child sees.
fn is_agent_variable(name: &OsString) -> bool {
    let upper = normalised(name);
    upper.starts_with("AGENT_")
}

/// Names that look like credentials. Unreachable for today's allowlist by
/// construction — it is the guard for tomorrow's.
fn is_secret_shaped(name: &OsString) -> bool {
    let upper = normalised(name);
    let secret_suffix = [
        "_API_KEY",
        "_APIKEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PASSWD",
        "_CREDENTIAL",
        "_CREDENTIALS",
    ];
    if secret_suffix.iter().any(|suffix| upper.ends_with(suffix)) {
        return true;
    }
    const SECRET_PREFIXES: &[&str] = &[
        "AWS_",
        "AZURE_",
        "GOOGLE_",
        "GITHUB_",
        "GH_",
        "OPENAI_",
        "ANTHROPIC_",
        "DOCKER_",
        "SSH_",
        "GPG_",
        "GIT_",
        "RUSTUP_",
        "CARGO_",
    ];
    if SECRET_PREFIXES.iter().any(|prefix| upper.starts_with(prefix)) {
        return true;
    }
    const SECRET_EXACT: &[&str] =
        &["RUSTUP_HOME", "CARGO_HOME", "DISPLAY", "WAYLAND_DISPLAY", "SSH_AUTH_SOCK", "KRB5CCNAME"];
    SECRET_EXACT.iter().any(|exact| upper == *exact)
}

/// Case-folded only where the platform does: Windows environment names are
/// case-insensitive; POSIX ones are not.
fn normalised(name: &OsString) -> String {
    let name = name.to_string_lossy();
    #[cfg(windows)]
    {
        name.to_ascii_uppercase()
    }
    #[cfg(not(windows))]
    {
        name.into_owned()
    }
}

fn same_name(a: &OsString, b: &OsString) -> bool {
    normalised(a) == normalised(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(name), OsString::from(value))
    }

    fn names(env: &[(OsString, OsString)]) -> Vec<String> {
        env.iter().map(|(name, _)| name.to_string_lossy().into_owned()).collect()
    }

    fn value_of(env: &[(OsString, OsString)], name: &str) -> Option<String> {
        env.iter()
            .find(|(key, _)| normalised(key) == normalised(&OsString::from(name)))
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    #[test]
    fn the_agent_key_never_reaches_a_child() {
        let parent = vec![
            pair("PATH", "/bin"),
            pair("AGENT_MODEL_API_KEY", "sk-live-secret"),
            pair("AGENT_DATA_DIR", "/tmp/data"),
        ];
        let env = child_environment(&parent);
        let names = names(&env);
        assert!(names.contains(&"PATH".to_string()));
        assert!(!names.contains(&"AGENT_MODEL_API_KEY".to_string()));
        assert!(!names.contains(&"AGENT_DATA_DIR".to_string()));
    }

    #[test]
    fn host_secrets_of_other_shapes_never_reach_a_child() {
        let parent = vec![
            pair("PATH", "/bin"),
            pair("OPENAI_API_KEY", "sk-x"),
            pair("SOME_SERVICE_TOKEN", "t"),
            pair("MY_PASSWORD", "p"),
        ];
        let names = names(&child_environment(&parent));
        assert!(names.contains(&"PATH".to_string()));
        assert!(!names.contains(&"OPENAI_API_KEY".to_string()));
        assert!(!names.contains(&"SOME_SERVICE_TOKEN".to_string()));
        assert!(!names.contains(&"MY_PASSWORD".to_string()));
    }

    #[test]
    fn cargo_credentials_are_not_reachable_through_the_child_environment() {
        let parent = vec![
            pair("PATH", "/bin"),
            pair("CARGO_HOME", "/home/me/.cargo"),
            pair("RUSTUP_HOME", "/home/me/.rustup"),
        ];
        let names = names(&child_environment(&parent));
        assert!(names.contains(&"PATH".to_string()));
        assert!(!names.contains(&"CARGO_HOME".to_string()));
        assert!(!names.contains(&"RUSTUP_HOME".to_string()));
    }

    #[test]
    fn a_configured_variable_is_passed_but_the_agent_namespace_is_not() {
        let parent = vec![pair("PATH", "/bin")];
        let explicit = vec![
            pair("HOSTNAME_FOR_SERVER", "example"),
            pair("AGENT_MODEL_API_KEY", "sk-live-secret"),
        ];
        let env = child_environment_with(&parent, &explicit);
        let names = names(&env);
        assert!(names.contains(&"HOSTNAME_FOR_SERVER".to_string()));
        assert!(!names.contains(&"AGENT_MODEL_API_KEY".to_string()));
    }

    #[test]
    fn an_explicit_value_replaces_an_inherited_one_instead_of_duplicating_it() {
        let parent = vec![pair("PATH", "/bin")];
        let explicit = vec![pair("PATH", "/opt/bin")];
        let env = child_environment_with(&parent, &explicit);
        assert_eq!(env.iter().filter(|(name, _)| normalised(name) == "PATH").count(), 1);
        assert_eq!(value_of(&env, "PATH").as_deref(), Some("/opt/bin"));
    }

    #[test]
    fn no_allowlisted_name_is_secret_shaped() {
        // The allowlist is the mechanism; this keeps the deny check from being
        // the only thing standing between a future entry and a leaked key.
        #[cfg(windows)]
        let allowed = WINDOWS_ALLOWED;
        #[cfg(not(windows))]
        let allowed = UNIX_ALLOWED;
        for name in allowed {
            let name = OsString::from(*name);
            assert!(!is_secret_shaped(&name), "allowlisted a secret-shaped name: {name:?}");
            assert!(!is_agent_variable(&name), "allowlisted the agent namespace: {name:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_children_never_search_the_current_directory_for_programs() {
        let env = child_environment(&[pair("PATH", r"C:\Windows")]);
        assert_eq!(
            value_of(&env, "NoDefaultCurrentDirectoryInExePath").as_deref(),
            Some("1"),
            "a workspace can otherwise supply the executable a bare command names"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_names_are_compared_exactly() {
        let env = child_environment(&[pair("path", "/bin"), pair("PATH", "/usr/bin")]);
        let names = names(&env);
        assert!(names.contains(&"PATH".to_string()));
        assert!(!names.contains(&"path".to_string()));
    }
}
