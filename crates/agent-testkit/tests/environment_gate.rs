//! Environment gate: the two machine facts the test suite cannot work without.
//!
//! On some machines the repository root carries a Low integrity label, and
//! every binary built beneath it inherits that label. A Low process may not
//! open the NUL device, and the CLI integration tests hand NUL to a child as
//! stdin; the failure then reads `PermissionDenied` and names the environment,
//! not the product. `scripts\test.ps1` raises the built test binaries to Medium
//! integrity and points TEMP at `%TEMP%\yaoyao-test-tmp`, and `scripts\preflight.ps1`
//! reports both facts before the gate runs.
//!
//! These two tests exist so that repair has a regression guard of its own: if
//! the script stops raising the label, or the TEMP redirection stops applying,
//! the gate fails here, where the message is about the environment, instead of
//! somewhere later, where the message is about an unrelated command.
//!
//! They are environment tests, not product tests. Running them under a plain
//! `cargo test` in a Low-labelled checkout is expected to fail; the repository
//! documents `scripts\test.ps1` as the entry point for exactly that reason.

use std::io::Write;

/// The suite's temporary files live under TEMP. A test that cannot create one
/// there cannot create a workspace, a data directory or an artifact.
#[test]
fn the_temp_directory_is_writable() {
    let dir = std::env::temp_dir();
    assert!(
        dir.is_absolute(),
        "TEMP resolved to `{}`, which is not an absolute path",
        dir.display()
    );
    let path = dir.join(format!(
        "yaoyao-environment-gate-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::write(&path, b"ok").unwrap_or_else(|error| {
        panic!(
            "TEMP `{}` is not writable: {error}\n\
             run scripts\\test.ps1 (it points TEMP at target\\tmp) or scripts\\preflight.ps1 to diagnose",
            dir.display()
        )
    });
    let read = std::fs::read(&path).expect("the file just written is readable");
    let _ = std::fs::remove_file(&path);
    assert_eq!(read, b"ok");
}

/// Windows opens `NUL` as a device for writing. The CLI tests give it to a
/// child as stdin, which requires opening it in the test process first.
#[cfg(windows)]
#[test]
fn the_nul_device_opens_for_writing() {
    let mut device = std::fs::OpenOptions::new().write(true).open("NUL").unwrap_or_else(|error| {
        panic!(
            "NUL cannot be opened for writing: {error}\n\
                 the process is probably below Medium integrity; run scripts\\test.ps1"
        )
    });
    device.write_all(b"probe").expect("writing to NUL discards bytes without failing");
}
