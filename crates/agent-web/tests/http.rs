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
use agent_web::{Backend, BackendError, Config, Decision, LiveSink, Meta, Server};
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
    resumes: Mutex<Vec<String>>,
    forks: Mutex<Vec<(String, Option<String>, String)>>,
    hold_approval: AtomicBool,
    /// What `interrupt` reports back: `true` unless a test says there is no
    /// open turn to stop.
    interrupt_queued: AtomicBool,
    release: Notify,
}

impl FakeBackend {
    fn new() -> Arc<Self> {
        let backend = Arc::new(Self::default());
        backend.interrupt_queued.store(true, Ordering::SeqCst);
        backend
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
}

#[async_trait::async_trait]
impl Backend for FakeBackend {
    async fn run_turn(
        &self,
        session: &SessionId,
        text: &str,
        _live: &LiveSink,
    ) -> Result<(), BackendError> {
        self.turns.lock().unwrap().push((session.as_str().to_string(), text.to_string()));
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
    let server = Server::bind(Config { port: 0, meta: meta(), store, data_dir, backend })
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
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let body = payload.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
