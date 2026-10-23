//! CompositeToolProvider — one tool surface assembled from several providers.
//!
//! The gateway is handed a single [`ToolProvider`] and asks it to describe and
//! run tools. Without this, adding a second tool family would mean teaching the
//! gateway about a list, which is assembly's job and nobody else's.
//!
//! [`ToolProvider`]: agent_ports::tool_provider::ToolProvider

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
};
use agent_protocol::tool::ToolSource;

/// A surface that cannot be presented as one, because two providers want the
/// same name.
#[derive(Debug, thiserror::Error)]
pub enum ToolSurfaceError {
    #[error(
        "tool '{name}' is declared twice, as a {} and as a {}; a name has to reach exactly one \
         implementation",
        kind(*first),
        kind(*second)
    )]
    DuplicateName { name: String, first: ToolSource, second: ToolSource },
}

fn kind(source: ToolSource) -> &'static str {
    match source {
        ToolSource::Builtin => "built-in tool",
        ToolSource::Plugin => "plugin tool",
        ToolSource::Mcp => "MCP tool",
    }
}

/// Presents several providers as one, and refuses to present a name twice.
pub struct CompositeToolProvider {
    providers: Vec<Arc<dyn ToolProvider>>,
}

impl CompositeToolProvider {
    /// Present `providers` as one surface.
    ///
    /// A surface where two providers declare the same name is refused rather
    /// than assembled. Routing sends a call to the first provider that claims
    /// it, so the second claimant is not an implementation that lost a vote —
    /// it is one nothing can ever reach, while the model is still offered the
    /// name twice. Which of them a call actually lands on would turn on the
    /// order assembly happened to pass the providers in, and a surface whose
    /// meaning depends on that is not one anybody chose.
    ///
    /// The check runs once, here. Staying true afterwards is the registry's
    /// job: a plugin that registers later is refused if it takes a name that is
    /// already spoken for, so a surface that was unique when it was built does
    /// not drift into a duplicate while it is in use.
    pub fn new(providers: Vec<Arc<dyn ToolProvider>>) -> Result<Self, ToolSurfaceError> {
        let composite = Self { providers };
        let mut seen: BTreeMap<String, ToolSource> = BTreeMap::new();
        for definition in composite.definitions() {
            if let Some(first) = seen.insert(definition.name.clone(), definition.source) {
                return Err(ToolSurfaceError::DuplicateName {
                    name: definition.name,
                    first,
                    second: definition.source,
                });
            }
        }
        Ok(composite)
    }

    /// The provider that owns `tool_name`.
    ///
    /// Both questions are asked because a provider is allowed to answer either
    /// one alone: `side_effect` is authoritative when it knows the tool, and a
    /// declared definition is the fallback for a provider that describes its
    /// surface but leaves the side-effect classification to the adapter.
    ///
    /// Asking the two questions separately is safe only because [`new`] made
    /// the names unique: with one claimant per name, "the first provider whose
    /// `side_effect` answers" and "the first provider that declares the name"
    /// cannot be two different providers.
    ///
    /// [`new`]: Self::new
    fn owner(&self, tool_name: &str) -> Option<&Arc<dyn ToolProvider>> {
        self.providers.iter().find(|provider| {
            provider.side_effect(tool_name).is_some()
                || provider.definitions().iter().any(|d| d.name == tool_name)
        })
    }
}

#[async_trait::async_trait]
impl ToolProvider for CompositeToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.providers.iter().flat_map(|provider| provider.definitions()).collect()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.providers.iter().find_map(|provider| provider.side_effect(tool_name))
    }

    /// Ask the provider that owns the tool. A name no member claims gets no
    /// path arguments — it is about to be refused as unknown, and inventing a
    /// boundary for it would be answering a question nobody asked.
    fn path_arguments(&self, tool_name: &str) -> Vec<String> {
        self.owner(tool_name).map(|provider| provider.path_arguments(tool_name)).unwrap_or_default()
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        match self.owner(&invocation.tool_name) {
            Some(provider) => provider.invoke(invocation).await,
            None => Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: self.definitions().into_iter().map(|d| d.name).collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, content: &str) -> Arc<dyn ToolProvider> {
        Arc::new(FakeToolProvider::new().with_tool(name, content, ToolSideEffect::Read))
    }

    use agent_testkit::FakeToolProvider;

    fn composite() -> CompositeToolProvider {
        CompositeToolProvider::new(vec![provider("fs.read", "file"), provider("git.status", "{}")])
            .expect("two providers, two names")
    }

    #[test]
    fn every_providers_definitions_are_offered() {
        let names: Vec<String> = composite().definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"fs.read".to_string()));
        assert!(names.contains(&"git.status".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn a_side_effect_is_answered_by_whoever_declares_the_tool() {
        assert_eq!(composite().side_effect("git.status"), Some(ToolSideEffect::Read));
        assert_eq!(composite().side_effect("shell.run"), None);
    }

    #[tokio::test]
    async fn a_call_reaches_the_provider_that_owns_the_name() {
        let result = composite()
            .invoke(&ToolInvocation::proposed("git.status", serde_json::json!({})))
            .await
            .unwrap();
        // The answer is git's, not the first provider's: routing by name is the
        // whole point of the fan-out.
        assert_eq!(result.content, "{}");
    }

    #[tokio::test]
    async fn an_unknown_tool_is_rejected_rather_than_sent_somewhere() {
        let err = composite()
            .invoke(&ToolInvocation::proposed("git.push", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolProviderError::UnknownTool { .. }), "{err:?}");
    }

    #[test]
    fn two_providers_claiming_one_name_are_refused() {
        let err = CompositeToolProvider::new(vec![
            provider("fs.read", "first"),
            provider("fs.read", "second"),
        ])
        .err()
        .expect("a name with two claimants is not a surface");
        let message = err.to_string();
        // Whichever provider happens to be asked first would decide which of
        // these two runs, so the surface is refused rather than assembled. The
        // refusal names the tool and both sides, because "duplicate name" alone
        // does not say which provider to go and fix.
        assert!(message.contains("fs.read"), "{message}");
        assert!(matches!(err, ToolSurfaceError::DuplicateName { first: ToolSource::Builtin, .. }));
    }
}
