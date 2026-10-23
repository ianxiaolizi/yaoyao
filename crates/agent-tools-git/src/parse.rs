//! Parsers for the two pieces of git output the tools expose.
//!
//! Both parse the machine-readable forms (`--porcelain=v1 -z`, plain `git
//! diff`), never the human-facing ones: the short format quotes paths
//! containing spaces, and a parser that has to unquote is a parser that has to
//! reimplement git's quoting rules.
//!
//! Everything here is a pure function of a string, so the shapes git actually
//! produced can be pinned in tests without a repository in sight — and the
//! integration tests check those shapes against a real one.

use serde_json::{Value, json};

/// One entry of `git status --porcelain=v1 -z`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub index: char,
    pub worktree: char,
    pub path: String,
    /// The source path of a rename or copy; `None` for every other entry.
    pub original: Option<String>,
}

impl StatusEntry {
    /// The state in a word, so a reader does not have to decode `XY` itself.
    pub fn state(&self) -> &'static str {
        state_of(self.index, self.worktree)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "index": self.index.to_string(),
            "worktree": self.worktree.to_string(),
            "state": self.state(),
            "original": self.original,
        })
    }
}

/// Read the two status columns as one word.
///
/// The order is the order of the questions a reader is asking: a path that is
/// both unknown and untracked is untracked, and one that is staged as modified
/// and deleted from the worktree is deleted.
fn state_of(index: char, worktree: char) -> &'static str {
    match (index, worktree) {
        ('?', '?') => "untracked",
        ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => "conflicted",
        ('R', _) => "renamed",
        ('C', _) => "copied",
        ('D', _) | (_, 'D') => "deleted",
        ('T', _) | (_, 'T') => "typechange",
        ('A', _) => "added",
        ('M', _) | (_, 'M') => "modified",
        _ => "unknown",
    }
}

/// Parse `git status --porcelain=v1 -z`.
///
/// Entries are `XY PATH\0`, and a rename or copy is followed by a second
/// `ORIGINAL\0` field — which is why this walks fields rather than lines.
pub fn parse_status(raw: &str) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    let mut fields = raw.split('\0');
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        // A short entry is not one; skipping beats inventing a path out of a
        // truncated field.
        if field.len() < 4 {
            continue;
        }
        let bytes = field.as_bytes();
        let index = bytes[0] as char;
        let worktree = bytes[1] as char;
        let path = field.get(3..).unwrap_or_default().to_string();
        let renamed = matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C');
        let original = if renamed { fields.next().map(str::to_string) } else { None };
        entries.push(StatusEntry { index, worktree, path, original });
    }
    entries
}

/// One file's section of a `git diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub file: String,
    pub patch: String,
    pub additions: u32,
    pub deletions: u32,
    pub status: &'static str,
}

impl FileDiff {
    pub fn to_json(&self) -> Value {
        json!({
            "file": self.file,
            "patch": self.patch,
            "additions": self.additions,
            "deletions": self.deletions,
            "status": self.status,
        })
    }
}

/// Parse a unified diff into one entry per file.
///
/// Counts come from the hunk bodies only: `---`/`+++` are headers, and the
/// `index`/`mode` lines above the first `@@` are metadata. Counting the whole
/// section would report every changed file as one line longer than it is.
pub fn parse_diff(raw: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut section: Option<Section> = None;

    for line in raw.lines() {
        if line.starts_with("diff --git ") {
            if let Some(finished) = section.take() {
                files.push(finished.finish());
            }
            section = Some(Section::new(line));
            continue;
        }
        if let Some(section) = section.as_mut() {
            section.push(line);
        }
    }
    if let Some(finished) = section {
        files.push(finished.finish());
    }
    files
}

/// One `diff --git` section, accumulated line by line.
struct Section {
    lines: Vec<String>,
    minus_path: Option<String>,
    plus_path: Option<String>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    additions: u32,
    deletions: u32,
    in_hunks: bool,
}

impl Section {
    fn new(header: &str) -> Self {
        Self {
            lines: vec![header.to_string()],
            minus_path: None,
            plus_path: None,
            rename_from: None,
            rename_to: None,
            additions: 0,
            deletions: 0,
            in_hunks: false,
        }
    }

    fn push(&mut self, line: &str) {
        self.lines.push(line.to_string());

        // The extended headers, which appear before any hunk. Their leading
        // markers are the same characters a content line could start with — a
        // hunk can certainly add a line reading "+++ x" — so nothing below may
        // look at them once a hunk has started.
        if !self.in_hunks {
            if let Some(rest) = line.strip_prefix("--- ") {
                self.minus_path = Some(rest.trim_end().to_string());
            } else if let Some(rest) = line.strip_prefix("+++ ") {
                self.plus_path = Some(rest.trim_end().to_string());
            } else if let Some(rest) = line.strip_prefix("rename from ") {
                self.rename_from = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                self.rename_to = Some(rest.to_string());
            }
        }

        if line.starts_with("@@ ") || line.starts_with("@@@ ") {
            self.in_hunks = true;
            return;
        }
        if !self.in_hunks {
            return;
        }
        match line.as_bytes().first() {
            Some(b'+') => self.additions += 1,
            Some(b'-') => self.deletions += 1,
            // `\ No newline at end of file` is neither.
            _ => {}
        }
    }

    fn finish(self) -> FileDiff {
        FileDiff {
            file: self.file_name(),
            patch: self.lines.join("\n"),
            additions: self.additions,
            deletions: self.deletions,
            status: self.status(),
        }
    }

    /// The path the change is *about*: the destination for everything except a
    /// deletion, which has none.
    fn file_name(&self) -> String {
        if let Some(to) = &self.rename_to {
            return to.clone();
        }
        match (&self.plus_path, &self.minus_path) {
            (Some(plus), _) if plus != "/dev/null" => strip_prefix(plus, "b/"),
            (_, Some(minus)) if minus != "/dev/null" => strip_prefix(minus, "a/"),
            (Some(plus), _) => plus.clone(),
            (_, Some(minus)) => minus.clone(),
            _ => String::new(),
        }
    }

    fn status(&self) -> &'static str {
        if self.rename_from.is_some() && self.rename_to.is_some() {
            return "renamed";
        }
        match (self.minus_path.as_deref(), self.plus_path.as_deref()) {
            (Some("/dev/null"), _) => "added",
            (_, Some("/dev/null")) => "deleted",
            _ => "modified",
        }
    }
}

/// Drop git's `a/` or `b/` diff prefix, if it is there.
fn strip_prefix(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly the bytes `git status --porcelain=v1 -z` emitted for a staged
    /// deletion, a new file with a space in its name, and a rename.
    const STATUS_Z: &str = "D  a.txt\0A  b with space.txt\0R  renamed file.txt\0a.txt\0";

    #[test]
    fn status_entries_come_back_with_their_columns_and_paths() {
        let entries = parse_status(STATUS_Z);

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "a.txt");
        assert_eq!(entries[0].state(), "deleted");
        // A path with a space survives, because `-z` never quotes.
        assert_eq!(entries[1].path, "b with space.txt");
        assert_eq!(entries[1].state(), "added");
    }

    #[test]
    fn a_rename_carries_the_path_it_came_from() {
        let entries = parse_status(STATUS_Z);
        let renamed = &entries[2];

        assert_eq!(renamed.path, "renamed file.txt");
        assert_eq!(renamed.original.as_deref(), Some("a.txt"));
        assert_eq!(renamed.state(), "renamed");
        // The original is not a fourth entry: it is a field of the third.
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn an_untracked_file_is_not_reported_as_a_modification() {
        let entries = parse_status("?? notes.md\0");
        assert_eq!(entries[0].state(), "untracked");
        assert_eq!(entries[0].path, "notes.md");
        assert!(entries[0].original.is_none());
    }

    #[test]
    fn empty_and_short_fields_are_skipped_rather_than_guessed_at() {
        assert!(parse_status("").is_empty());
        assert!(parse_status("\0\0").is_empty());
        assert!(parse_status("M\0").is_empty());
    }

    #[test]
    fn status_entries_serialize_with_the_state_spelled_out() {
        let json = parse_status(" M src/main.rs\0")[0].to_json();
        assert_eq!(json["path"], "src/main.rs");
        assert_eq!(json["index"], " ");
        assert_eq!(json["worktree"], "M");
        assert_eq!(json["state"], "modified");
        assert_eq!(json["original"], Value::Null);
    }

    /// A real `git diff` of one modified file.
    const MODIFIED_DIFF: &str = "\
diff --git a/notes.txt b/notes.txt
index 4cb29ea..6addb9b 100644
--- a/notes.txt
+++ b/notes.txt
@@ -1,3 +1,4 @@
 one
-two
+TWO
 three
+four
";

    #[test]
    fn a_modified_file_reports_its_lines_added_and_removed() {
        let files = parse_diff(MODIFIED_DIFF);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file, "notes.txt");
        assert_eq!(files[0].status, "modified");
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
    }

    #[test]
    fn the_hunk_headers_are_not_counted_as_changes() {
        let files = parse_diff(MODIFIED_DIFF);
        // `--- a/notes.txt` and `+++ b/notes.txt` are headers. Counting them
        // would make every file look one line bigger than it is.
        assert_eq!(files[0].additions, 2);
        assert!(files[0].patch.contains("--- a/notes.txt"));
    }

    #[test]
    fn a_context_line_that_looks_like_a_header_is_not_counted() {
        let raw = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,2 +1,2 @@
 keep
++++ looks like a header
";
        // The body line `++++ looks like a header` is an addition of the text
        // `+++ looks like a header`. Inside a hunk, `+` means one thing only.
        assert_eq!(parse_diff(raw)[0].additions, 1);
    }

    #[test]
    fn an_added_file_has_no_old_side_and_takes_its_name_from_the_new_one() {
        let raw = "\
diff --git a/fresh.txt b/fresh.txt
new file mode 100644
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1,1 @@
+hello
";
        let files = parse_diff(raw);
        assert_eq!(files[0].file, "fresh.txt");
        assert_eq!(files[0].status, "added");
        assert_eq!((files[0].additions, files[0].deletions), (1, 0));
    }

    #[test]
    fn a_deleted_file_takes_its_name_from_the_side_that_still_exists() {
        let raw = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
";
        let files = parse_diff(raw);
        assert_eq!(files[0].file, "gone.txt");
        assert_eq!(files[0].status, "deleted");
        assert_eq!((files[0].additions, files[0].deletions), (0, 1));
    }

    #[test]
    fn a_renamed_file_with_no_edits_is_still_a_file() {
        // A pure rename has no hunks at all, so a parser that only emits a file
        // when it sees `@@` would drop the change entirely.
        let raw = "\
diff --git a/a.txt b/renamed file.txt
similarity index 100%
rename from a.txt
rename to renamed file.txt
";
        let files = parse_diff(raw);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file, "renamed file.txt");
        assert_eq!(files[0].status, "renamed");
        assert_eq!((files[0].additions, files[0].deletions), (0, 0));
    }

    #[test]
    fn several_files_stay_separate() {
        let raw = format!("{MODIFIED_DIFF}{}", MODIFIED_DIFF.replace("notes.txt", "other.txt"));
        let files = parse_diff(&raw);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file, "notes.txt");
        assert_eq!(files[1].file, "other.txt");
    }

    #[test]
    fn a_diff_with_nothing_in_it_yields_nothing() {
        assert!(parse_diff("").is_empty());
        assert!(parse_diff("\n").is_empty());
    }

    #[test]
    fn a_trailing_tab_on_a_path_header_is_not_part_of_the_name() {
        // git appends a tab when a path contains a space, to keep the header
        // parseable; it is not part of the name.
        let raw = "\
diff --git a/b with space.txt b/b with space.txt
--- a/b with space.txt
+++ b/b with space.txt\t
@@ -0,0 +1,1 @@
+new
";
        assert_eq!(parse_diff(raw)[0].file, "b with space.txt");
    }
}
