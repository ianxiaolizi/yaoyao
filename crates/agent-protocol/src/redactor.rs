//! Redactor — scrub known secret patterns from anything that lands in the model
//! context or the event log: tool **output** and tool-call **arguments**.
//!
//! This is a best-effort backstop, not the security boundary: the durable types
//! already never carry secret plaintext, and `SecretProvider` injects plaintext
//! only at execution time. Redaction here catches secrets that leak through
//! tool stdout/stderr, or that a model copies into a call's arguments, so they
//! never reach the model or a durable `tool/*` event.
//!
//! Two layers, deliberately asymmetric. Known token shapes are replaced
//! outright — a `ghp_…` is not a word anyone meant to write. A **high-entropy**
//! token is only *reported* by default: it may be an API key missing from the
//! prefix list, or it may be a base64 blob in code the caller asked to read,
//! and replacing the second would silently change an answer. So the detector
//! writes a warning into the redaction metadata and leaves the text alone
//! unless a configuration asks for replacement (`HighEntropy::Redact`).
//!
//! The log is long-lived and read back on resume, so redacting the approval
//! *summary* while the full arguments sit in the log beside it would be no
//! redaction at all. Arguments are therefore scrubbed where they are written,
//! by the crate that proposes them — this module lives in `agent-protocol`
//! rather than the kernel so both the loop and the kernel can use it.

use crate::permissions::RedactionMetadata;
use serde_json::Value;

/// Known secret token prefixes, plus private-key PEM blocks handled separately.
const DEFAULT_PREFIXES: &[&str] = &[
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "AKIA",
    "ASIA",
    "sk-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
];

/// What to do with a token that looks like a secret but matches no known shape.
///
/// `Warn` is the default: the finding goes into `RedactionMetadata::warnings`
/// and the text stays. `Redact` replaces the token with `***` and names
/// `high_entropy` in `patterns`. `Off` skips the scan, which is the only
/// setting that makes a large result cheaper to process — the scan itself is
/// linear, but it is not free.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HighEntropy {
    /// Report the finding, keep the text.
    #[default]
    Warn,
    /// Replace the token with `***`.
    Redact,
    /// Do not look.
    Off,
}

/// The name a high-entropy finding is recorded under — never the value.
const HIGH_ENTROPY_PATTERN: &str = "high_entropy";

/// Shorter than this is a word, a hash fragment, or a path segment far more
/// often than it is a secret; most issued tokens are longer.
const MIN_HIGH_ENTROPY_CHARS: usize = 32;

/// Bits per character below which a run is not treated as random-looking.
/// English prose sits near 4.1 over a long sample but a single 32-character
/// run of letters rarely reaches 4.0; base64 and random tokens sit near 5.
const MIN_HIGH_ENTROPY_BITS: f64 = 4.0;

/// Scrubs known secret patterns from arbitrary text and JSON values.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    prefixes: Vec<&'static str>,
    high_entropy: HighEntropy,
}

impl Redactor {
    pub fn new() -> Self {
        Self { prefixes: DEFAULT_PREFIXES.to_vec(), high_entropy: HighEntropy::default() }
    }

    /// Choose what the high-entropy layer does. Defaults to [`HighEntropy::Warn`].
    pub fn with_high_entropy(mut self, policy: HighEntropy) -> Self {
        self.high_entropy = policy;
        self
    }

    /// What the high-entropy layer is set to.
    pub fn high_entropy(&self) -> HighEntropy {
        self.high_entropy
    }

    /// Replace secret-shaped tokens and private-key blocks with `***`, returning
    /// the scrubbed text and which patterns were applied.
    pub fn redact(&self, text: &str) -> (String, RedactionMetadata) {
        let mut patterns: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        let (dekeyed, key_redacted) = redact_key_blocks(text);
        if key_redacted {
            patterns.push("private_key_block".to_string());
        }

        let mut out = String::with_capacity(dekeyed.len());
        let mut word = String::new();
        for ch in dekeyed.chars() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                word.push(ch);
            } else {
                flush(&mut out, &mut word, &self.prefixes, &mut patterns);
                out.push(ch);
            }
        }
        flush(&mut out, &mut word, &self.prefixes, &mut patterns);

        // The second layer runs over what the first one left, so a token the
        // prefix list already replaced is not reported twice.
        if self.high_entropy != HighEntropy::Off {
            let (scanned, hit) = high_entropy_pass(&out, self.high_entropy);
            if hit {
                match self.high_entropy {
                    HighEntropy::Redact => {
                        out = scanned;
                        patterns.push(HIGH_ENTROPY_PATTERN.to_string());
                    }
                    _ => warnings.push(HIGH_ENTROPY_PATTERN.to_string()),
                }
            }
        }

        patterns.sort();
        patterns.dedup();
        warnings.sort();
        warnings.dedup();
        let redacted = !patterns.is_empty();
        (out, RedactionMetadata { redacted, patterns, warnings })
    }

    /// Redact every string leaf of a JSON value, preserving its shape.
    ///
    /// A call's arguments are a JSON value and a secret sits at some leaf of it
    /// (`{"headers": {"Authorization": "Bearer sk-..."}}`). The whole value has
    /// to survive: a *resumed* call is read back out of the log and handed to
    /// the tool, so replacing the value with a string, reordering keys, or
    /// dropping branches would change what the tool is asked to do beyond the
    /// bytes that were secret. Only the string leaves change.
    pub fn redact_value(&self, value: &Value) -> (Value, RedactionMetadata) {
        let mut patterns: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let scrubbed = self.redact_value_into(value, &mut patterns, &mut warnings);
        patterns.sort();
        patterns.dedup();
        warnings.sort();
        warnings.dedup();
        let redacted = !patterns.is_empty();
        (scrubbed, RedactionMetadata { redacted, patterns, warnings })
    }

    fn redact_value_into(
        &self,
        value: &Value,
        patterns: &mut Vec<String>,
        warnings: &mut Vec<String>,
    ) -> Value {
        match value {
            Value::String(text) => {
                let (scrubbed, meta) = self.redact(text);
                patterns.extend(meta.patterns);
                warnings.extend(meta.warnings);
                Value::String(scrubbed)
            }
            Value::Array(items) => Value::Array(
                items.iter().map(|item| self.redact_value_into(item, patterns, warnings)).collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| {
                        (key.clone(), self.redact_value_into(item, patterns, warnings))
                    })
                    .collect(),
            ),
            // Numbers, booleans, and null carry no secret and keep their type.
            other => other.clone(),
        }
    }
}

/// Replace (or, in `Warn`, merely keep) every high-entropy run of characters.
///
/// Returns the possibly-rewritten text and whether any run was found. The scan
/// is over the allowed alphabet rather than the word splitter's, because a
/// token is not required to be a word: `+`, `-` and `_` are part of base64 and
/// base64url, and cutting at them would hide exactly the tokens this looks for.
fn high_entropy_pass(text: &str, policy: HighEntropy) -> (String, bool) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut hit = false;
    let mut index = 0;
    while index < bytes.len() {
        if !is_token_byte(bytes[index]) {
            // Copy by character, not by byte: a separator is whatever is not
            // in the token alphabet, and that includes multi-byte characters.
            let ch = text[index..].chars().next().expect("index is inside the string");
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        let start = index;
        while index < bytes.len() && is_token_byte(bytes[index]) {
            index += 1;
        }
        let token = &text[start..index];
        let found = is_high_entropy_token(token);
        hit = hit || found;
        if found && policy == HighEntropy::Redact {
            out.push_str("***");
        } else {
            out.push_str(token);
        }
    }
    (out, hit)
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'_')
}

/// Whether a run looks like an issued secret rather than a word.
///
/// Three conditions together, because each alone is a false positive factory:
/// long enough to be a token, carrying all three character classes (which
/// excludes git hashes and lowercase UUIDs), and random-looking enough by
/// Shannon entropy (which excludes camelCase identifiers and English text).
fn is_high_entropy_token(token: &str) -> bool {
    if token.len() < MIN_HIGH_ENTROPY_CHARS {
        return false;
    }
    let (mut lower, mut upper, mut digit) = (false, false, false);
    for byte in token.bytes() {
        match byte {
            b'a'..=b'z' => lower = true,
            b'A'..=b'Z' => upper = true,
            b'0'..=b'9' => digit = true,
            _ => {}
        }
    }
    lower && upper && digit && shannon_bits(token) >= MIN_HIGH_ENTROPY_BITS && looks_random(token)
}

/// Whether the letters alternate case the way a random string does.
///
/// Entropy alone does not separate a token from an identifier: a long camelCase
/// name mixes three character classes and reaches the same bits per character
/// as a key. What a random string also does is switch case almost every time,
/// where a name switches a handful of times over its whole length. Counting
/// letters only: digits carry no case, and a run of them says nothing either way.
fn looks_random(token: &str) -> bool {
    let mut letters = 0usize;
    let mut switches = 0usize;
    let mut previous: Option<bool> = None; // `true` = upper
    for byte in token.bytes() {
        let upper = match byte {
            b'a'..=b'z' => false,
            b'A'..=b'Z' => true,
            _ => continue,
        };
        if let Some(was_upper) = previous {
            letters += 1;
            if was_upper != upper {
                switches += 1;
            }
        } else {
            letters = 1;
        }
        previous = Some(upper);
    }
    // At least two letters, of which two fifths or more switch case. A random
    // mixed-case token sits near one half (each letter is upper with roughly
    // the same probability); a camelCase identifier sits near three tenths,
    // because each capital costs two switches — one in, one out. The gap is
    // wide enough that the threshold does not need to be tuned per alphabet.
    letters >= 8 && (switches as f64) >= (letters - 1) as f64 * 0.4
}

/// Shannon entropy in bits per byte.
fn shannon_bits(token: &str) -> f64 {
    let mut counts = [0usize; 256];
    for byte in token.bytes() {
        counts[byte as usize] += 1;
    }
    let len = token.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = *count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

fn flush(out: &mut String, word: &mut String, prefixes: &[&str], patterns: &mut Vec<String>) {
    if word.is_empty() {
        return;
    }
    if let Some(name) = matching_prefix(word, prefixes) {
        out.push_str("***");
        patterns.push(name.to_string());
    } else {
        out.push_str(word);
    }
    word.clear();
}

fn matching_prefix<'a>(word: &str, prefixes: &[&'a str]) -> Option<&'a str> {
    // Require at least one extra character past the prefix to avoid flagging
    // the bare prefix itself (e.g. the literal word "sk-").
    prefixes.iter().find(|p| word.len() > p.len() && word.starts_with(**p)).copied()
}

/// Strip `-----BEGIN ... PRIVATE KEY-----` ... `-----END ...-----` blocks.
fn redact_key_blocks(text: &str) -> (String, bool) {
    let mut out = String::with_capacity(text.len());
    let mut in_block = false;
    let mut redacted = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN") {
            in_block = true;
            redacted = true;
            out.push_str("[REDACTED PRIVATE KEY]\n");
            continue;
        }
        if in_block {
            if trimmed.starts_with("-----END") {
                in_block = false;
            }
            continue; // drop every line inside the block
        }
        out.push_str(line);
    }
    (out, redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(text: &str) -> (String, RedactionMetadata) {
        Redactor::new().redact(text)
    }

    #[test]
    fn clean_text_passes_through_untouched() {
        let (out, meta) = redacted("cargo test passed: 3 tests ok");
        assert_eq!(out, "cargo test passed: 3 tests ok");
        assert!(!meta.redacted);
        assert!(meta.patterns.is_empty());
    }

    #[test]
    fn github_token_is_scrubbed() {
        let (out, meta) = redacted("token=ghp_abcdefghijklmnopqrstuvwxyz");
        assert!(!out.contains("ghp_"), "secret leaked: {out}");
        assert!(out.contains("***"));
        assert!(meta.redacted);
        assert!(meta.patterns.iter().any(|p| p == "ghp_"));
    }

    #[test]
    fn aws_access_key_is_scrubbed() {
        let (out, meta) = redacted("AKIAIOSFODNN7EXAMPLE key");
        assert!(!out.contains("AKIA"), "secret leaked: {out}");
        assert!(meta.patterns.iter().any(|p| p == "AKIA"));
    }

    #[test]
    fn openai_style_key_is_scrubbed() {
        let (out, meta) = redacted("Authorization: Bearer sk-1234567890abcdef");
        assert!(!out.contains("sk-"), "secret leaked: {out}");
        assert!(meta.patterns.iter().any(|p| p == "sk-"));
    }

    #[test]
    fn private_key_block_is_scrubbed() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----\n";
        let (out, meta) = redacted(&format!("key:\n{pem}"));
        assert!(!out.contains("MIIEow"), "secret leaked: {out}");
        assert!(out.contains("[REDACTED PRIVATE KEY]"));
        assert!(meta.patterns.iter().any(|p| p == "private_key_block"));
    }

    #[test]
    fn bare_prefix_is_not_flagged() {
        // "sk-" alone has no secret body, so it stays put.
        let (out, meta) = redacted("the prefix sk- is short");
        assert_eq!(out, "the prefix sk- is short");
        assert!(!meta.redacted);
    }

    #[test]
    fn metadata_never_contains_the_secret_value() {
        let (_, meta) = redacted("ghp_SECRET_VALUE_HERE");
        for pattern in &meta.patterns {
            assert!(!pattern.contains("SECRET_VALUE"));
        }
    }

    #[test]
    fn value_strings_are_scrubbed_and_the_shape_survives() {
        let args = serde_json::json!({
            "url": "https://example.test",
            "headers": { "Authorization": "Bearer sk-1234567890abcdef" },
            "retries": 3,
            "verbose": false,
            "tags": ["ok", "ghp_abcdefghijklmnopqrstuvwxyz"],
            "nothing": null,
        });
        let (out, meta) = Redactor::new().redact_value(&args);

        assert!(meta.redacted);
        assert!(meta.patterns.iter().any(|p| p == "sk-"));
        assert!(meta.patterns.iter().any(|p| p == "ghp_"));

        // No leaf anywhere in the value still carries the secret.
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("sk-1234"), "secret leaked: {text}");
        assert!(!text.contains("ghp_abcdef"), "secret leaked: {text}");

        // The shape is intact: same keys, same non-string leaves by type.
        assert_eq!(out["url"], serde_json::json!("https://example.test"));
        assert_eq!(out["headers"]["Authorization"], serde_json::json!("Bearer ***"));
        assert_eq!(out["retries"], serde_json::json!(3));
        assert_eq!(out["verbose"], serde_json::json!(false));
        assert_eq!(out["nothing"], serde_json::Value::Null);
        assert_eq!(out["tags"][0], serde_json::json!("ok"));
        assert_eq!(out["tags"][1], serde_json::json!("***"));
    }

    #[test]
    fn a_clean_value_is_returned_unchanged_and_unflagged() {
        let args = serde_json::json!({ "path": "src/main.rs", "lines": [1, 2] });
        let (out, meta) = Redactor::new().redact_value(&args);
        assert_eq!(out, args);
        assert!(!meta.redacted);
        assert!(meta.patterns.is_empty());
    }

    /// A random-looking token in no known shape.
    const OPAQUE: &str = "Zk3vQ9mR7tY1pL5nB8cF2hJ6sD4gW0xE7uI9oP1a";

    #[test]
    fn a_high_entropy_token_is_reported_but_kept_by_default() {
        let text = format!("header: Bearer {OPAQUE} done");
        let (out, meta) = Redactor::new().redact(&text);

        assert_eq!(out, text, "the default layer must not change what the caller asked to read");
        assert!(!meta.redacted, "nothing was removed, so the result is not a redacted one");
        assert!(meta.patterns.is_empty());
        assert_eq!(meta.warnings, vec!["high_entropy".to_string()]);
    }

    #[test]
    fn the_redact_policy_replaces_a_high_entropy_token() {
        let text = format!("header: Bearer {OPAQUE} done");
        let (out, meta) = Redactor::new().with_high_entropy(HighEntropy::Redact).redact(&text);

        assert!(!out.contains(OPAQUE), "the token survived redaction: {out}");
        assert!(out.contains("***"));
        assert!(meta.redacted);
        assert_eq!(meta.patterns, vec!["high_entropy".to_string()]);
        assert!(meta.warnings.is_empty(), "an applied pattern is not also a warning");
    }

    #[test]
    fn the_off_policy_does_not_look_at_all() {
        let text = format!("header: Bearer {OPAQUE} done");
        let (out, meta) = Redactor::new().with_high_entropy(HighEntropy::Off).redact(&text);

        assert_eq!(out, text);
        assert!(meta.warnings.is_empty());
    }

    #[test]
    fn ordinary_long_tokens_are_not_high_entropy() {
        let cases = [
            // A git hash: lowercase hex, no upper class.
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            // A UUID: lowercase and digits, hyphens split nothing.
            "3f2b8c1e-9d4a-4c6b-8e2f-1a2b3c4d5e6f",
            // A camelCase identifier with a number: no entropy to speak of.
            "highEntropyDetectionThresholdForToolResults2026",
            // English text with punctuation; no single run qualifies.
            "the redactor reports high entropy tokens but keeps the text",
            // A path: the periods cut the run into short segments.
            "crates/agent-protocol/src/redactor.rs",
        ];
        for case in cases {
            let (out, meta) = redacted(case);
            assert_eq!(out, case, "{case} was changed");
            assert!(meta.warnings.is_empty(), "{case} was reported as high entropy");
        }
    }

    #[test]
    fn a_high_entropy_warning_never_carries_the_value() {
        let text = format!("bearer {OPAQUE}");
        let (_, meta) = Redactor::new().redact(&text);
        let encoded = serde_json::to_string(&meta).unwrap();
        assert!(!encoded.contains(OPAQUE), "the value reached the metadata: {encoded}");
        assert!(encoded.contains("high_entropy"));
    }

    #[test]
    fn a_value_leaf_reports_its_warnings_too() {
        let args = serde_json::json!({
            "headers": { "Authorization": format!("Bearer {OPAQUE}") },
            "path": "src/main.rs",
        });
        let (out, meta) = Redactor::new().redact_value(&args);
        assert_eq!(out, args, "warn mode leaves the value alone");
        assert_eq!(meta.warnings, vec!["high_entropy".to_string()]);
    }

    #[test]
    fn metadata_written_before_warnings_existed_still_reads() {
        let old = serde_json::json!({ "redacted": false, "patterns": [] });
        let decoded: RedactionMetadata = serde_json::from_value(old).unwrap();
        assert!(decoded.warnings.is_empty());
    }
}
