//! Where the product gets a run's instructions.
//!
//! The kernel asks the port; this is the answer the shipped binary gives. It
//! composes one text out of two parts:
//!
//! * a built-in preamble describing the workspace the run executes in, the
//!   rules that outlive any single project, and how to read tool results;
//! * the workspace's own `AGENTS.md`, when it has one.
//!
//! The resolution happens once per run rather than once per process: a
//! long-lived `serve` must see an edited instruction file on its next turn
//! instead of on its next restart.
//!
//! Every failure here is a run that does not start. The alternative — starting
//! with a partial prompt — would leave the model acting under instructions the
//! log cannot show, which is the one thing the event log exists to prevent.

use std::io::Read;
use std::path::{Path, PathBuf};

use agent_ports::instructions::{InstructionError, InstructionSource};
use agent_protocol::instructions::RunInstructions;

/// The project instruction file, relative to the workspace root.
pub const PROJECT_FILE: &str = "AGENTS.md";

/// The largest project instruction file that will be folded into a prompt.
///
/// A file past this is refused rather than truncated: half an instruction set
/// is not a smaller instruction set, it is a different one, and the model would
/// have no way to tell that something is missing.
pub const MAX_PROJECT_FILE_BYTES: u64 = 64 * 1024;

/// Names the preamble below. Recorded with every run, so a later change to the
/// wording is visible in the log as a different prompt rather than as the same
/// one that mysteriously behaves differently.
pub const BUILTIN: &str = "yaoyao/instructions/1";

/// Instructions resolved from one workspace root.
pub struct WorkspaceInstructions {
    root: PathBuf,
}

impl WorkspaceInstructions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl InstructionSource for WorkspaceInstructions {
    fn for_run(&self) -> Result<RunInstructions, InstructionError> {
        resolve_instructions(&self.root)
    }
}

/// Read the workspace's instruction file and compose the run's prompt.
pub fn resolve_instructions(root: &Path) -> Result<RunInstructions, InstructionError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|source| InstructionError::Read { path: root.display().to_string(), source })?;
    let file = root.join(PROJECT_FILE);

    let project = match file.canonicalize() {
        Ok(canonical_file) => {
            // A file is only "the project's instructions" if it is the
            // workspace's own. A symlink pointing out of the tree would
            // otherwise fold arbitrary readable content into the prompt.
            if !canonical_file.starts_with(&canonical_root) {
                return Err(InstructionError::OutsideWorkspace {
                    path: file.display().to_string(),
                    resolved: canonical_file.display().to_string(),
                });
            }
            let display = file.display().to_string();
            // One byte past the cap is enough to tell "at cap" from "past
            // cap", and the read stays bounded whatever the file claims its
            // size is — a metadata check alone would trust a number that can
            // change between the check and the read.
            let mut bytes = Vec::new();
            std::fs::File::open(&canonical_file)
                .and_then(|file| {
                    let mut limited = file.take(MAX_PROJECT_FILE_BYTES + 1);
                    limited.read_to_end(&mut bytes)
                })
                .map_err(|source| InstructionError::Read { path: display.clone(), source })?;
            if bytes.len() as u64 > MAX_PROJECT_FILE_BYTES {
                return Err(InstructionError::TooLarge {
                    path: display,
                    limit: MAX_PROJECT_FILE_BYTES as usize,
                    size: bytes.len() as u64,
                });
            }
            let text = String::from_utf8(bytes)
                .map_err(|_| InstructionError::NotUtf8 { path: display })?;
            Some(text)
        }
        // A workspace without one is a legal state, not a failed read: most
        // workspaces will not have a file until someone writes one.
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(InstructionError::Read { path: file.display().to_string(), source });
        }
    };

    let mut text = builtin_preamble(root);
    if let Some(project) = &project {
        text.push_str("\n\n# Project instructions\n\n");
        text.push_str(project.trim_end());
        text.push('\n');
    }

    Ok(RunInstructions {
        text,
        project_file: project.as_ref().map(|_| PROJECT_FILE.to_string()),
        builtin: BUILTIN.to_string(),
    })
}

/// The part of the prompt that does not depend on the project.
fn builtin_preamble(root: &Path) -> String {
    format!(
        "You are an agent working inside one workspace.\n\
         \n\
         Workspace root: {root}\n\
         Platform: {os}/{arch}\n\
         \n\
         How to work:\n\
         - Paths passed to tools are resolved against the workspace root. Use \
         paths relative to it unless something gives you a reason not to, and \
         do not assume the current directory or the host's absolute paths are \
         meaningful.\n\
         - Inspect before you assert. A tool result is the only evidence you \
         have about the workspace; when you have not looked, say so instead of \
         guessing.\n\
         - Keep changes to what the task needs and match the surrounding code, \
         its naming and its comment style.\n\
         - When a tool call comes back refused, or the run stops for a human \
         decision, do not retry the same call expecting a different answer. \
         Report what happened and wait.\n\
         - Answer in the language the user writes in.\n",
        root = root.display(),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_workspace_without_a_file_still_gets_the_builtin_prompt() {
        let dir = workspace();
        let instructions = resolve_instructions(dir.path()).unwrap();
        assert_eq!(instructions.project_file, None);
        assert_eq!(instructions.builtin, BUILTIN);
        assert!(instructions.text.contains(&dir.path().display().to_string()));
        assert!(instructions.text.contains("Workspace root:"));
    }

    #[test]
    fn the_project_file_is_appended_after_the_builtin_prompt() {
        let dir = workspace();
        std::fs::write(dir.path().join(PROJECT_FILE), "Always run the tests.\n").unwrap();
        let instructions = resolve_instructions(dir.path()).unwrap();
        assert_eq!(instructions.project_file.as_deref(), Some(PROJECT_FILE));
        assert!(instructions.text.contains("# Project instructions"));
        assert!(instructions.text.contains("Always run the tests."));
        // The project's rules come last, and are not the whole prompt.
        assert!(
            instructions.text.find("Workspace root:").unwrap()
                < instructions.text.find("Always run the tests.").unwrap()
        );
    }

    #[test]
    fn an_edited_file_is_seen_on_the_next_resolution() {
        let dir = workspace();
        let file = dir.path().join(PROJECT_FILE);
        std::fs::write(&file, "first").unwrap();
        assert!(resolve_instructions(dir.path()).unwrap().text.contains("first"));
        std::fs::write(&file, "second").unwrap();
        let later = resolve_instructions(dir.path()).unwrap().text;
        assert!(later.contains("second"));
        assert!(!later.contains("first"));
    }

    #[test]
    fn a_file_over_the_cap_is_refused_rather_than_truncated() {
        let dir = workspace();
        let file = dir.path().join(PROJECT_FILE);
        std::fs::write(&file, vec![b'a'; MAX_PROJECT_FILE_BYTES as usize + 1]).unwrap();
        match resolve_instructions(dir.path()) {
            Err(InstructionError::TooLarge { size, limit, .. }) => {
                assert_eq!(size, MAX_PROJECT_FILE_BYTES + 1);
                assert_eq!(limit, MAX_PROJECT_FILE_BYTES as usize);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn invalid_utf8_is_refused() {
        let dir = workspace();
        std::fs::write(dir.path().join(PROJECT_FILE), [0xff, 0xfe, 0xfd]).unwrap();
        assert!(matches!(resolve_instructions(dir.path()), Err(InstructionError::NotUtf8 { .. })));
    }

    #[test]
    fn a_missing_root_is_a_read_failure_not_an_empty_prompt() {
        let dir = workspace();
        let gone = dir.path().join("not-a-directory");
        assert!(matches!(resolve_instructions(&gone), Err(InstructionError::Read { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_pointing_outside_the_workspace_is_refused() {
        let dir = workspace();
        let outside = workspace();
        let target = outside.path().join(PROJECT_FILE);
        std::fs::write(&target, "outside instructions").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(PROJECT_FILE)).unwrap();
        match resolve_instructions(dir.path()) {
            Err(InstructionError::OutsideWorkspace { resolved, .. }) => {
                assert_eq!(resolved, target.canonicalize().unwrap().display().to_string());
            }
            other => panic!("expected OutsideWorkspace, got {other:?}"),
        }
    }
}
