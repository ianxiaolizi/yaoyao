//! Namespacing and classification: what an MCP tool becomes on this side.
//!
//! Two servers may both offer a tool called `read`, and a server may offer a
//! name that is not a legal one here. Both are answered by the same rule: a
//! tool reaches the surface as `mcp__<server>__<tool>`, illegal characters
//! become `_`, and a name that is too long or that collides after sanitizing
//! carries an eight-hex disambiguator derived from `(server, tool)`.
//!
//! The digest is the protocol's one hash ([`fnv1a_hex`]) rather than a new one:
//! the same `(server, tool)` must produce the same name on every run and every
//! machine, so the hash has to be a function of the input and nothing else.

use agent_ports::tool_provider::{ToolDefinition, ToolSideEffect};
use agent_protocol::digest::fnv1a_hex;
use agent_protocol::permissions::Capability;
use agent_protocol::tool::ToolSource;

use crate::connection::{McpAnnotations, McpTool};

/// The longest tool name the surface accepts.
///
/// Sixty-four is the provider-side ceiling the ecosystem converges on
/// (OpenAI-compatible gateways enforce it), so it is enforced here rather than
/// discovered by a rejected request.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// The separator between the pieces of a namespaced name.
const SEP: &str = "__";

/// Replace anything outside the conservative name set with `_`.
///
/// A dot is *not* in the set. `fs.read` and `git.diff` are built-in names, and a
/// dotted MCP tool would read as one of them — the same blur the `mcp__` prefix
/// exists to avoid. Inside the tool segment a dot becomes `_`.
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') { c } else { '_' })
        .collect()
}

/// The un-disambiguated namespaced name for a tool.
pub fn plain_name(server: &str, tool: &str) -> String {
    format!("mcp{SEP}{}{SEP}{}", sanitize(server), sanitize(tool))
}

/// `name` with an eight-hex suffix derived from `(server, tool)`, truncated if
/// needed so the whole thing fits in [`MAX_TOOL_NAME_LEN`].
///
/// ASCII throughout (both inputs have been sanitized), so truncation cannot land
/// mid-character.
pub fn disambiguated_name(server: &str, tool: &str, name: &str) -> String {
    let digest = fnv1a_hex(format!("{server}\0{tool}").as_bytes());
    let suffix = format!("{SEP}{}", &digest[..8]);
    let keep = MAX_TOOL_NAME_LEN.saturating_sub(suffix.len());
    let head = if name.len() > keep { &name[..keep] } else { name };
    format!("{head}{suffix}")
}

/// The side-effect class an MCP tool carries, from its annotation hints.
///
/// The order is the spec's own: `readOnlyHint` first, because a read-only tool
/// is read-only whatever else it says. `destructiveHint` is only meaningful when
/// the tool is not read-only, and `openWorldHint` — a tool that talks to an open
/// set of external entities — is a weaker claim than "this modifies things", so
/// it is consulted after. A tool that says nothing is `Write`: the default has
/// to be the cautious one, because the annotations are hints and the absent case
/// is the one an untrusted server benefits from.
pub fn side_effect(annotations: &McpAnnotations) -> ToolSideEffect {
    match (annotations.read_only, annotations.destructive, annotations.open_world) {
        (Some(true), _, _) => ToolSideEffect::Read,
        (_, Some(true), _) => ToolSideEffect::Destructive,
        (_, _, Some(true)) => ToolSideEffect::Network,
        _ => ToolSideEffect::Write,
    }
}

/// Whether the tool may be retried from the annotation, defaulting to no.
pub fn idempotent(annotations: &McpAnnotations) -> bool {
    annotations.idempotent.unwrap_or(false)
}

/// Turn a discovered tool into the definition the surface offers.
///
/// `requires` is the server's declared capability set, inherited by every tool
/// under it: MCP tools do not declare capabilities of their own, so the server
/// entry is the only place the claim can come from.
pub fn definition(
    namespaced_name: &str,
    tool: &McpTool,
    requires: &[Capability],
    timeout_ms: Option<u64>,
) -> ToolDefinition {
    ToolDefinition {
        name: namespaced_name.to_string(),
        description: tool.description.clone(),
        source: ToolSource::Mcp,
        side_effect: side_effect(&tool.annotations),
        capability_requirements: requires.to_vec(),
        idempotent: idempotent(&tool.annotations),
        timeout_ms,
        input_schema: tool.input_schema.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: name.to_string(),
            description: "does a thing".to_string(),
            input_schema: json!({ "type": "object" }),
            annotations: McpAnnotations::default(),
        }
    }

    #[test]
    fn a_plain_name_is_the_server_and_tool_wrapped_in_the_mcp_prefix() {
        assert_eq!(plain_name("files", "read"), "mcp__files__read");
    }

    #[test]
    fn illegal_characters_become_underscores() {
        assert_eq!(plain_name("my server!", "read/file"), "mcp__my_server___read_file");
        // A dot inside the tool segment is not kept: `note.write` must not come
        // out looking like `fs.read`.
        assert_eq!(plain_name("files", "note.write"), "mcp__files__note_write");
        assert_eq!(plain_name("files", "a.b/c d"), "mcp__files__a_b_c_d");
    }

    #[test]
    fn side_effect_follows_the_annotations_and_defaults_to_write() {
        let read = McpAnnotations { read_only: Some(true), ..Default::default() };
        assert_eq!(side_effect(&read), ToolSideEffect::Read);

        let destructive = McpAnnotations { destructive: Some(true), ..Default::default() };
        assert_eq!(side_effect(&destructive), ToolSideEffect::Destructive);

        let open = McpAnnotations { open_world: Some(true), ..Default::default() };
        assert_eq!(side_effect(&open), ToolSideEffect::Network);

        // A read-only tool is read-only even if the server also flags it open:
        // "does not modify" is the stronger, more useful claim.
        let both =
            McpAnnotations { read_only: Some(true), open_world: Some(true), ..Default::default() };
        assert_eq!(side_effect(&both), ToolSideEffect::Read);

        // Nothing said -> the cautious class.
        assert_eq!(side_effect(&McpAnnotations::default()), ToolSideEffect::Write);
        assert_eq!(
            side_effect(&McpAnnotations { read_only: Some(false), ..Default::default() }),
            ToolSideEffect::Write
        );
    }

    #[test]
    fn idempotence_defaults_to_false() {
        assert!(!idempotent(&McpAnnotations::default()));
        assert!(idempotent(&McpAnnotations { idempotent: Some(true), ..Default::default() }));
    }

    #[test]
    fn a_long_name_is_truncated_and_keeps_the_server_and_tool_readable_in_the_head() {
        let server = "s".repeat(40);
        let tool = "t".repeat(40);
        let plain = plain_name(&server, &tool);
        assert!(plain.len() > MAX_TOOL_NAME_LEN);

        let name = disambiguated_name(&server, &tool, &plain);
        assert_eq!(name.len(), MAX_TOOL_NAME_LEN);
        assert!(name.starts_with("mcp__ss"));
    }

    #[test]
    fn the_disambiguator_is_stable_and_depends_on_both_parts() {
        let a = disambiguated_name("files", "read", "mcp__files__read");
        assert_eq!(a, disambiguated_name("files", "read", "mcp__files__read"));
        // Different tool, same server -> different suffix.
        assert_ne!(a, disambiguated_name("files", "write", "mcp__files__read"));
        // Same tool, different server -> different suffix.
        assert_ne!(a, disambiguated_name("other", "read", "mcp__files__read"));
    }

    #[test]
    fn a_definition_carries_the_servers_requires_and_the_mcp_source() {
        let requires = vec![Capability::new(Capability::FS_READ)];
        let definition = definition("mcp__files__read", &tool("read"), &requires, Some(1000));

        assert_eq!(definition.name, "mcp__files__read");
        assert_eq!(definition.source, ToolSource::Mcp);
        assert_eq!(definition.capability_requirements, requires);
        assert!(!definition.idempotent);
        assert_eq!(definition.timeout_ms, Some(1000));
        assert_eq!(definition.description, "does a thing");
    }
}
