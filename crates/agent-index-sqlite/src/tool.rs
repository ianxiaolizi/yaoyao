//! The `memory.search` tool: the same index, offered to the model.
//!
//! The tool is *bound* to one session at construction, because
//! `ToolInvocation` carries no session and deriving one from the arguments
//! would let a model name a session it cannot see. The binding is the same
//! answer the command path gives: the run's own history, never another's.
//!
//! The result is ordinary tool output, so the loop writes it as
//! `tool/completed` and a replay derives the same conversation the model was
//! shown. Nothing is injected into the transcript that the log does not hold.

use std::sync::Arc;

use agent_ports::memory_index::{MemoryHit, MemoryIndex, MemoryQuery};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::ids::SessionId;
use agent_protocol::permissions::Capability;

/// The name this tool is offered under.
pub const TOOL_NAME: &str = "memory.search";

/// How many hits the tool returns when the model names no limit.
const DEFAULT_LIMIT: usize = 5;

/// The most hits one call may ask for. A larger request is clamped rather than
/// refused: the model asked for more than it needs, not for something wrong.
const MAX_LIMIT: usize = 20;

/// A provider offering a session-bound search over the index.
pub struct MemoryToolProvider {
    index: Arc<dyn MemoryIndex>,
    session: SessionId,
}

impl MemoryToolProvider {
    pub fn new(index: Arc<dyn MemoryIndex>, session: SessionId) -> Self {
        Self { index, session }
    }
}

#[async_trait::async_trait]
impl ToolProvider for MemoryToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: TOOL_NAME.to_string(),
            description: "Search this session's earlier turns, assistant replies and tool output \
                          for a word or phrase. Returns matches newest first, each with the event \
                          type and a short snippet."
                .to_string(),
            source: ToolSource::Builtin,
            side_effect: ToolSideEffect::Pure,
            capability_requirements: vec![Capability::new(Capability::MEMORY_READ)],
            idempotent: true,
            timeout_ms: Some(10_000),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 }
                }
            }),
        }]
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        (tool_name == TOOL_NAME).then_some(ToolSideEffect::Pure)
    }

    /// No argument names a filesystem path: the tool reads the index, and the
    /// index has already materialized only the session's own text.
    fn path_arguments(&self, _tool_name: &str) -> Vec<String> {
        Vec::new()
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        if invocation.tool_name != TOOL_NAME {
            return Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: vec![TOOL_NAME.to_string()],
            });
        }

        let query = invocation
            .arguments
            .get("query")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| failed("missing 'query'"))?;
        let limit = limit_argument(invocation)?;

        let hits = self
            .index
            .search(&MemoryQuery {
                session_id: self.session.clone(),
                text: query.to_string(),
                limit,
            })
            .await
            .map_err(|error| failed(&error.to_string()))?;
        Ok(ToolResult::completed(render(query, &hits)))
    }
}

fn limit_argument(invocation: &ToolInvocation) -> Result<usize, ToolProviderError> {
    match invocation.arguments.get("limit") {
        None | Some(serde_json::Value::Null) => Ok(DEFAULT_LIMIT),
        Some(value) => {
            let limit = value
                .as_u64()
                .filter(|limit| *limit > 0)
                .ok_or_else(|| failed("'limit' must be a positive integer when given"))?;
            Ok((limit as usize).min(MAX_LIMIT))
        }
    }
}

/// The model-facing result: one line per hit, newest first.
///
/// Newlines inside a snippet become spaces so one hit stays one line — the
/// model reads the numbering, not a layout that changes with the text it
/// matched.
fn render(query: &str, hits: &[MemoryHit]) -> String {
    if hits.is_empty() {
        return format!("no matches for '{query}' in this session");
    }
    let mut text = format!("{} matches for '{query}' (newest first):", hits.len());
    for hit in hits {
        text.push('\n');
        text.push_str(&format!(
            "{}  {}  {}",
            hit.seq.as_u64(),
            hit.event_type,
            hit.snippet.replace('\n', " ")
        ));
    }
    text
}

fn failed(message: &str) -> ToolProviderError {
    ToolProviderError::Failed { tool: TOOL_NAME.to_string(), message: message.to_string() }
}

#[cfg(test)]
mod tests {
    use agent_ports::memory_index::MemoryIndexError;
    use agent_protocol::ids::EventSeq;
    use agent_protocol::ids::{StepId, TurnId};

    use super::*;

    struct FixedIndex {
        hits: Vec<MemoryHit>,
        seen: std::sync::Mutex<Vec<MemoryQuery>>,
    }

    impl FixedIndex {
        fn new(hits: Vec<MemoryHit>) -> Self {
            Self { hits, seen: std::sync::Mutex::new(Vec::new()) }
        }
    }

    #[async_trait::async_trait]
    impl MemoryIndex for FixedIndex {
        async fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryHit>, MemoryIndexError> {
            self.seen.lock().unwrap().push(query.clone());
            Ok(self.hits.iter().take(query.limit).cloned().collect())
        }

        async fn rebuild(&self, _session_id: &SessionId) -> Result<usize, MemoryIndexError> {
            Ok(self.hits.len())
        }
    }

    fn hit(seq: u64, text: &str) -> MemoryHit {
        MemoryHit {
            seq: EventSeq::new(seq),
            event_type: "inbox/appended".to_string(),
            turn_id: Some(TurnId::from_string("t1")),
            step_id: None::<StepId>,
            snippet: text.to_string(),
        }
    }

    fn invocation(arguments: serde_json::Value) -> ToolInvocation {
        ToolInvocation::proposed(TOOL_NAME, arguments)
    }

    fn provider(hits: Vec<MemoryHit>) -> (MemoryToolProvider, Arc<FixedIndex>) {
        let index = Arc::new(FixedIndex::new(hits));
        (MemoryToolProvider::new(index.clone(), SessionId::from_string("s1")), index)
    }

    #[test]
    fn the_definition_carries_the_read_capability_and_no_path() {
        let (provider, _) = provider(Vec::new());
        let definitions = provider.definitions();
        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];
        assert_eq!(definition.name, TOOL_NAME);
        assert_eq!(definition.source, ToolSource::Builtin);
        assert_eq!(definition.side_effect, ToolSideEffect::Pure);
        assert_eq!(
            definition.capability_requirements,
            vec![Capability::new(Capability::MEMORY_READ)]
        );
        assert!(definition.idempotent);
        assert!(provider.path_arguments(TOOL_NAME).is_empty());
        assert_eq!(provider.side_effect(TOOL_NAME), Some(ToolSideEffect::Pure));
        assert_eq!(provider.side_effect("fs.read"), None);
    }

    #[tokio::test]
    async fn a_hit_list_is_rendered_newest_first_with_the_session_bound() {
        let (provider, index) = provider(vec![hit(9, "a xylophone"), hit(4, "xylophone again")]);
        let result = provider
            .invoke(&invocation(serde_json::json!({"query": "xylophone"})))
            .await
            .expect("the search runs");

        assert_eq!(result.status, agent_protocol::tool::ToolStatus::Completed);
        assert!(result.content.starts_with("2 matches for 'xylophone' (newest first):"));
        assert!(result.content.contains("9  inbox/appended  a xylophone"));
        assert!(result.content.contains("4  inbox/appended  xylophone again"));

        let seen = index.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].session_id, SessionId::from_string("s1"));
        assert_eq!(seen[0].text, "xylophone");
        assert_eq!(seen[0].limit, DEFAULT_LIMIT);
    }

    #[tokio::test]
    async fn a_limit_is_clamped_and_a_bad_one_is_refused() {
        let (provider, index) = provider(Vec::new());
        provider
            .invoke(&invocation(serde_json::json!({"query": "x", "limit": 1000})))
            .await
            .expect("clamped");
        assert_eq!(index.seen.lock().unwrap()[0].limit, MAX_LIMIT);

        for bad in [serde_json::json!(0), serde_json::json!(-1), serde_json::json!("10")] {
            let error = provider
                .invoke(&invocation(serde_json::json!({"query": "x", "limit": bad})))
                .await
                .expect_err("a bad limit is refused");
            assert!(matches!(error, ToolProviderError::Failed { .. }));
        }
    }

    #[tokio::test]
    async fn a_blank_query_is_refused_rather_than_searched() {
        let (provider, index) = provider(Vec::new());
        for arguments in [
            serde_json::json!({}),
            serde_json::json!({"query": "   "}),
            serde_json::json!({"query": 7}),
        ] {
            let error = provider.invoke(&invocation(arguments)).await.expect_err("refused");
            assert!(matches!(error, ToolProviderError::Failed { .. }));
        }
        assert!(index.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_matches_is_still_a_successful_result() {
        let (provider, _) = provider(Vec::new());
        let result = provider
            .invoke(&invocation(serde_json::json!({"query": "zeppelin"})))
            .await
            .expect("the search runs");
        assert_eq!(result.content, "no matches for 'zeppelin' in this session");
    }

    #[tokio::test]
    async fn another_tool_name_is_unknown_here() {
        let (provider, _) = provider(Vec::new());
        let error = provider
            .invoke(&ToolInvocation::proposed("fs.read", serde_json::json!({})))
            .await
            .expect_err("not this provider's tool");
        assert!(matches!(
            error,
            ToolProviderError::UnknownTool { tool, available }
                if tool == "fs.read" && available == vec![TOOL_NAME.to_string()]
        ));
    }
}
