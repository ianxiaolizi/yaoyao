//! The verification apparatus: the files a check is graded against.
//!
//! A check that runs `cargo test` is not graded only by the code under test.
//! It is graded by the test files, the benchmarks and the acceptance scripts —
//! and every one of those is something the agent being graded can edit. Editing
//! `tests/foo.rs` so that it asserts nothing turns a failing suite green without
//! touching the work (CONTRACT §7 #172).
//!
//! The answer here is a *manifest*: the digest of every apparatus file as it
//! stood, recorded with the check. The first check of a session freezes one
//! (`verification/apparatus_frozen`, written by the kernel); every later check
//! records the manifest it actually ran against. Two digests that differ mean
//! the thing being graded moved, and the gate refuses the pass
//! ([`crate::evidence::ClaimVerdict::IntegrityChanged`]).
//!
//! The manifest is a fact about files, so this module is pure: it says what a
//! spec means and how to fold a list of files into one digest. Walking the
//! workspace and hashing contents is the adapter's job
//! (`agent_ports::verifier::CommandVerifier::manifest`), through the same
//! boundary a read goes through.
//!
//! Paths are workspace-relative and written with `/` on every platform. A
//! manifest that spelled the same file two ways would make the same tree
//! compare unequal on two hosts.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Which files count as apparatus.
///
/// `include` is a list of globs relative to the workspace root; `exclude` is
/// subtracted from it. Both are matched by [`glob_matches`], whose syntax is
/// deliberately small: `**` (any number of path segments), `*` (any run of
/// characters inside one segment) and `?` (one character). No character
/// classes, no braces, no escapes — a spec that needs more than this should be
/// two specs, and a matcher nobody can state exactly is a matcher that silently
/// covers the wrong files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ApparatusSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl ApparatusSpec {
    /// Whether this spec names anything at all.
    ///
    /// An empty spec freezes nothing: a session that never said what its
    /// apparatus is does not get a manifest, and its evidence says `None`
    /// rather than claiming a digest of the empty set.
    pub fn is_empty(&self) -> bool {
        self.include.is_empty()
    }

    /// Whether `path` (workspace-relative, `/`-separated) is apparatus.
    pub fn matches(&self, path: &str) -> bool {
        self.include.iter().any(|pattern| glob_matches(pattern, path))
            && !self.exclude.iter().any(|pattern| glob_matches(pattern, path))
    }

    /// Whether any include pattern could match something *below* `dir`.
    ///
    /// A walk uses this to leave a directory alone without listing it: a spec
    /// that names `tests/**` has no business descending into `target/`, and a
    /// repository's build directory is the difference between a manifest that
    /// takes a moment and one that takes minutes. Conservative in the only
    /// direction that is safe — it never says "no" about a directory something
    /// below it could match, because a pruned directory is a file missing from
    /// the digest.
    pub fn may_contain(&self, dir: &str) -> bool {
        self.include.iter().any(|pattern| reaches_below(pattern, dir))
    }
}

fn reaches_below(pattern: &str, dir: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').filter(|segment| !segment.is_empty()).collect();
    let dir: Vec<&str> = dir.split('/').filter(|segment| !segment.is_empty()).collect();
    reaches(&pattern, &dir)
}

fn reaches(pattern: &[&str], dir: &[&str]) -> bool {
    match pattern.split_first() {
        // The pattern has been used up naming a file; nothing below it is this
        // pattern's business.
        None => false,
        // `**` swallows whatever depth is left.
        Some((&"**", _)) => true,
        Some((segment, rest)) => match dir.split_first() {
            // The directory is shorter than the pattern: the remaining segments
            // are names still to be found below it.
            None => true,
            Some((first, tail)) => segment_matches(segment, first) && reaches(rest, tail),
        },
    }
}

/// One apparatus file as it stood.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApparatusEntry {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

impl ApparatusEntry {
    pub fn of(path: impl Into<String>, content: &[u8]) -> Self {
        Self {
            path: path.into(),
            sha256: format!("{:x}", Sha256::digest(content)),
            bytes: content.len() as u64,
        }
    }
}

/// The apparatus a check ran against, folded into one digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApparatusManifest {
    pub spec: ApparatusSpec,
    /// Sorted by path, so two walks of the same tree fold to the same digest
    /// whatever order the directory listings came back in.
    pub entries: Vec<ApparatusEntry>,
    /// Digest of the entries. This is what evidence records and what the frozen
    /// manifest is compared against.
    pub hash: String,
}

impl ApparatusManifest {
    /// Fold `entries` into a manifest over `spec`.
    ///
    /// Duplicate paths are collapsed: a walk that somehow saw one file twice
    /// (a case-insensitive filesystem, a mount point) must not make the digest
    /// depend on how many times it was seen.
    pub fn build(spec: ApparatusSpec, mut entries: Vec<ApparatusEntry>) -> Self {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries.dedup_by(|a, b| a.path == b.path);
        let hash = digest_of(&entries);
        Self { spec, entries, hash }
    }
}

/// The digest of a sorted entry list.
///
/// Each line is `path \0 sha256 \0 bytes`, so no two different file sets can
/// produce the same text: the separators are characters a path cannot hold.
fn digest_of(entries: &[ApparatusEntry]) -> String {
    let mut material = String::new();
    for entry in entries {
        material.push_str(&entry.path);
        material.push('\0');
        material.push_str(&entry.sha256);
        material.push('\0');
        material.push_str(&entry.bytes.to_string());
        material.push('\n');
    }
    format!("{:x}", Sha256::digest(material.as_bytes()))
}

/// Whether `pattern` matches `path`, both `/`-separated and workspace-relative.
///
/// `**` stands for any number of whole segments (including none), so
/// `tests/**` matches `tests/a.rs` and `tests/nested/b.rs`; `tests/*` matches
/// only the first. A pattern with no `/` matches a top-level name only, which is
/// the reading that makes `*_test.go` mean `./*_test.go` rather than every file
/// in the tree.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').filter(|segment| !segment.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((segment, rest)) => match path.split_first() {
            None => false,
            Some((first, tail)) => segment_matches(segment, first) && segments_match(rest, tail),
        },
    }
}

fn segment_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    match_head(&pattern, &text)
}

fn match_head(pattern: &[char], text: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).any(|skip| match_head(&pattern[1..], &text[skip..])),
        Some('?') => !text.is_empty() && match_head(&pattern[1..], &text[1..]),
        Some(character) => text.first() == Some(character) && match_head(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(include: &[&str], exclude: &[&str]) -> ApparatusSpec {
        ApparatusSpec {
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_double_star_covers_whole_segments() {
        assert!(glob_matches("tests/**", "tests/a.rs"));
        assert!(glob_matches("tests/**", "tests/nested/b.rs"));
        assert!(!glob_matches("tests/**", "src/tests/a.rs"));
        // At the end it also matches the directory name with nothing under it.
        assert!(glob_matches("tests/**", "tests"));
    }

    #[test]
    fn a_single_star_stays_inside_one_segment() {
        assert!(glob_matches("tests/*", "tests/a.rs"));
        assert!(!glob_matches("tests/*", "tests/nested/b.rs"));
        assert!(glob_matches("*_test.go", "parser_test.go"));
        assert!(!glob_matches("*_test.go", "pkg/parser_test.go"), "no slash means top level only");
    }

    #[test]
    fn a_question_mark_is_one_character() {
        assert!(glob_matches("t?st", "test"));
        assert!(!glob_matches("t?st", "tst"));
        assert!(!glob_matches("t?st", "teest"));
    }

    #[test]
    fn exclusion_wins_over_inclusion() {
        let spec = spec(&["tests/**"], &["tests/fixtures/**"]);
        assert!(spec.matches("tests/a.rs"));
        assert!(!spec.matches("tests/fixtures/data.json"));
        assert!(!spec.matches("src/lib.rs"), "a file no include names is not apparatus");
    }

    #[test]
    fn a_walk_knows_which_directories_it_must_enter() {
        let deep = spec(&["tests/**"], &[]);
        assert!(deep.may_contain("tests"));
        assert!(deep.may_contain("tests/nested"));
        assert!(!deep.may_contain("target"), "nothing under target can match tests/**");
        assert!(deep.may_contain(""), "the root is above everything, including tests/");

        let shallow = spec(&["tests/*.rs"], &[]);
        assert!(shallow.may_contain("tests"));
        assert!(!shallow.may_contain("tests/nested"), "one `*` cannot reach a level deeper");
    }

    #[test]
    fn an_empty_spec_matches_nothing() {
        assert!(ApparatusSpec::default().is_empty());
        assert!(!ApparatusSpec::default().matches("tests/a.rs"));
    }

    #[test]
    fn the_digest_does_not_depend_on_the_order_files_were_seen() {
        let one = ApparatusEntry::of("tests/a.rs", b"alpha");
        let two = ApparatusEntry::of("tests/b.rs", b"beta");
        let forwards =
            ApparatusManifest::build(spec(&["tests/**"], &[]), vec![one.clone(), two.clone()]);
        let backwards = ApparatusManifest::build(spec(&["tests/**"], &[]), vec![two, one]);

        assert_eq!(forwards.hash, backwards.hash);
        assert_eq!(forwards.entries[0].path, "tests/a.rs", "entries are sorted");
    }

    #[test]
    fn any_content_change_moves_the_digest() {
        let before = ApparatusManifest::build(
            spec(&["tests/**"], &[]),
            vec![ApparatusEntry::of("tests/a.rs", b"assert!(true)")],
        );
        let after = ApparatusManifest::build(
            spec(&["tests/**"], &[]),
            vec![ApparatusEntry::of("tests/a.rs", b"assert!(true); // edited")],
        );
        assert_ne!(before.hash, after.hash);
        assert_ne!(before.entries[0].sha256, after.entries[0].sha256);
    }

    #[test]
    fn a_moved_file_moves_the_digest_even_with_the_same_content() {
        let here = ApparatusManifest::build(
            spec(&["tests/**"], &[]),
            vec![ApparatusEntry::of("tests/a.rs", b"x")],
        );
        let there = ApparatusManifest::build(
            spec(&["tests/**"], &[]),
            vec![ApparatusEntry::of("tests/a.rs", b"x")],
        )
        .entries;
        let renamed = ApparatusManifest::build(
            spec(&["tests/**"], &[]),
            vec![ApparatusEntry::of("tests/b.rs", b"x")],
        );
        assert_eq!(here.entries, there, "the same tree folds the same way");
        assert_ne!(here.hash, renamed.hash, "the path is part of what was graded");
    }

    #[test]
    fn a_manifest_round_trips() {
        let manifest = ApparatusManifest::build(
            spec(&["tests/**"], &["tests/fixtures/**"]),
            vec![ApparatusEntry::of("tests/a.rs", b"alpha")],
        );
        let text = serde_json::to_string(&manifest).unwrap();
        assert_eq!(serde_json::from_str::<ApparatusManifest>(&text).unwrap(), manifest);
    }
}
