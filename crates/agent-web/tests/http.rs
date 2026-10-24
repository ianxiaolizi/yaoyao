//! The loopback surface: what needs the token, what the read endpoints answer,
//! and what a wrong host, path or session gets.
//!
//! The client here is the smallest thing that can speak HTTP/1.1 over a socket.
//! A TLS-capable client would pull a native crypto build into a crate whose
//! subject is a plaintext loopback listener, and the test would then be about
//! the machine's C toolchain.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use agent_ports::event_store::EventStore;
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{
    EventId, EventSeq, InboxItemId, SchemaVersion, SessionId, StepId, TurnId,
};
use agent_protocol::inbox::{InboxItem, InboxItemStatus, InboxKind};
use agent_protocol::model::ModelResponse;
use agent_protocol::reason::{Basis, ReasonKind, ReasonNode, ReasonSource};
use agent_store_jsonl::JsonlEventStore;
use agent_web::{Config, Meta, Server};
use chrono::Utc;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn meta() -> Meta {
    Meta {
        workspace: "yaoyao-public".to_string(),
        model: "deepseek-flash".to_string(),
        sandbox: "local".to_string(),
        judgment: "gates".to_string(),
        capabilities: vec!["meta".to_string()],
    }
}

/// An empty data directory with a store over it.
fn empty() -> (Arc<dyn EventStore>, TempDir) {
    let dir = tempfile::tempdir().expect("a temporary data directory");
    let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(dir.path()));
    (store, dir)
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
    let server = Server::bind(Config { port: 0, meta: meta(), store, data_dir })
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
async fn a_session_without_a_chain_is_not_found() {
    let (store, dir, _) = seeded().await;
    let (addr, token, handle) = serve_over(store, dir.path().to_path_buf()).await;

    let (status, response) = request(addr, "/api/sessions/s1/why", Some(&token), None).await;
    assert_eq!(status, 404, "{response}");
    assert!(body(&response).contains("\"code\":\"why_unavailable\""), "{response}");
    handle.abort();
}
