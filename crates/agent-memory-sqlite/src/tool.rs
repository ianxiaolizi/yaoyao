//! The fact tools the model is offered: `memory.recall`, `memory.remember` and
//! `memory.forget`.
//!
//! Three tools rather than one with a verb argument, because the three claims
//! are different: reading what was already remembered is `memory.read`, while
//! writing and deleting are `memory.write` and ask a person by default. One
//! tool spanning both would carry the write's gate on every recall.
//!
//! The provider is *bound* to one session at construction, the same way
//! `memory.search` is: a fact written by this run records which session wrote
//! it, and `ToolInvocation` carries no session a model could name.
//!
//! The instant each call judges "is this expired" against comes from the
//! process edge, handed in as a function (C7 #20). The tools read the clock;
//! the decision that depends on it is taken with the instant they were given,
//! and a test can hand them a fixed one.

use std::sync::Arc;

use agent_ports::memory_store::{
    MemoryFact, MemoryOrigin, MemorySensitivity, MemorySource, MemoryStore, Recall, Remember,
};
use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::ids::SessionId;
use agent_protocol::permissions::Capability;
use chrono::{DateTime, Utc};

/// Look up remembered facts.
pub const RECALL_TOOL: &str = "memory.recall";

/// Write one fact down.
pub const REMEMBER_TOOL: &str = "memory.remember";

/// Delete one fact for good.
pub const FORGET_TOOL: &str = "memory.forget";

/// The current instant, supplied by the process edge.
pub type Now = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// How many facts a recall returns when the model names no limit.
const DEFAULT_RECALL_LIMIT: usize = 10;

/// The most facts one call may ask for. Clamped rather than refused, like
/// `memory.search`: the model asked for more than it needs, not for something
/// wrong.
const MAX_RECALL_LIMIT: usize = 50;

/// The durable store, offered to the model through the three fact tools.
pub struct MemoryFactsToolProvider {
    store: Arc<dyn MemoryStore>,
    session: SessionId,
    now: Now,
}

impl MemoryFactsToolProvider {
    pub fn new(store: Arc<dyn MemoryStore>, session: SessionId, now: Now) -> Self {
        Self { store, session, now }
    }

    fn definition(name: &str) -> Option<ToolDefinition> {
        let (description, side_effect, capability, idempotent, schema) = match name {
            RECALL_TOOL => (
                "Look up facts you or the user chose to remember, including ones from earlier \
                 sessions. Use it when the user refers to something from before, or asks what you \
                 remember. Matching is a case-insensitive substring of the fact text; the most \
                 recently updated facts come first. No matches is a normal answer."
                    .to_string(),
                ToolSideEffect::Read,
                Capability::MEMORY_READ,
                true,
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string",
                                   "description": "words to look for in the fact text" },
                        "key": { "type": "string",
                                 "description": "the exact key a fact was stored under" },
                        "limit": { "type": "integer", "minimum": 1 }
                    }
                }),
            ),
            REMEMBER_TOOL => (
                "Remember one fact for future sessions. Call this only when the user explicitly \
                 asks you to remember something — never to record your own observations or \
                 summaries. The fact is written to a store the user can list and delete. With \
                 'key' the store keeps one current answer per key: writing a second fact for a \
                 key that already has one is refused unless 'supersedes' names the fact being \
                 replaced, and the refusal says which id to name. The result gives you the new \
                 fact's id."
                    .to_string(),
                ToolSideEffect::Write,
                Capability::MEMORY_WRITE,
                false,
                serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": { "type": "string",
                                  "description": "the fact, as one self-contained sentence" },
                        "key": { "type": "string",
                                 "description": "a short identity for the fact, e.g. 'indent'" },
                        "supersedes": { "type": "string",
                                        "description": "the id of the fact this one replaces" }
                    }
                }),
            ),
            FORGET_TOOL => (
                "Delete a remembered fact for good. Use it when the user asks you to forget \
                 something; take the id from a memory.recall result or from what the user gives \
                 you. The row is deleted, not hidden. Forgetting a fact that is already gone is \
                 not an error."
                    .to_string(),
                ToolSideEffect::Destructive,
                Capability::MEMORY_WRITE,
                false,
                serde_json::json!({
                    "type": "object",
                    "required": ["id"],
                    "properties": {
                        "id": { "type": "string",
                                "description": "the id memory.recall or memory.remember returned" }
                    }
                }),
            ),
            _ => return None,
        };
        Some(ToolDefinition {
            name: name.to_string(),
            description,
            source: ToolSource::Builtin,
            side_effect,
            capability_requirements: vec![Capability::new(capability)],
            idempotent,
            timeout_ms: Some(10_000),
            input_schema: schema,
        })
    }
}

#[async_trait::async_trait]
impl ToolProvider for MemoryFactsToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        [RECALL_TOOL, REMEMBER_TOOL, FORGET_TOOL].into_iter().filter_map(Self::definition).collect()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        Self::definition(tool_name).map(|definition| definition.side_effect)
    }

    /// No argument names a filesystem path: the facts live in the data
    /// directory's store, and no argument reaches beyond it.
    fn path_arguments(&self, _tool_name: &str) -> Vec<String> {
        Vec::new()
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        match invocation.tool_name.as_str() {
            RECALL_TOOL => self.recall(invocation).await,
            REMEMBER_TOOL => self.remember(invocation).await,
            FORGET_TOOL => self.forget(invocation).await,
            other => Err(ToolProviderError::UnknownTool {
                tool: other.to_string(),
                available: vec![
                    RECALL_TOOL.to_string(),
                    REMEMBER_TOOL.to_string(),
                    FORGET_TOOL.to_string(),
                ],
            }),
        }
    }
}

impl MemoryFactsToolProvider {
    async fn recall(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let query = Recall {
            namespace: None,
            key: optional_string(invocation, "key")?,
            text: optional_string(invocation, "query")?,
            ..Recall::default()
        };
        let query = Recall { limit: limit_argument(invocation)?, ..query };
        let facts = self
            .store
            .recall(&query, (self.now)())
            .await
            .map_err(|error| failed(RECALL_TOOL, &error.to_string()))?;
        Ok(ToolResult::completed(render_recall(&query, &facts)))
    }

    async fn remember(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let text = required_string(invocation, "text")?;
        let request = Remember {
            namespace: None,
            key: optional_string(invocation, "key")?,
            text,
            confidence: None,
            origin: MemoryOrigin::Agent,
            source: Some(MemorySource {
                session: self.session.clone(),
                call: invocation.provider_tool_call_id.clone(),
            }),
            sensitivity: MemorySensitivity::Normal,
            supersedes: optional_string(invocation, "supersedes")?,
            expires_at: None,
        };
        let fact = self
            .store
            .remember(request, (self.now)())
            .await
            .map_err(|error| failed(REMEMBER_TOOL, &error.to_string()))?;
        Ok(ToolResult::completed(format!("remembered {}  {}", fact.id, describe(&fact))))
    }

    async fn forget(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        let id = required_string(invocation, "id")?;
        let forgotten = self
            .store
            .forget(&id)
            .await
            .map_err(|error| failed(FORGET_TOOL, &error.to_string()))?;
        // Forgetting something that is already gone is the state the caller
        // asked for, so it is a result rather than a failure.
        let content =
            if forgotten { format!("forgot {id}") } else { format!("no fact with id '{id}'") };
        Ok(ToolResult::completed(content))
    }
}

/// A required argument that must be present, a string, and not blank.
fn required_string(invocation: &ToolInvocation, name: &str) -> Result<String, ToolProviderError> {
    optional_string(invocation, name)?
        .ok_or_else(|| failed(&invocation.tool_name, &format!("missing '{name}'")))
}

/// An optional string argument. Absent and `null` are both "not given"; a
/// blank one is not, because a model that sent `""` meant something and the
/// store would otherwise have to guess.
fn optional_string(
    invocation: &ToolInvocation,
    name: &str,
) -> Result<Option<String>, ToolProviderError> {
    match invocation.arguments.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => {
            Ok(Some(text.trim().to_string()))
        }
        Some(_) => Err(failed(
            &invocation.tool_name,
            &format!("'{name}' must be a non-empty string when given"),
        )),
    }
}

fn limit_argument(invocation: &ToolInvocation) -> Result<usize, ToolProviderError> {
    match invocation.arguments.get("limit") {
        None | Some(serde_json::Value::Null) => Ok(DEFAULT_RECALL_LIMIT),
        Some(value) => {
            let limit = value.as_u64().filter(|limit| *limit > 0).ok_or_else(|| {
                failed(&invocation.tool_name, "'limit' must be a positive integer")
            })?;
            Ok((limit as usize).min(MAX_RECALL_LIMIT))
        }
    }
}

/// `workspace` alone, or `workspace/key` when the fact has an identity.
fn describe(fact: &MemoryFact) -> String {
    match &fact.key {
        Some(key) => format!("{}/{key}", fact.namespace),
        None => fact.namespace.clone(),
    }
}

/// The model-facing result: one line per fact, newest first.
///
/// The namespace is shown even when it is the default, because "which scope
/// does this answer belong to" is what a later write has to match, and hiding
/// the default would make the scope look like there is none.
fn render_recall(query: &Recall, facts: &[MemoryFact]) -> String {
    if facts.is_empty() {
        return match query.text.as_deref() {
            Some(text) => format!("nothing is remembered matching '{text}'"),
            None => "nothing is remembered yet".to_string(),
        };
    }
    let mut text = format!("{} remembered fact(s) (newest first):", facts.len());
    for fact in facts {
        text.push('\n');
        text.push_str(&format!(
            "{}  {}  {}",
            fact.id,
            describe(fact),
            fact.text.replace('\n', " ")
        ));
    }
    text
}

fn failed(tool: &str, message: &str) -> ToolProviderError {
    ToolProviderError::failed(tool, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::memory_store::{MemoryStatus, MemoryStoreError};
    use chrono::Duration;
    use std::sync::Mutex;

    /// A store that answers from memory and records what it was asked, so the
    /// arguments a tool passed down can be checked as the store saw them.
    #[derive(Default)]
    struct FixedStore {
        facts: Mutex<Vec<MemoryFact>>,
        calls: Mutex<Vec<String>>,
        next_id: Mutex<usize>,
        error: Option<MemoryStoreError>,
    }

    impl FixedStore {
        fn with_facts(facts: Vec<MemoryFact>) -> Self {
            Self { facts: Mutex::new(facts), ..Self::default() }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl MemoryStore for FixedStore {
        async fn remember(
            &self,
            request: Remember,
            now: DateTime<Utc>,
        ) -> Result<MemoryFact, MemoryStoreError> {
            self.calls.lock().unwrap().push(format!(
                "remember {} {} {}",
                request.text,
                request.key.clone().unwrap_or_default(),
                request.source.as_ref().and_then(|s| s.call.clone()).unwrap_or_default()
            ));
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            let fact = MemoryFact {
                id: format!("mem_{next}"),
                namespace: "workspace".to_string(),
                key: request.key,
                text: request.text,
                confidence: 1.0,
                origin: request.origin,
                source: request.source,
                sensitivity: request.sensitivity,
                created_at: now,
                updated_at: now,
                supersedes: request.supersedes,
                expires_at: None,
                status: MemoryStatus::Active,
            };
            self.facts.lock().unwrap().push(fact.clone());
            Ok(fact)
        }

        async fn forget(&self, id: &str) -> Result<bool, MemoryStoreError> {
            self.calls.lock().unwrap().push(format!("forget {id}"));
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            let mut facts = self.facts.lock().unwrap();
            let before = facts.len();
            facts.retain(|fact| fact.id != id);
            Ok(facts.len() != before)
        }

        async fn recall(
            &self,
            query: &Recall,
            _now: DateTime<Utc>,
        ) -> Result<Vec<MemoryFact>, MemoryStoreError> {
            self.calls.lock().unwrap().push(format!(
                "recall {} {} {}",
                query.text.clone().unwrap_or_default(),
                query.key.clone().unwrap_or_default(),
                query.limit
            ));
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok(self.facts.lock().unwrap().iter().take(query.limit).cloned().collect())
        }

        async fn list(
            &self,
            _filter: &agent_ports::memory_store::ListFacts,
            _now: DateTime<Utc>,
        ) -> Result<Vec<MemoryFact>, MemoryStoreError> {
            Ok(self.facts.lock().unwrap().clone())
        }
    }

    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("a fixed instant")
    }

    fn provider(store: Arc<FixedStore>) -> MemoryFactsToolProvider {
        let now: Now = Arc::new(at);
        MemoryFactsToolProvider::new(store, SessionId::from_string("s1"), now)
    }

    fn fact(id: &str, key: Option<&str>, text: &str) -> MemoryFact {
        MemoryFact {
            id: id.to_string(),
            namespace: "workspace".to_string(),
            key: key.map(str::to_string),
            text: text.to_string(),
            confidence: 1.0,
            origin: MemoryOrigin::User,
            source: None,
            sensitivity: MemorySensitivity::Normal,
            created_at: at(),
            updated_at: at() + Duration::seconds(1),
            supersedes: None,
            expires_at: None,
            status: MemoryStatus::Active,
        }
    }

    #[test]
    fn the_surface_is_three_tools_with_the_capabilities_that_gate_them() {
        let provider = provider(Arc::new(FixedStore::default()));
        let definitions = provider.definitions();
        let names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec![RECALL_TOOL, REMEMBER_TOOL, FORGET_TOOL]);

        let of = |name: &str| definitions.iter().find(|d| d.name == name).unwrap().clone();
        assert_eq!(
            of(RECALL_TOOL).capability_requirements,
            vec![Capability::new(Capability::MEMORY_READ)]
        );
        assert_eq!(
            of(REMEMBER_TOOL).capability_requirements,
            vec![Capability::new(Capability::MEMORY_WRITE)]
        );
        assert_eq!(
            of(FORGET_TOOL).capability_requirements,
            vec![Capability::new(Capability::MEMORY_WRITE)]
        );
        assert_eq!(of(REMEMBER_TOOL).side_effect, ToolSideEffect::Write);
        assert_eq!(of(FORGET_TOOL).side_effect, ToolSideEffect::Destructive);
        assert_ne!(
            of(REMEMBER_TOOL).side_effect,
            of(RECALL_TOOL).side_effect,
            "reading what was remembered is not writing"
        );
        assert!(provider.path_arguments(RECALL_TOOL).is_empty());
        assert_eq!(provider.side_effect("fs.read"), None);
    }

    #[test]
    fn the_descriptions_say_what_only_a_person_decided_to_remember() {
        // The one sentence this tool exists behind: the model records what it
        // was asked to, not what it noticed.
        let provider = provider(Arc::new(FixedStore::default()));
        let remember = provider
            .definitions()
            .into_iter()
            .find(|d| d.name == REMEMBER_TOOL)
            .expect("the write tool");
        assert!(
            remember.description.contains("only when the user explicitly"),
            "{}",
            remember.description
        );
    }

    #[tokio::test]
    async fn a_remember_records_the_session_and_the_calling_tool_id() {
        let store = Arc::new(FixedStore::default());
        let provider = provider(store.clone());

        let result = provider
            .invoke(&ToolInvocation::from_tool_call(
                "call-7",
                REMEMBER_TOOL,
                serde_json::json!({"text": "  prefers Chinese  ", "key": "language"}),
            ))
            .await
            .expect("remembered");

        assert_eq!(result.content, "remembered mem_1  workspace/language");
        let facts = store.facts.lock().unwrap();
        assert_eq!(facts[0].text, "prefers Chinese", "the store trims, the tool passes it on");
        assert_eq!(facts[0].origin, MemoryOrigin::Agent);
        assert_eq!(
            facts[0].source,
            Some(MemorySource {
                session: SessionId::from_string("s1"),
                call: Some("call-7".to_string())
            })
        );
    }

    #[tokio::test]
    async fn a_conflict_comes_back_as_a_failure_naming_the_id_to_supersede() {
        let store = Arc::new(FixedStore {
            error: Some(MemoryStoreError::Conflict {
                namespace: "workspace".to_string(),
                key: "language".to_string(),
                id: "mem_old".to_string(),
            }),
            ..FixedStore::default()
        });
        let provider = provider(store);

        let error = provider
            .invoke(&ToolInvocation::proposed(
                REMEMBER_TOOL,
                serde_json::json!({"text": "prefers English", "key": "language"}),
            ))
            .await
            .expect_err("a conflict is the store's answer");

        let ToolProviderError::Failed { tool, message } = error else {
            panic!("a conflict is the tool's own answer");
        };
        assert_eq!(tool, REMEMBER_TOOL);
        assert!(message.contains("mem_old"), "the model needs the id to retry with: {message}");
        assert!(message.contains("supersedes"), "{message}");
    }

    #[tokio::test]
    async fn a_recall_passes_its_filters_down_and_renders_newest_first() {
        let store =
            Arc::new(FixedStore::with_facts(vec![fact("mem_2", Some("indent"), "uses spaces")]));
        let provider = provider(store.clone());

        let result = provider
            .invoke(&ToolInvocation::proposed(
                RECALL_TOOL,
                serde_json::json!({"query": "spaces", "key": "indent", "limit": 5}),
            ))
            .await
            .expect("recall");

        assert_eq!(
            result.content,
            "1 remembered fact(s) (newest first):\nmem_2  workspace/indent  uses spaces"
        );
        assert_eq!(store.calls(), vec!["recall spaces indent 5".to_string()]);
    }

    #[tokio::test]
    async fn a_recall_with_no_matches_says_so_and_a_limit_is_clamped() {
        let store = Arc::new(FixedStore::default());
        let provider = provider(store.clone());

        let result = provider
            .invoke(&ToolInvocation::proposed(
                RECALL_TOOL,
                serde_json::json!({"query": "zeppelin"}),
            ))
            .await
            .expect("an empty recall is an answer");
        assert_eq!(result.content, "nothing is remembered matching 'zeppelin'");

        provider
            .invoke(&ToolInvocation::proposed(RECALL_TOOL, serde_json::json!({"limit": 1000})))
            .await
            .expect("clamped");
        assert!(store.calls()[1].ends_with(&format!(" {MAX_RECALL_LIMIT}")), "{:?}", store.calls());
    }

    #[tokio::test]
    async fn forgetting_a_fact_that_is_not_there_is_an_answer_not_a_failure() {
        let store = Arc::new(FixedStore::default());
        let provider = provider(store);

        let result = provider
            .invoke(&ToolInvocation::proposed(FORGET_TOOL, serde_json::json!({"id": "mem_gone"})))
            .await
            .expect("the state the caller asked for already holds");
        assert_eq!(result.content, "no fact with id 'mem_gone'");
    }

    #[tokio::test]
    async fn a_missing_or_blank_argument_is_refused_before_the_store_is_asked() {
        let store = Arc::new(FixedStore::default());
        let provider = provider(store.clone());

        for (tool, arguments) in [
            (REMEMBER_TOOL, serde_json::json!({})),
            (REMEMBER_TOOL, serde_json::json!({"text": "   "})),
            (FORGET_TOOL, serde_json::json!({"id": 7})),
            (RECALL_TOOL, serde_json::json!({"limit": 0})),
            (RECALL_TOOL, serde_json::json!({"query": ""})),
        ] {
            provider
                .invoke(&ToolInvocation::proposed(tool, arguments))
                .await
                .expect_err("a bad argument is refused");
        }
        assert!(store.calls().is_empty(), "nothing reached the store: {:?}", store.calls());
    }

    #[tokio::test]
    async fn another_tool_name_is_unknown_here() {
        let provider = provider(Arc::new(FixedStore::default()));
        let error = provider
            .invoke(&ToolInvocation::proposed("memory.search", serde_json::json!({})))
            .await
            .expect_err("not this provider's tool");
        assert!(matches!(
            error,
            ToolProviderError::UnknownTool { tool, available }
                if tool == "memory.search" && available.len() == 3
        ));
    }
}
