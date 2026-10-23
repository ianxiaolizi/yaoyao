//! The note that says a workspace tree has already been granted.
//!
//! `acl::grant_tree` walks every file to propagate an inheritable ACE, which
//! costs about 0.1 ms per file — around two seconds on a 20k-file workspace,
//! paid by the first command of every process. The walk is only needed for the
//! objects that existed **before** the ACE was set: anything created later
//! inherits it. So a record of one successful walk makes every later process
//! skip it.
//!
//! A stale record is not a security problem. Skipping a walk that should have
//! happened leaves files the container cannot open, and the tool reports an
//! access-denied error; it cannot grant more than the tree's root ACE allows.
//! The failure this trades for is "denied where it should be allowed", and the
//! remedy is deleting the record.
//!
//! What the record is bound to, and why each part matters:
//!
//! * the workspace path — a different workspace has not been walked;
//! * the SID — a different profile grants a different trustee;
//! * the access mask — a widened grant must not be inferred from a narrower
//!   one (a later change to [`super::acl::MODIFY`] invalidates old records);
//! * the machine name — the AppContainer SID derives from the profile *name*,
//!   so it is identical on every machine; a workspace copied here with its
//!   `.agent-data` would otherwise claim a grant this machine never made.
//!
//! The format is one `key value` line each rather than JSON: this crate builds
//! no schema otherwise, and a record is read once per process.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const VERSION: &str = "1";

/// What a grant covers. Every field is compared; any difference means the
/// tree is walked again.
pub(crate) struct GrantRecord<'a> {
    pub workspace: &'a Path,
    pub sid: &'a str,
    pub machine: &'a str,
    pub access: u32,
}

/// Whether the record at `path` describes this workspace and this grant.
///
/// A missing, unreadable, or malformed record is simply "not current": the
/// caller walks the tree and writes a new one.
pub(crate) fn is_current(path: &Path, record: &GrantRecord<'_>) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let mut version = None;
    let mut workspace = None;
    let mut sid = None;
    let mut machine = None;
    let mut access = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "version" => version = Some(value),
            "workspace" => workspace = Some(value),
            "sid" => sid = Some(value),
            "machine" => machine = Some(value),
            // The mask is written in decimal; a value that does not parse is a
            // record this code did not write.
            "access" => access = value.parse::<u32>().ok(),
            _ => {}
        }
    }
    version == Some(VERSION)
        && sid == Some(record.sid)
        && machine == Some(record.machine)
        && access == Some(record.access)
        // The path is compared as it is spelled in both places. Windows paths
        // cannot contain line feeds, so the line-oriented format round-trips
        // them; two spellings of one path (a `..` here, a different case
        // there) only cost a second walk.
        && workspace == Some(&*record.workspace.to_string_lossy())
}

/// Write the record, replacing an earlier one.
///
/// Through a temporary file and a rename, so a concurrent process never reads
/// half a record: both halves of that race walk the tree, which is idempotent.
pub(crate) fn write(path: &Path, record: &GrantRecord<'_>) -> io::Result<()> {
    let text = format!(
        "version {VERSION}\nworkspace {}\nsid {}\nmachine {}\naccess {}\n",
        record.workspace.to_string_lossy(),
        record.sid,
        record.machine,
        record.access
    );
    let staging = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&staging, text)?;
    fs::rename(&staging, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("yaoyao-grant-record-{}-{name}", std::process::id()));
        fs::create_dir_all(&directory).expect("create the test directory");
        directory
    }

    fn record<'a>(workspace: &'a Path) -> GrantRecord<'a> {
        GrantRecord {
            workspace,
            sid: "S-1-15-2-1-2-3-4-5-6-7",
            machine: "TEST-MACHINE",
            access: 0x0012_01FF,
        }
    }

    #[test]
    fn a_record_matches_only_what_it_was_written_for() {
        let directory = fixture("match");
        let path = directory.join("workspace-grant");
        let workspace = Path::new(r"G:\workspace");
        assert!(!is_current(&path, &record(workspace)), "nothing is written yet");

        write(&path, &record(workspace)).expect("write the record");
        assert!(is_current(&path, &record(workspace)));

        let other_workspace = Path::new(r"G:\other");
        assert!(!is_current(&path, &record(other_workspace)), "a different tree");
        assert!(
            !is_current(&path, &GrantRecord { sid: "S-1-15-2-9", ..record(workspace) }),
            "a different trustee"
        );
        assert!(
            !is_current(&path, &GrantRecord { machine: "OTHER", ..record(workspace) }),
            "a record written on another machine"
        );
        assert!(
            !is_current(&path, &GrantRecord { access: 0x1, ..record(workspace) }),
            "a different access mask"
        );

        fs::remove_dir_all(&directory).expect("remove the test directory");
    }

    #[test]
    fn a_missing_or_foreign_record_means_walk_the_tree() {
        let directory = fixture("foreign");
        let path = directory.join("workspace-grant");
        let workspace = Path::new(r"G:\workspace");

        assert!(!is_current(&path, &record(workspace)), "missing");

        fs::write(&path, "not a record\n").expect("write garbage");
        assert!(!is_current(&path, &record(workspace)), "unparseable");

        // A well-formed record from a future version is not assumed compatible.
        let future = format!(
            "version 2\nworkspace {}\nsid {}\nmachine {}\naccess {}\n",
            workspace.display(),
            "S-1-15-2-1-2-3-4-5-6-7",
            "TEST-MACHINE",
            "1180159"
        );
        fs::write(&path, future).expect("write a future record");
        assert!(!is_current(&path, &record(workspace)), "unknown version");

        fs::remove_dir_all(&directory).expect("remove the test directory");
    }

    #[test]
    fn writing_replaces_the_earlier_record_and_leaves_no_staging_file() {
        let directory = fixture("replace");
        let path = directory.join("workspace-grant");
        write(&path, &record(Path::new(r"G:\first"))).expect("first write");
        write(&path, &record(Path::new(r"G:\second"))).expect("second write");

        assert!(is_current(&path, &record(Path::new(r"G:\second"))));
        assert!(!is_current(&path, &record(Path::new(r"G:\first"))));
        assert!(
            !directory.join("workspace-grant.tmp").exists(),
            "the staging file is renamed, not left behind"
        );

        fs::remove_dir_all(&directory).expect("remove the test directory");
    }
}
