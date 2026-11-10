//! What the model may see of a result: scrubbing, capping, and the preview an
//! offloaded answer leaves behind.
//!
//! Two different jobs that share one answer — the text a model is handed after
//! a call — and they are kept here rather than in the gateway because neither
//! needs to know what a gateway is. A value this run resolved is scrubbed
//! before the pattern-based redactor: the redactor knows what a secret *looks*
//! like, and this knows the exact bytes of the one that was just handed to the
//! tool. Neither subsumes the other.

use agent_ports::tool_provider::ToolProviderError;
use agent_protocol::permissions::ArtifactRef;

/// How much of an offloaded result still reaches the model: enough to recognise
/// what it is and where it went wrong, not enough to be the thing itself.
pub(crate) const PREVIEW_HEAD_BYTES: usize = 2_048;
pub(crate) const PREVIEW_TAIL_BYTES: usize = 512;

/// A result past this many lines is offloaded however small it is in bytes.
pub(crate) const OFFLOAD_MAX_LINES: usize = 2_000;

/// The shortest resolved value the result scrubber replaces.
///
/// A backstop, not the boundary: the arguments never carried the value, and a
/// tool that echoes it is the case this covers. Below this length a value is
/// indistinguishable from an ordinary word, and replacing every occurrence
/// would corrupt perfectly good output while protecting nothing.
const MIN_SECRET_SCRUB_CHARS: usize = 8;

/// A reference resolved for one call, kept only long enough to scrub the
/// answer and any error the provider returns.
pub(crate) struct ResolvedSecret {
    /// The reference as written, which is what the redaction metadata names.
    pub(crate) reference: String,
    /// The plaintext that was handed to the tool. Never persisted.
    pub(crate) value: String,
}

/// Whether content is past either offload limit.
pub(crate) fn exceeds_offload_limits(content: &str, max_bytes: usize) -> bool {
    content.len() > max_bytes || content.lines().count() > OFFLOAD_MAX_LINES
}

/// Replace every occurrence of a resolved value with `***`.
///
/// Returns the scrubbed text and the references whose values were found, so
/// the redaction metadata can say what happened without carrying the value.
/// Longest first: when one secret contains another, replacing the shorter one
/// first would leave a fragment of the longer one behind.
pub(crate) fn scrub_text(text: &str, secrets: &[ResolvedSecret]) -> (String, Vec<String>) {
    let mut out = text.to_string();
    let mut patterns = Vec::new();
    let mut ordered: Vec<&ResolvedSecret> =
        secrets.iter().filter(|secret| secret.value.len() >= MIN_SECRET_SCRUB_CHARS).collect();
    ordered.sort_by_key(|secret| std::cmp::Reverse(secret.value.len()));
    for secret in ordered {
        if out.contains(&secret.value) {
            out = out.replace(&secret.value, "***");
            patterns.push(secret.reference.clone());
        }
    }
    (out, patterns)
}

/// Scrub resolved values out of a provider's error message.
///
/// A shell that echoes its command line, or a server that quotes the header it
/// rejected, would otherwise put the value into `tool/failed` — durable, and
/// exported with the session.
pub(crate) fn scrub_provider_error(
    error: ToolProviderError,
    secrets: &[ResolvedSecret],
) -> ToolProviderError {
    match error {
        ToolProviderError::Failed { tool, message } => {
            let (message, _) = scrub_text(&message, secrets);
            ToolProviderError::Failed { tool, message }
        }
        ToolProviderError::Classified { tool, kind, message } => {
            let (message, _) = scrub_text(&message, secrets);
            ToolProviderError::Classified { tool, kind, message }
        }
        other => other,
    }
}

/// The head, a marker naming the full content's locator, and the tail.
///
/// The marker is a sentence the model can act on rather than a bare hash: it
/// says what happened, how much there was, and what to ask for to see it.
pub(crate) fn offload_preview(content: &str, artifact: &ArtifactRef) -> String {
    // Short-but-long-by-lines content would otherwise have its head and tail
    // overlap, printing the same text twice.
    let (head_end, tail_start) = if content.len() <= PREVIEW_HEAD_BYTES + PREVIEW_TAIL_BYTES {
        (content.len(), content.len())
    } else {
        (
            char_boundary_at_or_before(content, PREVIEW_HEAD_BYTES),
            char_boundary_at_or_after(content, PREVIEW_TAIL_BYTES),
        )
    };
    let marker = format!(
        "... output truncated; full content saved to {} ({} bytes, {} lines) ...",
        artifact.locator(),
        artifact.size,
        content.lines().count()
    );
    let head = &content[..head_end];
    let tail = &content[tail_start..];
    if tail.is_empty() { format!("{head}\n{marker}") } else { format!("{head}\n{marker}\n{tail}") }
}

/// The largest index at or below `max` that starts a character.
fn char_boundary_at_or_before(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// The smallest index at or above `len - from_end` that starts a character.
fn char_boundary_at_or_after(text: &str, from_end: usize) -> usize {
    if text.len() <= from_end {
        return 0;
    }
    let mut start = text.len() - from_end;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    start
}
