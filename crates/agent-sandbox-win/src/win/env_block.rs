//! The environment block handed to `CreateProcessW`.
//!
//! `lpEnvironment = NULL` is the dangerous shorthand: it makes the child inherit
//! the *calling process's* whole environment, which in this agent contains the
//! model key (`AGENT_MODEL_API_KEY`). The allowlist in
//! `agent_ports::process_env` is what picks the variables; this module only
//! serialises them the way the API requires:
//!
//! * `NAME=VALUE\0` for every entry, then one extra NUL to terminate the block;
//! * UTF-16, sorted case-insensitively by name — the documented requirement, and
//!   one a binary search of the block depends on;
//! * a sandbox override replaces an inherited variable of the same name (Windows
//!   names are case-insensitive), rather than appearing twice.

use std::ffi::OsString;

/// Build the block. `overrides` win over `base`, case-insensitively.
pub fn build(base: &[(OsString, OsString)], overrides: &[(&str, OsString)]) -> Vec<u16> {
    let mut entries: Vec<(String, OsString)> = Vec::with_capacity(base.len() + overrides.len());
    for (name, value) in base {
        entries.push((name.to_string_lossy().into_owned(), value.clone()));
    }
    for (name, value) in overrides {
        let name = (*name).to_string();
        match entries.iter_mut().find(|(existing, _)| existing.eq_ignore_ascii_case(&name)) {
            Some(slot) => slot.1 = value.clone(),
            None => entries.push((name, value.clone())),
        }
    }

    // Neither half may contain the separators the format itself uses. A name
    // with `=` would move the boundary, a NUL would end the block early; both
    // are dropped rather than allowed to change what the child sees.
    entries.retain(|(name, value)| {
        !name.is_empty()
            && !name.contains('=')
            && !name.contains('\0')
            && !value.to_string_lossy().contains('\0')
    });
    entries.sort_by(|(left, _), (right, _)| {
        left.to_ascii_uppercase().cmp(&right.to_ascii_uppercase())
    });

    let mut block = Vec::new();
    for (name, value) in entries {
        block.extend(name.encode_utf16());
        block.push(u16::from(b'='));
        // Lossy because a Windows environment value is a UTF-16 string; a value
        // that cannot be represented was not one the host could pass either.
        block.extend(value.to_string_lossy().encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        // An empty block is still terminated: the API treats a leading NUL as
        // "no variables" and a NULL pointer as "inherit everything".
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(name: &str, value: &str) -> (OsString, OsString) {
        (OsString::from(name), OsString::from(value))
    }

    fn entries(block: &[u16]) -> Vec<String> {
        // The block ends with two NULs; each entry ends with one.
        let text = String::from_utf16_lossy(block);
        text.split('\0').filter(|entry| !entry.is_empty()).map(str::to_string).collect()
    }

    #[test]
    fn an_override_replaces_an_inherited_value_case_insensitively() {
        let block = build(
            &[pair("Path", r"C:\host"), pair("TEMP", r"C:\host\temp")],
            &[("TEMP", OsString::from(r"C:\sandbox\temp"))],
        );
        assert_eq!(
            entries(&block),
            vec![r"Path=C:\host".to_string(), r"TEMP=C:\sandbox\temp".to_string()]
        );
    }

    #[test]
    fn the_block_is_sorted_case_insensitively_and_double_terminated() {
        let block = build(&[pair("zulu", "1"), pair("Alpha", "2"), pair("mike", "3")], &[]);
        assert_eq!(entries(&block), vec!["Alpha=2", "mike=3", "zulu=1"]);
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
    }

    #[test]
    fn a_new_override_is_added() {
        let block =
            build(&[pair("PATH", r"C:\Windows")], &[("CARGO_HOME", OsString::from(r"C:\s"))]);
        assert_eq!(entries(&block), vec![r"CARGO_HOME=C:\s", r"PATH=C:\Windows"]);
    }

    #[test]
    fn malformed_names_are_dropped_rather_than_shifting_the_format() {
        let block =
            build(&[pair("A=B", "x"), pair("GOOD", "1")], &[("", OsString::from("empty name"))]);
        assert_eq!(entries(&block), vec!["GOOD=1"]);
    }

    #[test]
    fn an_empty_input_is_still_terminated() {
        let block = build(&[], &[]);
        assert_eq!(block, vec![0, 0]);
    }
}
