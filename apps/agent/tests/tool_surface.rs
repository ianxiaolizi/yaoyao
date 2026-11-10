//! 阶段 A gate 14 — the first-release tool surface carries no way off the
//! machine.
//!
//! The design's threshold is "首期不注册外部副作用工具": before the agent is
//! given autonomous permission (stage B), what it can reach must be the
//! workspace and nothing else. Two halves, checked here so the claim is a test
//! and not a reading of the code at one point in time:
//!
//! * the built-in families offer no send / publish / mail / upload / deploy /
//!   credential tool, and their names are an exact list — a family that grows a
//!   new tool fails this test until someone writes the name down and says why;
//! * the shipped default policy grants no way out: `network.request` is
//!   advertised by the environment but no rule allows it (so `web.request` is
//!   dropped from the model's surface), and `process.spawn` is `ask` (so
//!   `shell.run` runs only under an approval).
//!
//! A profile may still opt into the network — that is a person's decision, made
//! in configuration, and CONTRACT §7 #173's neighbours record what it takes.

use std::path::PathBuf;
use std::sync::Arc;

use agent_kernel::policy::default_policy_with;
use agent_ports::tool_provider::{ToolProvider, ToolSideEffect};
use agent_protocol::permissions::{Capability, PolicyDecision};
use agent_tools_fs::{FsToolProvider, LocalExecutionEnvironment};
use agent_tools_git::GitToolProvider;
use agent_tools_web::WebToolProvider;

/// The built-in families the assembled binary registers, as `stack.rs` builds
/// them. Memory tools are absent on purpose: long-term memory is off unless the
/// user configuration turns it on, and the facts provider is built only then.
fn builtin_families(root: &PathBuf) -> Vec<Arc<dyn ToolProvider>> {
    vec![
        Arc::new(FsToolProvider::new(Arc::new(LocalExecutionEnvironment::new(root)))),
        Arc::new(GitToolProvider::new(root.clone())),
        Arc::new(WebToolProvider::new()),
    ]
}

/// Words no first-release tool name may contain. The list is the design
/// threshold written as strings, not a taste in naming.
const FORBIDDEN: [&str; 12] = [
    "send",
    "publish",
    "post",
    "mail",
    "upload",
    "deploy",
    "push",
    "commit",
    "credential",
    "secret",
    "token",
    "auth",
];

#[test]
fn the_built_in_families_offer_no_way_off_the_machine() {
    // A root that need not exist: these tests read definitions, never call.
    let root = std::env::temp_dir().join("yaoyao-tool-surface");
    let families = builtin_families(&root);

    let mut names: Vec<String> = families
        .iter()
        .flat_map(|family| family.definitions())
        .map(|definition| definition.name)
        .collect();
    names.sort();

    // The exact surface. Adding a tool means editing this line, which is the
    // point: the first release's surface is a decision, not an accumulation.
    assert_eq!(
        names,
        vec![
            "fs.edit",
            "fs.glob",
            "fs.grep",
            "fs.list",
            "fs.read",
            "fs.write",
            "git.diff",
            "git.status",
            "shell.run",
            "web.request",
        ],
        "a built-in family changed what it offers"
    );

    for name in &names {
        for word in FORBIDDEN {
            assert!(
                !name.to_ascii_lowercase().contains(word),
                "`{name}` reads as a {word}-class tool; the first release registers none"
            );
        }
    }

    // One network tool, and it is the one the default policy drops below.
    let network: Vec<String> = families
        .iter()
        .flat_map(|family| family.definitions())
        .filter(|definition| definition.side_effect == ToolSideEffect::Network)
        .map(|definition| definition.name)
        .collect();
    assert_eq!(network, vec![String::from("web.request")], "only one tool reaches the network");
}

#[test]
fn the_shipped_policy_grants_no_way_out() {
    let policy = default_policy_with(&agent_kernel::policy::default_sandbox_capabilities());

    // Advertised by the environment, denied by every default rule: a run that
    // wants `web.request` has to say so in a profile.
    let network = Capability::new(Capability::NETWORK_REQUEST);
    assert_eq!(
        policy.evaluate(std::slice::from_ref(&network), &[]),
        PolicyDecision::Deny,
        "network.request is advertised but ungranted"
    );

    // `shell.run` is on the surface, and every call has to be approved.
    let spawn = Capability::new(Capability::PROCESS_SPAWN);
    assert_eq!(
        policy.evaluate(std::slice::from_ref(&spawn), &[]),
        PolicyDecision::Ask,
        "process.spawn runs only under an approval"
    );

    // The workspace itself is the part that is allowed without asking; the
    // boundary is the sandbox's, not this test's.
    let read = Capability::new(Capability::FS_READ);
    assert_eq!(policy.evaluate(std::slice::from_ref(&read), &[]), PolicyDecision::Allow);
}
