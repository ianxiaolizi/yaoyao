//! What a Python interpreter started by `shell.run` is told about its world.
//!
//! The agent writes a script into the workspace and runs it. Two properties of
//! that interpreter are not the model's to discover the hard way: which
//! directory comes first on `sys.path`, and what encoding its output uses.
//! Both have answers that look like the agent's own mistakes — a traceback
//! pointing at the script it just wrote, or output the model reads as mojibake
//! — so they are fixed here, once, rather than left to a prompt line the model
//! may not follow (CONTRACT §7 #147).
//!
//! These are added to the environment of **this tool's** children only, after
//! the allowlist has been applied. `agent_ports::process_env` decides what a
//! child may inherit; this decides what a shell command is given on top of it.
//! An MCP server runs Python of its own and is configured by its operator, so
//! it does not get these.

use std::ffi::OsString;

/// Variables whose absence makes Python behave in ways that read as the
/// agent's own mistakes.
///
/// * `PYTHONSAFEPATH=1` keeps the script's directory off `sys.path`. Without
///   it a workspace file named `inspect.py`, `json.py` or `types.py` shadows
///   the stdlib module of that name, and the traceback points at the agent's
///   own script rather than at the import that was meant. Python 3.11+ reads
///   the variable; older interpreters ignore it, which is why the built-in
///   preamble also states the naming rule.
/// * `PYTHONIOENCODING=utf-8` makes the interpreter write UTF-8 regardless of
///   the console code page. Output comes back through a pipe, and on a
///   localized Windows host the default page is not UTF-8, so the model would
///   read mojibake in every message it has to reason about.
/// * `PYTHONDONTWRITEBYTECODE=1` keeps `__pycache__` out of the workspace and
///   keeps a rewritten script from being run through stale bytecode.
pub(crate) const PYTHON_ENVIRONMENT: &[(&str, &str)] =
    &[("PYTHONSAFEPATH", "1"), ("PYTHONIOENCODING", "utf-8"), ("PYTHONDONTWRITEBYTECODE", "1")];

/// Append [`PYTHON_ENVIRONMENT`] to a child's environment.
///
/// A name already present is left alone: the allowlist does not currently pass
/// any of these through, so in practice these are the values a child sees, but
/// a future allowlist entry must not be silently overridden here. The check is
/// case-insensitive on Windows because that is how Windows compares names.
pub(crate) fn add_python_environment(env: &mut Vec<(OsString, OsString)>) {
    for (name, value) in PYTHON_ENVIRONMENT {
        let present = env.iter().any(|(existing, _)| same_env_name(existing, name));
        if !present {
            env.push((OsString::from(name), OsString::from(value)));
        }
    }
}

/// Whether two environment names are the same on this platform.
fn same_env_name(existing: &OsString, name: &str) -> bool {
    #[cfg(windows)]
    {
        existing.to_string_lossy().eq_ignore_ascii_case(name)
    }
    #[cfg(not(windows))]
    {
        existing.to_string_lossy() == name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value the caller already chose is not replaced: the defaults are a
    /// floor, not an opinion the environment overwrites.
    #[test]
    fn a_python_default_does_not_replace_an_existing_value() {
        let mut env = vec![(OsString::from("PYTHONIOENCODING"), OsString::from("gbk"))];

        add_python_environment(&mut env);

        let stated = env.iter().filter(|(name, _)| same_env_name(name, "PYTHONIOENCODING"));
        assert_eq!(stated.count(), 1, "the variable is stated once");
        assert_eq!(env[0].1, OsString::from("gbk"));
        assert_eq!(env.len(), PYTHON_ENVIRONMENT.len(), "one of the three was already there");
    }

    #[test]
    fn every_python_default_is_added_to_an_empty_environment() {
        let mut env: Vec<(OsString, OsString)> = Vec::new();

        add_python_environment(&mut env);
        add_python_environment(&mut env);

        assert_eq!(env.len(), PYTHON_ENVIRONMENT.len(), "adding twice adds nothing");
    }
}
