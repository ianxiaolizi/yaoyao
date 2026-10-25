//! The loopback surface: what needs the token, what the read endpoints answer,
//! and what a wrong host, path or session gets.
//!
//! The client here is the smallest thing that can speak HTTP/1.1 over a socket.
//! A TLS-capable client would pull a native crypto build into a crate whose
//! subject is a plaintext loopback listener, and the test would then be about
//! the machine's C toolchain.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_kernel::inbox::Inbox;
use agent_kernel::model_selection::ModelChoice;
use agent_ports::event_store::EventStore;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{
    ApprovalId, EventId, EventSeq, InboxItemId, SchemaVersion, SessionId, StepId, TurnId,
};
use agent_protocol::inbox::{InboxItem, InboxItemStatus, InboxKind};
use agent_protocol::model::ModelResponse;
use agent_protocol::permissions::{ApprovalRequest, ApprovalStatus, Capability};
use agent_protocol::reason::{Basis, ReasonKind, ReasonNode, ReasonSource};
use agent_store_jsonl::JsonlEventStore;
use agent_web::{Backend, BackendError, Config, Decision, LiveChunk, LiveSink, Meta, Server};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

fn meta() -> Meta {
    Meta {
        workspace: "yaoyao-public".to_string(),
        model: "deepseek-flash".to_string(),
        sandbox: "local".to_string(),
        judgment: "gates".to_string(),
        budget: Some(32_000),
        capabilities: vec!["meta".to_string()],
        models: vec![agent_web::ModelOption {
            provider: "echo".to_string(),
            model: None,
            label: "echo".to_string(),
        }],
    }
}

/// An empty data directory with a store over it.
fn empty() -> (Arc<dyn EventStore>, TempDir) {
    let dir = tempfile::tempdir().expect("a temporary data directory");
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(dir.path()));
    (store, dir)
}

/// A backend that records what the HTTP layer asked it to do.
///
/// The point of these tests is the surface, not the kernel: whether a turn
/// actually runs is the binary's business and is covered by its own acceptance
/// suites. What this double proves is that a click reaches exactly one verb,
/// with the decision and text the page sent, and that a second click does not.
#[derive(Default)]
struct FakeBackend {
    turns: Mutex<Vec<(String, String)>>,
    decisions: Mutex<Vec<(String, String, Decision)>>,
    steers: Mutex<Vec<(String, String)>>,
    interrupts: Mutex<Vec<String>>,
    /// Every `set_model` call, as `(session, provider, model)`.
    models: Mutex<Vec<(String, String, Option<String>)>>,
    resumes: Mutex<Vec<String>>,
    /// Every `delete_session` call, by id. The fake leaves the log alone so a
    /// test can tell a refused request from one that went through.
    deleted: Mutex<Vec<String>>,
    /// Whether `delete_session` is accepted. `false` stands for a turn of this
    /// server's own still running on the session.
    accept_delete: AtomicBool,
    forks: Mutex<Vec<(String, Option<String>, String)>>,
    hold_approval: AtomicBool,
    /// Whether `set_model` accepts the name it was given. `true` unless a test
    /// makes the provider unknown.
    accept_model: AtomicBool,
    /// What `interrupt` reports back: `true` unless a test says there is no
    /// open turn to stop.
    interrupt_queued: AtomicBool,
    /// The live previews `run_turn` publishes while it runs, in order.
    previews: Mutex<Vec<LiveChunk>>,
    release: Notify,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        let backend = Arc::new(Self::default());
        backend.interrupt_queued.store(true, Ordering::SeqCst);
        backend.accept_model.store(true, Ordering::SeqCst);
        backend.accept_delete.store(true, Ordering::SeqCst);
        backend
    }

    /// Refuse a delete the way the driver does while a turn is running.
    fn refuse_delete(&self) {
        self.accept_delete.store(false, Ordering::SeqCst);
    }

    fn turn_count(&self) -> usize {
        self.turns.lock().unwrap().len()
    }

    fn decision_count(&self) -> usize {
        self.decisions.lock().unwrap().len()
    }

    fn hold_approvals(&self, hold: bool) {
        self.hold_approval.store(hold, Ordering::SeqCst);
    }

    fn interrupt_queued(&self, queued: bool) {
        self.interrupt_queued.store(queued, Ordering::SeqCst);
    }

    /// Refuse every model name, the way the binary refuses one it cannot build.
    fn refuse_models(&self) {
        self.accept_model.store(false, Ordering::SeqCst);
    }

    /// What the next turn will publish as live text, before it finishes.
    fn previews(&self, chunks: Vec<LiveChunk>) {
        self.previews.lock().unwrap().extend(chunks);
    }
}

#[async_trait::async_trait]
impl Backend for FakeBackend {
    async fn run_turn(
        &self,
        session: &SessionId,
        text: &str,
        live: &LiveSink,
    ) -> Result<(), BackendError> {
        self.turns.lock().unwrap().push((session.as_str().to_string(), text.to_string()));
        for chunk in self.previews.lock().unwrap().drain(..) {
            let _ = live.send(chunk);
        }
        Ok(())
    }

    async fn settle_approval(
        &self,
        session: &SessionId,
        approval_id: &str,
        decision: Decision,
        _live: &LiveSink,
    ) -> Result<(), BackendError> {
        self.decisions.lock().unwrap().push((
            session.as_str().to_string(),
            approval_id.to_string(),
            decision,
        ));
        if self.hold_approval.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        Ok(())
    }

    async fn resume(&self, session: &SessionId, _live: &LiveSink) -> Result<(), BackendError> {
        self.resumes.lock().unwrap().push(session.as_str().to_string());
        Ok(())
    }

    async fn fork(
        &self,
        session: &SessionId,
        at: Option<&str>,
        reason: &str,
    ) -> Result<Option<SessionId>, BackendError> {
        self.forks.lock().unwrap().push((
            session.as_str().to_string(),
            at.map(str::to_string),
            reason.to_string(),
        ));
        Ok(Some(SessionId::from_string("child")))
    }

    async fn steer(&self, session: &SessionId, text: &str) -> Result<(), BackendError> {
        self.steers.lock().unwrap().push((session.as_str().to_string(), text.to_string()));
        Ok(())
    }

    async fn interrupt(&self, session: &SessionId) -> Result<bool, BackendError> {
        self.interrupts.lock().unwrap().push(session.as_str().to_string());
        Ok(self.interrupt_queued.load(Ordering::SeqCst))
    }

    async fn set_model(
        &self,
        session: &SessionId,
        provider: &str,
        model: Option<&str>,
    ) -> Result<ModelChoice, BackendError> {
        if !self.accept_model.load(Ordering::SeqCst) {
            return Err(BackendError::new(
                "invalid_model",
                "unknown provider 'nope': use 'echo' or 'openai'",
            ));
        }
        self.models.lock().unwrap().push((
            session.as_str().to_string(),
            provider.to_string(),
            model.map(str::to_string),
        ));
        Ok(ModelChoice::new(provider, model.map(str::to_string)))
    }

    async fn delete_session(&self, session: &SessionId) -> Result<(), BackendError> {
        if !self.accept_delete.load(Ordering::SeqCst) {
            return Err(BackendError::busy("a turn is running in this session"));
        }
        self.deleted.lock().unwrap().push(session.as_str().to_string());
        Ok(())
    }
}

/// One event, with the schema this build writes.
fn draft(
    session: &SessionId,
    event_type: &str,
    turn: Option<&TurnId>,
    step: Option<&StepId>,
    payload: Value,
    actor: Actor,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session.clone(),
        run_id: None,
        turn_id: turn.cloned(),
        step_id: step.cloned(),
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: json!({}),
        ignorable: false,
    }
}

/// A session with a prompt, a step, an answer and an end — enough for both read
/// endpoints to have something true to say.
async fn seeded() -> (Arc<dyn EventStore>, TempDir, SessionId) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let item = InboxItem {
        item_id: InboxItemId::from_string("i1"),
        session_id: session.clone(),
        kind: InboxKind::NextTurn,
        payload: json!({ "text": "hola" }),
        priority: 0,
        created_at: Utc::now(),
        status: InboxItemStatus::Pending,
    };
    let drafts = vec![
        draft(
            &session,
            EventType::INBOX_APPENDED,
            None,
            None,
            serde_json::to_value(&item).unwrap(),
            Actor::User,
        ),
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
        ),
        draft(&session, EventType::STEP_STARTED, Some(&turn), Some(&step), json!({}), Actor::Agent),
        draft(
            &session,
            EventType::MODEL_RESPONSE_COMPLETED,
            Some(&turn),
            Some(&step),
            json!({ "attempt_id": "a1", "response": ModelResponse::text("hello") }),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::TURN_COMPLETED,
            Some(&turn),
            None,
            json!({ "final_content": "hello" }),
            Actor::Scheduler,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the log");
    (store, dir, session)
}

/// A session with an open turn and no terminal event: the write endpoints must
/// refuse a second turn rather than queue one behind it.
async fn seeded_open_turn() -> (Arc<dyn EventStore>, TempDir) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let item = InboxItem {
        item_id: InboxItemId::from_string("i1"),
        session_id: session.clone(),
        kind: InboxKind::NextTurn,
        payload: json!({ "text": "keep going" }),
        priority: 0,
        created_at: Utc::now(),
        status: InboxItemStatus::Pending,
    };
    let drafts = vec![
        draft(
            &session,
            EventType::INBOX_APPENDED,
            None,
            None,
            serde_json::to_value(&item).unwrap(),
            Actor::User,
        ),
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
        ),
        draft(&session, EventType::STEP_STARTED, Some(&turn), Some(&step), json!({}), Actor::Agent),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the open turn");
    (store, dir)
}

/// A session paused on one approval request.
async fn seeded_approval() -> (Arc<dyn EventStore>, TempDir) {
    seeded_approval_at(Utc::now()).await
}

/// A session paused on one approval request made at `created_at`.
async fn seeded_approval_at(created_at: DateTime<Utc>) -> (Arc<dyn EventStore>, TempDir) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let item = InboxItem {
        item_id: InboxItemId::from_string("i1"),
        session_id: session.clone(),
        kind: InboxKind::NextTurn,
        payload: json!({ "text": "run the tests" }),
        priority: 0,
        created_at: Utc::now(),
        status: InboxItemStatus::Pending,
    };
    let request = ApprovalRequest {
        approval_id: ApprovalId::from_string("ap-1"),
        session_id: session.clone(),
        capability: Capability::new(Capability::PROCESS_SPAWN),
        tool_name: "shell.run".to_string(),
        args_summary: "cargo test -p agent".to_string(),
        status: ApprovalStatus::Pending,
        created_at,
    };
    let drafts = vec![
        draft(
            &session,
            EventType::INBOX_APPENDED,
            None,
            None,
            serde_json::to_value(&item).unwrap(),
            Actor::User,
        ),
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
        ),
        draft(&session, EventType::STEP_STARTED, Some(&turn), Some(&step), json!({}), Actor::Agent),
        draft(
            &session,
            EventType::APPROVAL_REQUESTED,
            Some(&turn),
            Some(&step),
            serde_json::to_value(&request).unwrap(),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::TURN_YIELDED,
            Some(&turn),
            None,
            json!({ "approval_id": "ap-1" }),
            Actor::Scheduler,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the approval");
    (store, dir)
}

/// A session whose log holds one completed `git.diff` observation.
async fn seeded_diff() -> (Arc<dyn EventStore>, TempDir) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let item = InboxItem {
        item_id: InboxItemId::from_string("i1"),
        session_id: session.clone(),
        kind: InboxKind::NextTurn,
        payload: json!({ "text": "show the diff" }),
        priority: 0,
        created_at: Utc::now(),
        status: InboxItemStatus::Pending,
    };
    let diff = json!({
        "files": [{
            "file": "src/lib.rs",
            "patch": "@@ -1,1 +1,2 @@\n-old\n+new\n+more",
            "additions": 2,
            "deletions": 1,
            "status": "modified"
        }],
        "file_count": 1,
        "additions": 2,
        "deletions": 1,
    });
    let drafts = vec![
        draft(
            &session,
            EventType::INBOX_APPENDED,
            None,
            None,
            serde_json::to_value(&item).unwrap(),
            Actor::User,
        ),
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
        ),
        draft(&session, EventType::STEP_STARTED, Some(&turn), Some(&step), json!({}), Actor::Agent),
        draft(
            &session,
            EventType::TOOL_STARTED,
            Some(&turn),
            Some(&step),
            json!({
                "tool_name": "git.diff",
                "provider_tool_call_id": "c1",
                "arguments": { "staged": false },
            }),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::TOOL_COMPLETED,
            Some(&turn),
            Some(&step),
            json!({
                "tool_name": "git.diff",
                "provider_tool_call_id": "c1",
                "content": diff.to_string(),
            }),
            Actor::Agent,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the diff");
    (store, dir)
}

/// A session whose last model request recorded a context breakdown.
async fn seeded_context() -> (Arc<dyn EventStore>, TempDir) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let item = InboxItem {
        item_id: InboxItemId::from_string("i1"),
        session_id: session.clone(),
        kind: InboxKind::NextTurn,
        payload: json!({ "text": "what is in context" }),
        priority: 0,
        created_at: Utc::now(),
        status: InboxItemStatus::Pending,
    };
    let payload = json!({
        "estimated_context_tokens": 100,
        "estimated_messages_tokens": 150,
        "estimated_tool_tokens": 20,
        "estimated_request_tokens": 170,
        "max_input_tokens": 8000,
        "reserved_output_tokens": 2000,
        "message_count": 10,
        "context_message_count": 8,
        "dropped_message_count": 2,
        "tool_count": 7,
        "tool_manifest_id": "manifest-1",
        "context_layers": {
            "state": { "message_uid": "msg/state/t1", "text": "the state layer" },
            "task": { "message_uid": "msg/task/t1", "text": "the task layer" },
        },
        "provider_usage": {
            "input_tokens": 170,
            "output_tokens": 20,
            "total_tokens": 190,
            "cache_hit_tokens": 80,
            "reasoning_tokens": 5,
        },
    });
    let drafts = vec![
        draft(
            &session,
            EventType::INBOX_APPENDED,
            None,
            None,
            serde_json::to_value(&item).unwrap(),
            Actor::User,
        ),
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
        ),
        draft(&session, EventType::STEP_STARTED, Some(&turn), Some(&step), json!({}), Actor::Agent),
        draft(
            &session,
            EventType::MODEL_REQUEST_PREPARED,
            Some(&turn),
            Some(&step),
            payload,
            Actor::Agent,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the context");
    (store, dir)
}

/// A session whose log holds one plan ledger.
async fn seeded_plan() -> (Arc<dyn EventStore>, TempDir) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let item = InboxItem {
        item_id: InboxItemId::from_string("i1"),
        session_id: session.clone(),
        kind: InboxKind::NextTurn,
        payload: json!({ "text": "make a plan" }),
        priority: 0,
        created_at: Utc::now(),
        status: InboxItemStatus::Pending,
    };
    let drafts = vec![
        draft(
            &session,
            EventType::INBOX_APPENDED,
            None,
            None,
            serde_json::to_value(&item).unwrap(),
            Actor::User,
        ),
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
        ),
        draft(&session, EventType::STEP_STARTED, Some(&turn), Some(&step), json!({}), Actor::Agent),
        draft(
            &session,
            EventType::PLAN_UPDATED,
            Some(&turn),
            Some(&step),
            json!({
                "provider_tool_call_id": "p1",
                "plan": {
                    "goal": "finish the GUI",
                    "items": [
                        { "text": "context panel", "status": "done" },
                        { "text": "plan panel", "status": "doing" }
                    ],
                    "facts": ["the log is the source"]
                }
            }),
            Actor::Agent,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the plan");
    (store, dir)
}

/// A session whose log holds one delegated child.
async fn seeded_subagent() -> (Arc<dyn EventStore>, TempDir) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let item = InboxItem {
        item_id: InboxItemId::from_string("i1"),
        session_id: session.clone(),
        kind: InboxKind::NextTurn,
        payload: json!({ "text": "delegate" }),
        priority: 0,
        created_at: Utc::now(),
        status: InboxItemStatus::Pending,
    };
    let drafts = vec![
        draft(
            &session,
            EventType::INBOX_APPENDED,
            None,
            None,
            serde_json::to_value(&item).unwrap(),
            Actor::User,
        ),
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": item.item_id }),
            Actor::Scheduler,
        ),
        draft(&session, EventType::STEP_STARTED, Some(&turn), Some(&step), json!({}), Actor::Agent),
        draft(
            &session,
            EventType::SUBAGENT_SPAWNED,
            Some(&turn),
            Some(&step),
            json!({
                "child_session_id": "child-1",
                "provider": "mock",
                "parent_turn_id": "t1",
                "fork_at_turn_id": "t1",
                "request": { "objective": "do the separable work" }
            }),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::SUBAGENT_COMPLETED,
            Some(&turn),
            Some(&step),
            json!({
                "child_session_id": "child-1",
                "provider": "mock",
                "parent_turn_id": "t1",
                "outcome": { "status": "completed", "answer": "done" }
            }),
            Actor::System,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the subagent");
    (store, dir)
}

/// A session whose log holds a two-node reasoning chain.
async fn seeded_chain() -> (Arc<dyn EventStore>, TempDir) {
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let situation = ReasonNode {
        id: "r/t1/1".to_string(),
        turn_id: "t1".to_string(),
        step: 0,
        kind: ReasonKind::Situation,
        claim: "the person asked a question".to_string(),
        basis: vec![Basis { node: None, event: None, fact: Some("their words".to_string()) }],
        rule: "situation/derive".to_string(),
        source: ReasonSource::Kernel,
    };
    let verdict = ReasonNode {
        id: "r/t1/2".to_string(),
        turn_id: "t1".to_string(),
        step: 0,
        kind: ReasonKind::Verdict,
        claim: "answered".to_string(),
        basis: vec![Basis { node: Some("r/t1/1".to_string()), event: None, fact: None }],
        rule: "verdict/answer".to_string(),
        source: ReasonSource::Kernel,
    };
    let drafts = vec![
        draft(
            &session,
            EventType::REASON_RECORDED,
            Some(&turn),
            None,
            json!({ "node": situation }),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::REASON_RECORDED,
            Some(&turn),
            None,
            json!({ "node": verdict }),
            Actor::Agent,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the chain");
    (store, dir)
}

/// Bind on an ephemeral port and start serving.
async fn serve_over(
    store: Arc<dyn EventStore>,
    data_dir: PathBuf,
) -> (SocketAddr, String, tokio::task::JoinHandle<()>) {
    serve_over_with(store, data_dir, None).await
}

/// Bind with a driver, for the endpoints that do work.
async fn serve_over_with(
    store: Arc<dyn EventStore>,
    data_dir: PathBuf,
    backend: Option<Arc<dyn Backend>>,
) -> (SocketAddr, String, tokio::task::JoinHandle<()>) {
    let server =
        Server::bind(Config { port: 0, meta: meta(), store, data_dir, backend, now: Utc::now })
            .await
            .expect("bind loopback");
    let addr = server.addr();
    let token = server.token().to_string();
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    (addr, token, handle)
}

/// A server over an empty data directory. The directory is returned so the
/// caller keeps it alive for the length of the test.
async fn serve() -> (SocketAddr, String, tokio::task::JoinHandle<()>, TempDir) {
    let (store, dir) = empty();
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;
    (addr, token, handle, dir)
}

/// One request, one connection, the response read to EOF.
async fn request(
    addr: SocketAddr,
    path: &str,
    token: Option<&str>,
    host: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let auth = token.map(|token| format!("Authorization: Bearer {token}\r\n")).unwrap_or_default();
    let host = host.map(|host| host.to_string()).unwrap_or_else(|| addr.to_string());
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.expect("read");
    let text = String::from_utf8_lossy(&bytes).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    (status, text)
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or(response)
}

/// One DELETE with no body, read to EOF.
async fn delete(addr: SocketAddr, path: &str, token: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!(
        "DELETE {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.expect("read");
    let text = String::from_utf8_lossy(&bytes).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    (status, text)
}

/// One POST with a JSON body, read to EOF.
async fn post(addr: SocketAddr, path: &str, token: &str, payload: &Value) -> (u16, String) {
    post_raw(addr, path, token, Some("application/json"), &payload.to_string()).await
}

/// One POST whose body is written out by the caller.
///
/// A body that is not JSON cannot be built from a [`Value`], and a request the
/// surface has to refuse for its shape is exactly what these tests need.
async fn post_raw(
    addr: SocketAddr,
    path: &str,
    token: &str,
    content_type: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let content_type =
        content_type.map(|value| format!("Content-Type: {value}\r\n")).unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         {content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.expect("read");
    let text = String::from_utf8_lossy(&bytes).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    (status, text)
}

/// Read the beginning of an SSE response, enough to see the replay.
///
/// An SSE response stays open, so this reads until the first item rather than to
/// EOF, then drops the socket. That is what a browser does when it closes a tab.
async fn read_sse(addr: SocketAddr, path: &str, token: Option<&str>) -> (u16, String) {
    use tokio::time::{Duration, timeout};

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let auth = token.map(|token| format!("Authorization: Bearer {token}\r\n")).unwrap_or_default();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Accept: text/event-stream\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut bytes = Vec::new();
    let _ = timeout(Duration::from_secs(3), async {
        loop {
            let mut buffer = [0u8; 1024];
            let read = stream.read(&mut buffer).await.expect("read");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&bytes);
            if text.contains("event: item") && text.contains("\n\n") {
                break;
            }
        }
    })
    .await;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"));
    (status, text)
}

#[tokio::test]
async fn meta_requires_the_process_token() {
    let (addr, token, handle, _dir) = serve().await;

    let (status, response) = request(addr, "/api/meta", None, None).await;
    assert_eq!(status, 401, "{response}");
    assert!(body(&response).contains("\"code\":\"unauthorized\""), "{response}");

    let (status, response) = request(addr, "/api/meta", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload = body(&response);
    assert!(payload.contains("\"v\":1"), "{payload}");
    assert!(payload.contains("\"workspace\":\"yaoyao-public\""), "{payload}");
    handle.abort();
}

#[tokio::test]
async fn the_page_is_public_and_names_the_product() {
    let (addr, _, handle, _dir) = serve().await;
    let (status, response) = request(addr, "/", None, None).await;
    assert_eq!(status, 200, "{response}");
    let page = body(&response);
    assert!(page.contains("爻爻"), "the page names the product");
    handle.abort();
}

#[tokio::test]
async fn the_page_assets_are_embedded_and_served() {
    let (addr, _, handle, _dir) = serve().await;
    let (status, script) = request(addr, "/app.js", None, None).await;
    assert_eq!(status, 200, "{script}");
    assert!(body(&script).contains("renderTimeline"), "the page script is embedded");
    let (status, style) = request(addr, "/app.css", None, None).await;
    assert_eq!(status, 200, "{style}");
    assert!(body(&style).contains(".window"), "the page stylesheet is embedded");
    handle.abort();
}

#[tokio::test]
async fn a_foreign_host_is_refused_before_the_token_is_read() {
    let (addr, token, handle, _dir) = serve().await;
    let (status, response) = request(addr, "/api/meta", Some(&token), Some("example.com")).await;
    assert_eq!(status, 403, "{response}");
    assert!(body(&response).contains("\"code\":\"host_not_allowed\""), "{response}");
    handle.abort();
}

#[tokio::test]
async fn an_unknown_api_path_is_not_found() {
    let (addr, token, handle, _dir) = serve().await;
    let (status, _) = request(addr, "/api/nope", Some(&token), None).await;
    assert_eq!(status, 404);
    handle.abort();
}

#[tokio::test]
async fn the_session_list_names_what_happened() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload = body(&response);
    assert!(payload.contains("\"id\":\"s1\""), "{payload}");
    assert!(payload.contains("\"title\":\"hola\""), "the first prompt is the title: {payload}");
    assert!(payload.contains("\"snippet\":\"hello\""), "the last answer is the snippet: {payload}");
    assert!(payload.contains("\"steps\":1"), "one step was opened: {payload}");
    handle.abort();
}

#[tokio::test]
async fn the_transcript_is_the_turn_in_order() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/transcript", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["v"], 1);
    assert_eq!(payload["head"], 4, "four events were folded");
    let items = payload["items"].as_array().expect("items");
    let kinds: Vec<&str> = items.iter().filter_map(|item| item["kind"].as_str()).collect();
    assert_eq!(kinds, ["user", "answer", "turn_end"], "{items:?}");
    assert_eq!(items[0]["text"], "hola");
    assert_eq!(items[1]["text"], "hello");
    assert_eq!(items[2]["ending"], "answered");
    handle.abort();
}

#[tokio::test]
async fn since_returns_what_changed_after_the_cursor() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) =
        request(addr, "/api/sessions/s1/transcript?since=3", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    let items = payload["items"].as_array().expect("items");
    // The draft answer was written at 3 and replaced by the turn's final content
    // at 4, so it *changed* after 3 — an upsert reader needs it again.
    let kinds: Vec<&str> = items.iter().filter_map(|item| item["kind"].as_str()).collect();
    assert_eq!(kinds, ["answer", "turn_end"], "{items:?}");
    assert!(!kinds.contains(&"user"), "the prompt was not touched after 3");
    handle.abort();
}

#[tokio::test]
async fn a_cached_fold_is_invalidated_by_an_append() {
    let (store, dir, session) = seeded().await;
    let (addr, token, handle) = serve_over(store.clone(), dir.path().to_path_buf()).await;

    let (status, first) = request(addr, "/api/sessions/s1/transcript", Some(&token), None).await;
    assert_eq!(status, 200, "{first}");
    let first: Value = serde_json::from_str(body(&first)).expect("a JSON body");
    assert_eq!(first["head"], 4);

    // A core event changes the head without changing a readable row. If the
    // cached fold were reused, the second request would still say 4.
    store
        .append(
            &session,
            EventSeq::new(5),
            vec![draft(
                &session,
                EventType::MODEL_REQUEST_PREPARED,
                None,
                None,
                json!({}),
                Actor::Agent,
            )],
        )
        .await
        .expect("append a core event");

    let (status, second) = request(addr, "/api/sessions/s1/transcript", Some(&token), None).await;
    assert_eq!(status, 200, "{second}");
    let second: Value = serde_json::from_str(body(&second)).expect("a JSON body");
    assert_eq!(second["head"], 5, "the cache must follow the file's signature");
    handle.abort();
}

#[tokio::test]
async fn a_session_can_be_exported_as_markdown_or_json() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) =
        request(addr, "/api/sessions/s1/export?format=md", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    assert!(response.contains("content-type: text/markdown"), "{response}");
    assert!(response.contains("content-disposition: attachment"), "{response}");
    assert!(body(&response).contains("hola"), "the prompt is in the export: {response}");

    let (status, response) =
        request(addr, "/api/sessions/s1/export?format=json", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    assert!(response.contains("content-type: application/json"), "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON export");
    assert!(payload.get("schema_version").is_some(), "{payload}");
    handle.abort();
}

#[tokio::test]
async fn an_artifact_is_served_only_by_its_content_hash() {
    let (store, dir) = empty();
    let artifacts = dir.path().join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("artifact directory");
    let hash = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    std::fs::write(artifacts.join(format!("{hash}.blob")), b"hello").expect("blob");

    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;
    let (status, response) =
        request(addr, &format!("/api/artifacts/{hash}"), Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    // A tab cannot render an opaque stream: text is served as text so clicking
    // an artifact shows the bytes instead of opening a blank page.
    assert!(response.contains("content-type: text/plain; charset=utf-8"), "{response}");
    assert_eq!(body(&response), "hello");

    let locator = format!("/api/artifacts/artifact%3A%2F%2Fsha256%2F{hash}");
    let (status, response) = request(addr, &locator, Some(&token), None).await;
    assert_eq!(status, 200, "a scheme-qualified locator is accepted: {response}");

    let (status, response) = request(addr, "/api/artifacts/not-a-hash", Some(&token), None).await;
    assert_eq!(status, 400, "{response}");

    // Bytes that are not text stay opaque: a false "text" label would garble
    // them in the tab rather than hand them over as a file.
    let binary_hash = "b02a591131217cb579165aeccf0d94569acffb9934c84d6c813d77e3abedd233";
    std::fs::write(artifacts.join(format!("{binary_hash}.blob")), [0x00u8, 0x9f, 0x92, 0x96])
        .expect("binary blob");
    let (status, response) =
        request(addr, &format!("/api/artifacts/{binary_hash}"), Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    assert!(response.contains("content-type: application/octet-stream"), "{response}");
    handle.abort();
}

#[tokio::test]
async fn a_scheduled_turn_can_be_listed_created_and_cancelled() {
    let (store, dir, session) = seeded().await;
    let (addr, token, handle) = serve_over(store.clone(), dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/schedules", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let initial: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(initial["items"].as_array().expect("items").len(), 0);

    let (status, response) = post(
        addr,
        "/api/sessions/s1/schedules",
        &token,
        &json!({ "v": 1, "when": "+10m", "text": "later" }),
    )
    .await;
    assert_eq!(status, 200, "{response}");
    let created: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    let item_id = created["item_id"].as_str().expect("an item id").to_string();

    let (status, response) = request(addr, "/api/sessions/s1/schedules", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let listed: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(listed["items"][0]["text"], "later");

    let (status, response) =
        delete(addr, &format!("/api/sessions/s1/schedules/{item_id}"), &token).await;
    assert_eq!(status, 200, "{response}");
    let inbox = Inbox::new(store);
    assert!(inbox.scheduled(&session).await.expect("read inbox").is_empty());
    handle.abort();
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    let (addr, token, handle, _dir) = serve().await;
    let (status, response) =
        request(addr, "/api/sessions/ghost/transcript", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    handle.abort();
}

#[tokio::test]
async fn the_why_report_is_the_chain() {
    let (store, dir) = seeded_chain().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/why", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["turn"], "t1");
    assert_eq!(payload["check"], "ok");
    assert_eq!(payload["verdict"]["rule"], "verdict/answer");
    assert_eq!(payload["nodes"].as_array().expect("nodes").len(), 2);
    handle.abort();
}

#[tokio::test]
async fn the_diff_report_is_the_observation_the_agent_saw() {
    let (store, dir) = seeded_diff().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/diff", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["source"], "git.diff");
    assert_eq!(payload["diff"]["file_count"], 1);
    assert_eq!(payload["diff"]["files"][0]["file"], "src/lib.rs");
    assert_eq!(payload["diff"]["files"][0]["additions"], 2);
    handle.abort();
}

#[tokio::test]
async fn the_context_report_is_the_last_prepared_request() {
    let (store, dir) = seeded_context().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/context", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["used"], 170);
    assert_eq!(payload["budget"]["max_input_tokens"], 8000);
    assert_eq!(payload["messages"]["dropped"], 2);
    assert_eq!(payload["tools"]["count"], 7);
    let categories = payload["categories"].as_array().expect("categories");
    assert_eq!(categories.len(), 3, "{categories:?}");
    assert_eq!(categories[0]["name"], "历史");
    assert_eq!(categories[0]["tokens"], 100);
    assert_eq!(payload["usage"]["cache_hit_tokens"], 80);
    handle.abort();
}

#[tokio::test]
async fn the_plan_report_is_the_latest_plan_update() {
    let (store, dir) = seeded_plan().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/plan", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["source"], "plan/updated");
    assert_eq!(payload["plan"]["goal"], "finish the GUI");
    assert_eq!(payload["plan"]["items"][1]["status"], "doing");
    handle.abort();
}

#[tokio::test]
async fn the_subagent_report_tracks_the_child_status() {
    let (store, dir) = seeded_subagent().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/subagents", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["children"][0]["child_session_id"], "child-1");
    assert_eq!(payload["children"][0]["objective"], "do the separable work");
    assert_eq!(payload["children"][0]["status"], "completed");
    handle.abort();
}

#[tokio::test]
async fn a_session_without_subagents_is_not_found() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/subagents", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    assert!(body(&response).contains("\"code\":\"subagents_unavailable\""), "{response}");
    handle.abort();
}

#[tokio::test]
async fn a_session_without_a_plan_is_not_found() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/plan", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    assert!(body(&response).contains("\"code\":\"plan_unavailable\""), "{response}");
    handle.abort();
}

#[tokio::test]
async fn a_session_without_a_prepared_request_has_no_context_report() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/context", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    assert!(body(&response).contains("\"code\":\"context_unavailable\""), "{response}");
    handle.abort();
}

#[tokio::test]
async fn a_session_without_a_diff_observation_is_not_found() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/diff", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    assert!(body(&response).contains("\"code\":\"diff_unavailable\""), "{response}");
    handle.abort();
}

#[tokio::test]
async fn a_session_without_a_chain_is_not_found() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/why", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    assert!(body(&response).contains("\"code\":\"why_unavailable\""), "{response}");
    handle.abort();
}

/// Poll a synchronous assertion for a background task's effect.
async fn eventually(mut check: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_turn_is_accepted_and_reaches_the_driver() {
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/turns", &token, &json!({ "v": 1, "text": "next" })).await;
    assert_eq!(status, 202, "{response}");
    assert!(body(&response).contains("\"accepted\":true"), "{response}");
    assert!(eventually(|| backend.turn_count() == 1).await, "the driver never saw the turn");
    let turns = backend.turns.lock().unwrap();
    assert_eq!(turns[0], ("s1".to_string(), "next".to_string()));
    drop(turns);
    handle.abort();
}

#[tokio::test]
async fn a_new_session_can_receive_its_first_turn() {
    let (store, dir) = empty();
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/fresh/turns", &token, &json!({ "v": 1, "text": "hello" })).await;
    assert_eq!(status, 202, "the log is created by the turn: {response}");
    assert!(eventually(|| backend.turn_count() == 1).await);
    handle.abort();
}

#[tokio::test]
async fn a_newer_protocol_is_refused_rather_than_guessed() {
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/turns", &token, &json!({ "v": 2, "text": "future" })).await;
    assert_eq!(status, 426, "{response}");
    assert!(body(&response).contains("\"kind\":\"version\""), "{response}");
    assert_eq!(backend.turn_count(), 0);
    handle.abort();
}

#[tokio::test]
async fn a_second_turn_while_one_is_open_is_refused() {
    let (store, dir) = seeded_open_turn().await;
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/turns", &token, &json!({ "v": 1, "text": "again" })).await;
    assert_eq!(status, 409, "{response}");
    assert!(body(&response).contains("\"code\":\"turn_in_progress\""), "{response}");
    assert_eq!(backend.turn_count(), 0, "a refused turn must not reach the driver");
    handle.abort();
}

#[tokio::test]
async fn an_approval_is_settled_once() {
    let (store, dir) = seeded_approval().await;
    let backend = FakeBackend::new();
    backend.hold_approvals(true);
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) = post(
        addr,
        "/api/sessions/s1/approvals/ap-1",
        &token,
        &json!({ "v": 1, "decision": "once" }),
    )
    .await;
    assert_eq!(status, 200, "{response}");
    assert!(body(&response).contains("\"state\":\"granted\""), "{response}");
    assert!(eventually(|| backend.decision_count() == 1).await);

    // The fake holds the settlement open, so the first decision is still in
    // flight. A second click must not start a second resume.
    let (status, response) = post(
        addr,
        "/api/sessions/s1/approvals/ap-1",
        &token,
        &json!({ "v": 1, "decision": "deny" }),
    )
    .await;
    assert_eq!(status, 409, "{response}");
    assert!(body(&response).contains("\"code\":\"approval_not_pending\""), "{response}");
    assert_eq!(backend.decision_count(), 1);
    backend.release.notify_waiters();
    handle.abort();
}

#[tokio::test]
async fn an_expired_approval_is_not_offered_and_can_be_carried_forward() {
    let (store, dir) = seeded_approval_at(Utc::now() - Duration::hours(25)).await;
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) = request(addr, "/api/sessions/s1/transcript", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    let approval = payload["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["kind"] == "approval")
        .expect("an approval row");
    assert_eq!(approval["state"], "expired", "{payload}");
    assert!(approval["expires_at"].as_str().is_some(), "{payload}");

    let (status, response) = post(
        addr,
        "/api/sessions/s1/approvals/ap-1",
        &token,
        &json!({ "v": 1, "decision": "once" }),
    )
    .await;
    assert_eq!(status, 409, "{response}");
    assert!(body(&response).contains("\"code\":\"approval_not_pending\""), "{response}");

    let (status, response) =
        post(addr, "/api/sessions/s1/resume", &token, &json!({ "v": 1 })).await;
    assert_eq!(status, 202, "{response}");
    assert!(eventually(|| backend.resumes.lock().unwrap().len() == 1).await);
    handle.abort();
}

#[tokio::test]
async fn a_fork_returns_the_child_session() {
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/fork", &token, &json!({ "v": 1, "reason": "try another" }))
            .await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["forked"], true);
    assert_eq!(payload["session_id"], "child");
    assert!(eventually(|| backend.forks.lock().unwrap().len() == 1).await);
    handle.abort();
}

#[tokio::test]
async fn steer_and_interrupt_reach_the_driver() {
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/steer", &token, &json!({ "v": 1, "text": "look here" })).await;
    assert_eq!(status, 202, "{response}");
    assert!(eventually(|| backend.steers.lock().unwrap().len() == 1).await);

    let (status, response) =
        post(addr, "/api/sessions/s1/interrupt", &token, &json!({ "v": 1 })).await;
    assert_eq!(status, 200, "{response}");
    assert!(eventually(|| backend.interrupts.lock().unwrap().len() == 1).await);
    handle.abort();
}

#[tokio::test]
async fn interrupt_says_so_when_the_turn_cannot_be_stopped() {
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    backend.interrupt_queued(false);
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/interrupt", &token, &json!({ "v": 1 })).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    // The page must not promise a stop that will never happen.
    assert_eq!(payload["interrupting"], false);
    assert!(payload["message"].as_str().map(|text| !text.is_empty()).unwrap_or(false));
    handle.abort();
}

#[tokio::test]
async fn choosing_a_model_reaches_the_driver_and_is_named_in_the_reply() {
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) = post(
        addr,
        "/api/sessions/s1/model",
        &token,
        &json!({ "v": 1, "provider": "openai", "model": "deepseek-flash" }),
    )
    .await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["model"]["provider"], "openai");
    assert_eq!(payload["model"]["model"], "deepseek-flash");

    let calls = backend.models.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![("s1".to_string(), "openai".to_string(), Some("deepseek-flash".to_string()))]
    );
    handle.abort();
}

#[tokio::test]
async fn a_model_the_build_cannot_make_is_the_requests_fault() {
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    backend.refuse_models();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/model", &token, &json!({ "v": 1, "provider": "nope" })).await;
    // Not a 500: picking a provider this build does not have is a bad request,
    // and the page shows the message beside the composer.
    assert_eq!(status, 400, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert!(
        payload["message"].as_str().unwrap_or("").contains("nope"),
        "the refusal names what was asked for: {response}"
    );

    let (status, response) =
        post(addr, "/api/sessions/s1/model", &token, &json!({ "v": 1, "provider": "" })).await;
    assert_eq!(status, 400, "{response}");
    assert!(backend.models.lock().unwrap().is_empty(), "nothing was recorded");
    handle.abort();
}

#[tokio::test]
async fn a_session_summary_reports_the_model_it_was_set_to() {
    let (store, dir, session) = seeded().await;
    store
        .append(
            &session,
            EventSeq::new(5),
            vec![draft(
                &session,
                EventType::SESSION_MODEL_SELECTED,
                None,
                None,
                json!({ "provider": "openai", "model": "deepseek-flash" }),
                Actor::User,
            )],
        )
        .await
        .expect("record the choice");
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    assert_eq!(payload["session"]["model"]["provider"], "openai");
    assert_eq!(payload["session"]["model"]["model"], "deepseek-flash");
    handle.abort();
}

#[tokio::test]
async fn archiving_a_session_is_an_event_its_log_keeps() {
    let (store, dir, session) = seeded().await;
    let (addr, token, handle) = serve_over(store.clone(), dir.path().to_path_buf()).await;

    let (status, response) =
        post(addr, "/api/sessions/s1/archive", &token, &json!({ "v": 1, "archived": true })).await;
    assert_eq!(status, 200, "{response}");

    // The list says it is archived; the transcript is untouched — archiving is
    // a decision about the list, not a change to what happened.
    let (_, listed) = request(addr, "/api/sessions", Some(&token), None).await;
    let listed: Value = serde_json::from_str(body(&listed)).expect("a JSON body");
    assert_eq!(listed["sessions"][0]["archived"], true);
    let (status, transcript) =
        request(addr, "/api/sessions/s1/transcript", Some(&token), None).await;
    assert_eq!(status, 200, "{transcript}");

    // Taking it back out is one more event, and the log still remembers both.
    let (status, response) =
        post(addr, "/api/sessions/s1/archive", &token, &json!({ "v": 1, "archived": false })).await;
    assert_eq!(status, 200, "{response}");
    let (_, listed) = request(addr, "/api/sessions", Some(&token), None).await;
    let listed: Value = serde_json::from_str(body(&listed)).expect("a JSON body");
    assert_eq!(listed["sessions"][0]["archived"], false);

    let events = store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.expect("reads");
    let archive_events: Vec<&str> = events
        .iter()
        .map(|event| event.event.event_type.as_str())
        .filter(|name| {
            name.starts_with("session/archived") || name.starts_with("session/unarchived")
        })
        .collect();
    assert_eq!(archive_events, vec![EventType::SESSION_ARCHIVED, EventType::SESSION_UNARCHIVED]);
    handle.abort();
}

#[tokio::test]
async fn deleting_a_session_removes_its_log_from_the_disk() {
    let (store, dir, _session) = seeded().await;
    let log = dir.path().join("s1").join("events.jsonl");
    assert!(log.is_file(), "the seed wrote a log");
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = delete(addr, "/api/sessions/s1", &token).await;
    assert_eq!(status, 200, "{response}");
    assert!(!log.exists(), "the log is gone, not hidden");
    assert!(!dir.path().join("s1").exists(), "the session directory goes with it");

    // And the id is no longer a session to anyone: the endpoints say so.
    let (status, response) = request(addr, "/api/sessions/s1", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    let (_, listed) = request(addr, "/api/sessions", Some(&token), None).await;
    let listed: Value = serde_json::from_str(body(&listed)).expect("a JSON body");
    assert_eq!(listed["sessions"].as_array().map(Vec::len), Some(0));
    handle.abort();
}

#[tokio::test]
async fn deleting_a_session_under_a_running_turn_is_refused() {
    let (store, dir) = seeded_open_turn().await;
    let backend = FakeBackend::new();
    backend.refuse_delete();
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend.clone())).await;

    let (status, response) = delete(addr, "/api/sessions/s1", &token).await;
    assert_eq!(status, 409, "{response}");
    assert!(dir.path().join("s1").join("events.jsonl").is_file(), "the log survives a refusal");
    assert!(backend.deleted.lock().unwrap().is_empty(), "a refused delete records nothing");
    handle.abort();
}

#[tokio::test]
async fn the_event_stream_replays_history_before_it_waits() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = read_sse(addr, "/api/sessions/s1/events?from=0", Some(&token)).await;
    assert_eq!(status, 200, "{response}");
    assert!(response.contains("content-type: text/event-stream"), "{response}");
    assert!(response.contains("event: hello"), "{response}");
    assert!(response.contains("event: item"), "{response}");
    assert!(response.contains("\"kind\":\"user\""), "the prompt is replayed: {response}");
    // The replay is complete enough that the connection is open for more, not
    // closed after a snapshot.
    assert!(response.contains("HTTP/1.1 200"), "{response}");
    handle.abort();
}

#[tokio::test]
async fn a_build_with_no_driver_reads_but_does_not_run() {
    // `agent web` without a driver still serves the log: reading is this
    // process's own business. What it cannot do is start work, and every verb
    // that would says which capability is missing in the same envelope rather
    // than failing silently or pretending to have queued something.
    let (store, dir, _session) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/transcript", Some(&token), None).await;
    assert_eq!(status, 200, "the log is still readable: {response}");

    let verbs = [
        ("turns", json!({ "v": 1, "text": "hola" })),
        ("resume", json!({ "v": 1 })),
        ("fork", json!({ "v": 1, "reason": "another way" })),
        ("steer", json!({ "v": 1, "text": "not that" })),
        ("interrupt", json!({ "v": 1 })),
        ("model", json!({ "v": 1, "provider": "echo" })),
        ("approvals/ap-1", json!({ "v": 1, "decision": "once" })),
    ];
    for (endpoint, payload) in verbs {
        let (status, response) =
            post(addr, &format!("/api/sessions/s1/{endpoint}"), &token, &payload).await;
        assert_eq!(status, 501, "{endpoint} answered {status}: {response}");
        assert!(
            body(&response).contains("\"code\":\"no_backend\""),
            "{endpoint} names the missing driver: {response}"
        );
    }

    // Deleting is the exception: the log is this process's to remove even when
    // nothing can run in it, and an endpoint that only ever said "no driver"
    // would leave a person unable to clear a broken session.
    let (status, response) = delete(addr, "/api/sessions/s1", &token).await;
    assert_eq!(status, 200, "{response}");
    assert!(!dir.path().join("s1").exists(), "the log is gone");
    handle.abort();
}

#[tokio::test]
async fn live_text_reaches_an_open_stream_with_its_kind() {
    // A preview is not part of the log, so the only place it can be seen is a
    // stream that was already open when the turn spoke. Each kind travels with
    // its own label: the page draws the model's reasoning differently from its
    // prose, and a note from the host differently from both.
    let (store, dir, _) = seeded().await;
    let backend = FakeBackend::new();
    backend.previews(vec![
        LiveChunk::text("thinking about it"),
        LiveChunk::reasoning("because the file is there"),
        LiveChunk::notice("the shell was refused"),
    ]);
    let (addr, token, handle) =
        serve_over_with(store, dir.path().to_path_buf(), Some(backend)).await;

    // The stream is read on its own task: it stays open for the length of the
    // turn, which is the only way to be subscribed when the turn speaks.
    let seen = Arc::new(Mutex::new(String::new()));
    let sink = seen.clone();
    let reader_token = token.clone();
    let reader = tokio::spawn(async move {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "GET /api/sessions/s1/events?from=0 HTTP/1.1\r\nHost: {addr}\r\n\
             Authorization: Bearer {reader_token}\r\nAccept: text/event-stream\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.expect("write");
        loop {
            let mut buffer = [0u8; 2048];
            let read = stream.read(&mut buffer).await.expect("read");
            if read == 0 {
                break;
            }
            let mut sink = sink.lock().unwrap();
            sink.push_str(&String::from_utf8_lossy(&buffer[..read]));
            if sink.contains("the shell was refused") {
                break;
            }
        }
    });

    // Subscribed before the turn starts: the replay is what says so.
    assert!(eventually(|| seen.lock().unwrap().contains("event: item")).await);
    let (status, response) =
        post(addr, "/api/sessions/s1/turns", &token, &json!({ "v": 1, "text": "go" })).await;
    assert_eq!(status, 202, "{response}");
    assert!(eventually(|| seen.lock().unwrap().contains("the shell was refused")).await);

    let text = seen.lock().unwrap().clone();
    for (kind, words) in [
        ("text", "thinking about it"),
        ("reasoning", "because the file is there"),
        ("notice", "the shell was refused"),
    ] {
        assert!(text.contains("event: live"), "the previews arrive as live events: {text}");
        assert!(text.contains(&format!("\"kind\":\"{kind}\"")), "the {kind} label: {text}");
        assert!(text.contains(words), "the {kind} text: {text}");
    }
    reader.abort();
    handle.abort();
}

#[tokio::test]
async fn the_transcript_shows_the_rows_a_turn_produces() {
    // A turn that reasoned, ran a tool and ended: the three rows the page
    // draws beyond the prompt and the answer. Each comes from its own event,
    // and the fold is what turns them into the shape the page reads.
    let (store, dir) = empty();
    let session = SessionId::from_string("s1");
    let turn = TurnId::from_string("t1");
    let step = StepId::from_string("s1");
    let node = ReasonNode {
        id: "r/t1/1".to_string(),
        turn_id: "t1".to_string(),
        step: 0,
        kind: ReasonKind::Situation,
        claim: "the person asked for a file".to_string(),
        basis: vec![Basis { node: None, event: None, fact: Some("their words".to_string()) }],
        rule: "situation/derive".to_string(),
        source: ReasonSource::Kernel,
    };
    let drafts = vec![
        draft(
            &session,
            EventType::TURN_STARTED,
            Some(&turn),
            None,
            json!({ "inbox_item": "i1" }),
            Actor::Scheduler,
        ),
        draft(
            &session,
            EventType::REASON_RECORDED,
            Some(&turn),
            None,
            json!({ "node": node }),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::TOOL_STARTED,
            Some(&turn),
            Some(&step),
            json!({
                "tool_name": "read_file",
                "provider_tool_call_id": "c1",
                "arguments": { "path": "a.txt" },
            }),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::TOOL_COMPLETED,
            Some(&turn),
            Some(&step),
            json!({ "tool_name": "read_file", "provider_tool_call_id": "c1", "content": "hello" }),
            Actor::Agent,
        ),
        draft(
            &session,
            EventType::TURN_COMPLETED,
            Some(&turn),
            None,
            json!({ "final_content": "there it is" }),
            Actor::Scheduler,
        ),
    ];
    store.append(&session, EventSeq::ZERO, drafts).await.expect("seed the log");
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/transcript", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let transcript: Value = serde_json::from_str(body(&response)).expect("a JSON body");
    let items = transcript["items"].as_array().expect("a list of rows");
    let row = |kind: &str| {
        items
            .iter()
            .find(|item| item["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} row in {}", body(&response)))
    };

    // The reasoning line carries how many steps it took to get there.
    let thinking = row("thinking");
    assert!(thinking["text"].as_str().unwrap_or_default().contains("asked for a file"));
    assert_eq!(thinking["steps"], 1);

    // The tool row is one row for both halves: what it was asked and what it
    // said back, under the same id so the page upserts rather than appends.
    let action = row("action");
    assert_eq!(action["tool"], "read_file");
    assert_eq!(action["status"], "completed");
    assert_eq!(action["content"], "hello");
    assert_eq!(action["arguments"]["path"], "a.txt");
    assert!(action["duration_ms"].is_number(), "the tool row is timed: {action}");

    // And the turn says how it ended, separately from what it answered.
    assert_eq!(row("turn_end")["ending"], "answered");
    assert_eq!(row("answer")["text"], "there it is");
    handle.abort();
}

#[tokio::test]
async fn a_request_the_surface_cannot_answer_is_refused_by_name() {
    let (store, dir, _session) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    // A cursor that is not a whole number: answering with an empty transcript
    // would say the turn was empty, which is not what the log holds.
    let (status, response) =
        request(addr, "/api/sessions/s1/transcript?since=yesterday", Some(&token), None).await;
    assert_eq!(status, 400, "{response}");
    assert!(body(&response).contains("\"code\":\"bad_request\""), "{response}");
    assert!(body(&response).contains("since"), "the refusal names the cursor: {response}");
    let (status, response) =
        request(addr, "/api/sessions/s1/events?from=soon", Some(&token), None).await;
    assert_eq!(status, 400, "{response}");
    assert!(body(&response).contains("cursor"), "the refusal names the cursor: {response}");

    // An id that is not a session id is refused before it becomes a path, on
    // every verb that takes one.
    for path in ["/api/sessions/a%2Fb/transcript", "/api/sessions/a%2Fb/events"] {
        let (status, response) = request(addr, path, Some(&token), None).await;
        assert_eq!(status, 400, "{path}: {response}");
    }
    let (status, response) = delete(addr, "/api/sessions/a%2Fb", &token).await;
    assert_eq!(status, 400, "{response}");
    let (status, response) =
        post(addr, "/api/sessions/a%2Fb/archive", &token, &json!({ "v": 1, "archived": true }))
            .await;
    assert_eq!(status, 400, "{response}");

    // A body that is not JSON is the request's fault, not a 500.
    let (status, response) =
        post_raw(addr, "/api/sessions/s1/turns", &token, Some("application/json"), "{\"v\":1,")
            .await;
    assert_eq!(status, 400, "{response}");
    assert!(body(&response).contains("\"code\":\"bad_request\""), "{response}");

    // Archiving without saying which way is refused rather than guessed.
    let (status, response) =
        post(addr, "/api/sessions/s1/archive", &token, &json!({ "v": 1 })).await;
    assert_eq!(status, 400, "{response}");
    assert!(body(&response).contains("archived"), "{response}");

    // The icon the browser asks for on every load, answered without a body.
    let (status, response) = request(addr, "/favicon.ico", Some(&token), None).await;
    assert_eq!(status, 204, "{response}");

    // A stream for a session no turn has written yet waits for the write, then
    // names the id it could not find.
    let (status, response) =
        request(addr, "/api/sessions/ghost/events?from=0", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    assert!(body(&response).contains("ghost"), "{response}");
    handle.abort();
}
