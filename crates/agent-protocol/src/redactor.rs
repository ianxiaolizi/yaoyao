//! Redactor — scrub known secret patterns from anything that lands in the model
//! context or the event log: tool **output** and tool-call **arguments**.
//!
//! This is a best-effort backstop, not the security boundary: the durable types
//! already never carry secret plaintext, and `SecretProvider` injects plaintext
//! only at execution time. Redaction here catches secrets that leak through
//! tool stdout/stderr, or that a model copies into a call's arguments, so they
//! never reach the model or a durable `tool/*` event.
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

/// Scrubs known secret patterns from arbitrary text and JSON values.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    prefixes: Vec<&'static str>,
}

impl Redactor {
    pub fn new() -> Self {
        Self { prefixes: DEFAULT_PREFIXES.to_vec() }
    }

    /// Replace secret-shaped tokens and private-key blocks with `***`, returning
    /// the scrubbed text and which patterns were applied.
    pub fn redact(&self, text: &str) -> (String, RedactionMetadata) {
        let mut patterns: Vec<String> = Vec::new();

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

        patterns.sort();
        patterns.dedup();
        let redacted = !patterns.is_empty();
        (out, RedactionMetadata { redacted, patterns })
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
        let scrubbed = self.redact_value_into(value, &mut patterns);
        patterns.sort();
        patterns.dedup();
        let redacted = !patterns.is_empty();
        (scrubbed, RedactionMetadata { redacted, patterns })
    }

    fn redact_value_into(&self, value: &Value, patterns: &mut Vec<String>) -> Value {
        match value {
            Value::String(text) => {
                let (scrubbed, meta) = self.redact(text);
                patterns.extend(meta.patterns);
                Value::String(scrubbed)
            }
            Value::Array(items) => Value::Array(
                items.iter().map(|item| self.redact_value_into(item, patterns)).collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), self.redact_value_into(item, patterns)))
                    .collect(),
            ),
            // Numbers, booleans, and null carry no secret and keep their type.
            other => other.clone(),
        }
    }
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
}
