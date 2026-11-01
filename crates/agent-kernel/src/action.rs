//! Action facts: what one tool call proposes, in the form a decision is bound
//! to.
//!
//! A permission decision that says "the user allowed `fs.write`" authorises
//! every write there is; one that says "the user allowed this digest, touching
//! this path, in this task revision" authorises one call. This module is the
//! one place those facts are built, so the digest a lease is bound to and the
//! digest a result is recorded under cannot be two different numbers.
//!
//! Three questions are answered here, each with one producer:
//!
//! * **What is the risk?** The provider's own declaration when it has one,
//!   otherwise the side-effect class, otherwise [`RiskLevel::UNKNOWN`]. Nothing
//!   can lower that answer: [`ActionFacts::raised_to`] can only raise it.
//! * **What does it touch?** Declared path arguments, a `command` line, a `url`
//!   origin, and every `secret://` reference — never a value.
//! * **Which call is this?** A SHA-256 over the stable fields, computed by
//!   [`ActionFacts::from_spec`].
//!
//! The summary a person reads is built from targets rather than from the raw
//! arguments: an argument is arbitrary text a model wrote, and a record that
//! quoted it would carry whatever it contained.

use agent_ports::tool_provider::{ToolInvocation, ToolProvider};
use agent_protocol::permission::{ActionFacts, ActionSpec, ActionTarget, Reversibility, RiskLevel};
use agent_protocol::permissions::Capability;
use agent_protocol::redactor::Redactor;

/// The scheme a tool argument uses to name a secret instead of carrying one.
pub(crate) const SECRET_SCHEME: &str = "secret://";

/// The program an action fact names for a whole command line.
///
/// This build runs a model-written command through the backend's shell
/// (`ExecutionEnvironment::shell_invocation`), so the line — not a program the
/// gateway could name — is what a person approves. The digest still changes
/// with the line, because the line is the argument.
const SHELL_PROGRAM: &str = "shell";

/// The facts one call produces, built from what the provider declares.
///
/// `capabilities` is the tool's own declaration, read by the caller because the
/// gateway already has it for the policy question and a second read could see a
/// surface that changed between the two.
pub(crate) fn action_facts(
    provider: &dyn ToolProvider,
    capabilities: &[Capability],
    task_id: &str,
    task_revision: u64,
    invocation: &ToolInvocation,
) -> ActionFacts {
    let risk = risk_for(provider, invocation);
    let targets = targets_for(provider, capabilities, invocation);
    ActionFacts::from_spec(ActionSpec {
        task_id: task_id.to_string(),
        task_revision,
        tool_name: invocation.tool_name.clone(),
        capabilities: capabilities.to_vec(),
        risk,
        reversibility: Reversibility::for_risk(risk),
        summary: action_summary(&invocation.tool_name, &targets),
        targets,
        arguments: invocation.arguments.clone(),
    })
}

/// The risk a call carries: the provider's own answer when it has one,
/// otherwise the side-effect class. Neither answering is
/// [`RiskLevel::UNKNOWN`] — a tool nobody classified is the most dangerous
/// thing a surface can offer, not the least.
fn risk_for(provider: &dyn ToolProvider, invocation: &ToolInvocation) -> RiskLevel {
    if let Some(risk) = provider.risk(invocation) {
        return risk;
    }
    match provider.side_effect(&invocation.tool_name) {
        Some(effect) => RiskLevel::floor_for(effect),
        None => RiskLevel::UNKNOWN,
    }
}

/// What the call would touch, in the vocabulary a decision is bound to.
///
/// Every declared path argument is a target (K-08: the declaration, never a
/// conventional key name), a `command` argument is the shell's own line, a
/// `url` argument is the origin the address guard will check, and every
/// `secret://` reference is named — by reference only, because that is the only
/// form arguments ever carry.
fn targets_for(
    provider: &dyn ToolProvider,
    capabilities: &[Capability],
    invocation: &ToolInvocation,
) -> Vec<ActionTarget> {
    let mut targets = Vec::new();
    for key in provider.path_arguments(&invocation.tool_name) {
        if let Some(path) = invocation.arguments.get(&key).and_then(|v| v.as_str()) {
            targets
                .push(ActionTarget::WorkspacePath { path: path.to_string(), writable_root: None });
        }
    }
    if let Some(command) = invocation.arguments.get("command").and_then(|v| v.as_str()) {
        targets.push(ActionTarget::Command {
            program: SHELL_PROGRAM.to_string(),
            args: vec![command.to_string()],
            cwd: invocation.arguments.get("cwd").and_then(|v| v.as_str()).map(str::to_string),
        });
    }
    if let Some(target) = network_target(&invocation.arguments) {
        targets.push(target);
    }
    collect_secret_references(&invocation.arguments, &mut targets);
    if capabilities.iter().any(|capability| capability.as_str() == Capability::MEMORY_WRITE) {
        targets.push(ActionTarget::DataStore { name: "memory".to_string() });
    }
    if targets.is_empty() {
        targets.push(ActionTarget::Other { tool: invocation.tool_name.clone() });
    }
    targets
}

/// The arguments as a redacted, size-bounded string for a person to read.
///
/// This is the fallback summary — an approval request that has no action facts
/// beside it. The preferred sentence is [`action_summary`], built from targets;
/// this one quotes the arguments, so everything it shows has been through the
/// redactor first.
pub(crate) fn summarize_arguments(redactor: &Redactor, invocation: &ToolInvocation) -> String {
    let raw = serde_json::to_string(&invocation.arguments).unwrap_or_default();
    let (scrubbed, _) = redactor.redact(&raw);
    if scrubbed.len() > 200 {
        // 200 is a *size* budget, so the cut is a byte index — but the
        // arguments are whatever the caller sent, and in this repo that is
        // routinely not ASCII. The index has to be moved back to a character
        // boundary or the slice panics, which would happen while asking a
        // person to decide. `chars().take(200)` would also be safe and is the
        // wrong fix: it counts characters, which for Chinese text is a bound
        // three times larger.
        format!("{}…", &scrubbed[..scrubbed.floor_char_boundary(200)])
    } else {
        scrubbed
    }
}

/// A one-line description of an action, built from its targets rather than
/// from the raw arguments.
///
/// The approval card shows this sentence; the digest never does. Building it
/// from targets is what keeps arbitrary argument text — a value a model pasted
/// where a reference belonged — out of the record while still saying what the
/// call would do.
fn action_summary(tool: &str, targets: &[ActionTarget]) -> String {
    let described: Vec<String> = targets
        .iter()
        .map(|target| match target {
            ActionTarget::WorkspacePath { path, .. } => path.clone(),
            ActionTarget::Command { args, cwd, .. } => {
                let line = args.join(" ");
                match cwd {
                    Some(cwd) => format!("{line} (in {cwd})"),
                    None => line,
                }
            }
            ActionTarget::Network { scheme, host, port, method } => {
                format!("{method} {scheme}://{host}:{port}")
            }
            ActionTarget::Secret { reference } => reference.clone(),
            ActionTarget::DataStore { name } => name.clone(),
            ActionTarget::Other { tool } => tool.clone(),
        })
        .collect();
    let text = format!("{tool} {}", described.join(", "));
    if text.len() <= 200 {
        return text;
    }
    format!("{}…", &text[..text.floor_char_boundary(200)])
}

/// The `url` argument as a network target, when it can be read as one.
///
/// Parsed here rather than through a URL crate because the gateway only needs
/// the stable identity of the destination — scheme, host, port, method — not a
/// request-complete URL. A value this cannot read becomes no target at all; the
/// arguments' own digest still changes with it, and the web tool refuses the
/// call itself.
fn network_target(arguments: &serde_json::Value) -> Option<ActionTarget> {
    let url = arguments.get("url")?.as_str()?;
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Userinfo is not part of the destination; the web guard refuses it, and a
    // target that carried it would put a credential-shaped string into a record.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = split_host_port(authority, &scheme)?;
    let method = arguments
        .get("method")
        .and_then(|value| value.as_str())
        .map(|method| method.to_ascii_uppercase())
        .unwrap_or_else(|| "GET".to_string());
    Some(ActionTarget::Network { scheme, host, port, method })
}

/// Split `host[:port]` — including the bracketed IPv6 form — with the scheme's
/// default port when none is stated.
fn split_host_port(authority: &str, scheme: &str) -> Option<(String, u16)> {
    let default_port = if scheme == "https" { 443 } else { 80 };
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = format!("[{}]", &rest[..end]);
        let after = &rest[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host.to_string(), port.parse().ok()?)),
        Some(_) => None,
        None => Some((authority.to_string(), default_port)),
    }
}

/// Every `secret://<source>/<name>` reference in a value, as targets.
///
/// References only: the shape arguments carry is the reference, so a plaintext
/// value can only appear if a caller put it there against the contract — and
/// then it is hashed with the arguments, never turned into a target.
fn collect_secret_references(value: &serde_json::Value, targets: &mut Vec<ActionTarget>) {
    match value {
        serde_json::Value::String(text) => {
            let mut rest = text.as_str();
            while let Some(start) = rest.find(SECRET_SCHEME) {
                let after = &rest[start + SECRET_SCHEME.len()..];
                let end = after
                    .find(|ch: char| {
                        !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/'))
                    })
                    .unwrap_or(after.len());
                targets.push(ActionTarget::Secret {
                    reference: format!("{SECRET_SCHEME}{}", &after[..end]),
                });
                rest = &after[end..];
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_secret_references(item, targets);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_secret_references(item, targets);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Arc;

    use agent_ports::tool_provider::{
        ToolDefinition, ToolProviderError, ToolResult, ToolSideEffect, ToolSource,
    };
    use agent_protocol::permissions::Workspace;
    use agent_testkit::{FakeEventStore, InMemoryArtifactStore};

    use crate::approval::ApprovalService;
    use crate::guarded::GuardedToolProvider;
    use crate::policy::PolicyEngine;
    use crate::resource_guard::ResourceGuard;

    /// A surface that declares its tools and nothing else. These tests ask what
    /// a call would touch, never what it returns.
    struct Declared {
        defs: Vec<ToolDefinition>,
        paths: HashMap<String, Vec<String>>,
    }

    impl Declared {
        fn new(defs: Vec<ToolDefinition>) -> Self {
            let paths = HashMap::from([
                ("fs.read".to_string(), vec!["path".to_string()]),
                ("fs.write".to_string(), vec!["path".to_string()]),
            ]);
            Self { defs, paths }
        }
    }

    #[async_trait::async_trait]
    impl ToolProvider for Declared {
        fn definitions(&self) -> Vec<ToolDefinition> {
            self.defs.clone()
        }

        fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
            self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
        }

        fn path_arguments(&self, tool_name: &str) -> Vec<String> {
            self.paths.get(tool_name).cloned().unwrap_or_default()
        }

        async fn invoke(
            &self,
            invocation: &ToolInvocation,
        ) -> Result<ToolResult, ToolProviderError> {
            Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: Vec::new(),
            })
        }
    }

    fn def(name: &str, caps: &[&str], side_effect: ToolSideEffect) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("test tool {name}"),
            source: ToolSource::Builtin,
            side_effect,
            capability_requirements: caps.iter().map(|c| Capability::new(*c)).collect(),
            idempotent: true,
            timeout_ms: None,
            input_schema: serde_json::json!({}),
        }
    }

    fn workspace() -> Workspace {
        Workspace {
            workspace_root: std::env::current_dir().unwrap().display().to_string(),
            writable_roots: vec![],
            external_directories: vec![],
        }
    }

    fn gateway(defs: Vec<ToolDefinition>) -> GuardedToolProvider {
        GuardedToolProvider::new(
            Arc::new(Declared::new(defs)),
            PolicyEngine::new(),
            Arc::new(ApprovalService::new(Arc::new(FakeEventStore::new()))),
            ResourceGuard::new(workspace()),
            Arc::new(InMemoryArtifactStore::default()),
        )
    }

    #[test]
    fn an_unclassified_tool_degrades_to_l3() {
        // No definition, no side effect, no provider declaration: the gateway
        // cannot say what the call would do, and the highest risk is the only
        // answer that does not turn "unknown" into "harmless".
        let gateway = gateway(vec![]);

        let facts = gateway.action_facts("intent-1", 0, &invocation("mystery.tool"));

        assert_eq!(facts.risk, RiskLevel::UNKNOWN);
        assert_eq!(facts.reversibility, Reversibility::Irreversible);
        assert_eq!(facts.targets, vec![ActionTarget::Other { tool: "mystery.tool".to_string() }]);
    }

    #[test]
    fn the_side_effect_class_supplies_the_risk_floor() {
        let gateway = gateway(vec![
            def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read),
            def("fs.write", &[Capability::FS_WRITE], ToolSideEffect::Write),
            def("shell.run", &[Capability::PROCESS_SPAWN], ToolSideEffect::Destructive),
        ]);

        let read = gateway.action_facts("t", 0, &invocation("fs.read"));
        assert_eq!(read.risk, RiskLevel::L0);
        assert_eq!(read.reversibility, Reversibility::Reversible);

        let write = gateway.action_facts("t", 0, &invocation("fs.write"));
        assert_eq!(write.risk, RiskLevel::L1);

        let destructive = gateway.action_facts("t", 0, &invocation("shell.run"));
        assert_eq!(destructive.risk, RiskLevel::L3);
        assert_eq!(destructive.reversibility, Reversibility::Irreversible);
    }

    #[test]
    fn a_lower_risk_suggestion_cannot_downgrade_a_destructive_tool() {
        // An estimate (a classifier's, a heuristic's) answers "at least this
        // risky"; it never answers "less". A shell call stays L3 whatever says
        // otherwise, and so does the digest that names it.
        let gateway = gateway(vec![def(
            "shell.run",
            &[Capability::PROCESS_SPAWN],
            ToolSideEffect::Destructive,
        )]);

        let facts = gateway.action_facts(
            "t",
            0,
            &ToolInvocation::proposed(
                "shell.run",
                serde_json::json!({ "command": "rm -rf build" }),
            ),
        );
        assert_eq!(facts.risk, RiskLevel::L3);

        let softened = facts.raised_to(RiskLevel::L0);
        assert_eq!(softened.risk, RiskLevel::L3);
        assert_eq!(softened.digest, facts.digest);
    }

    #[test]
    fn a_path_change_changes_the_action_digest_at_the_gateway() {
        let gateway =
            gateway(vec![def("fs.write", &[Capability::FS_WRITE], ToolSideEffect::Write)]);

        let one = gateway.action_facts(
            "t",
            0,
            &ToolInvocation::proposed("fs.write", serde_json::json!({ "path": "a.txt" })),
        );
        let other = gateway.action_facts(
            "t",
            0,
            &ToolInvocation::proposed("fs.write", serde_json::json!({ "path": "b.txt" })),
        );

        assert_ne!(one.digest, other.digest);
        assert_eq!(
            one.targets,
            vec![ActionTarget::WorkspacePath { path: "a.txt".to_string(), writable_root: None }]
        );
    }

    #[test]
    fn a_command_line_and_its_working_directory_are_both_in_the_fingerprint() {
        let gateway = gateway(vec![def(
            "shell.run",
            &[Capability::PROCESS_SPAWN],
            ToolSideEffect::Destructive,
        )]);

        let facts = gateway.action_facts(
            "t",
            0,
            &ToolInvocation::proposed(
                "shell.run",
                serde_json::json!({ "command": "cargo test", "cwd": "crates/agent-kernel" }),
            ),
        );
        assert_eq!(
            facts.targets,
            vec![ActionTarget::Command {
                program: "shell".to_string(),
                args: vec!["cargo test".to_string()],
                cwd: Some("crates/agent-kernel".to_string()),
            }]
        );

        // A different working directory is a different action.
        let elsewhere = gateway.action_facts(
            "t",
            0,
            &ToolInvocation::proposed(
                "shell.run",
                serde_json::json!({ "command": "cargo test", "cwd": "apps/agent" }),
            ),
        );
        assert_ne!(facts.digest, elsewhere.digest);
    }

    #[test]
    fn a_url_argument_names_its_origin_and_method_but_no_userinfo() {
        let gateway = gateway(vec![def(
            "web.request",
            &[Capability::NETWORK_REQUEST],
            ToolSideEffect::Network,
        )]);

        let facts = gateway.action_facts(
            "t",
            0,
            &ToolInvocation::proposed(
                "web.request",
                serde_json::json!({ "url": "https://user:pass@docs.rs/index.html", "method": "get" }),
            ),
        );
        assert_eq!(
            facts.targets,
            vec![ActionTarget::Network {
                scheme: "https".to_string(),
                host: "docs.rs".to_string(),
                port: 443,
                method: "GET".to_string(),
            }],
            "userinfo is not part of the destination and must not enter a record"
        );
        assert_eq!(facts.risk, RiskLevel::L2, "a provider that declares nothing is the class");
    }

    #[test]
    fn a_secret_reference_is_a_target_and_its_value_is_never_one() {
        let gateway = gateway(vec![def(
            "shell.run",
            &[Capability::PROCESS_SPAWN],
            ToolSideEffect::Destructive,
        )]);

        let facts = gateway.action_facts(
            "t",
            0,
            &ToolInvocation::proposed(
                "shell.run",
                serde_json::json!({
                    "command": "curl -H 'Authorization: secret://env/TOKEN' x",
                    // A caller that pasted a value instead of a reference: the
                    // args digest covers it, the facts never carry it.
                    "note": "s3cr3t-plaintext",
                }),
            ),
        );
        assert!(
            facts.targets.iter().any(|target| target
                == &ActionTarget::Secret { reference: "secret://env/TOKEN".to_string() }),
            "{:?}",
            facts.targets
        );
        let rendered = serde_json::to_string(&facts).unwrap();
        assert!(!rendered.contains("s3cr3t-plaintext"), "{rendered}");
    }

    fn invocation(name: &str) -> ToolInvocation {
        ToolInvocation::proposed(name, serde_json::json!({}))
    }
}
