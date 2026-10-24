//! agent-web: the loopback HTTP surface.
//!
//! This crate is a front door, not a second agent. It binds `127.0.0.1`, serves
//! the embedded UI, folds the read model, and streams the log; the write
//! endpoints hand their verbs to a [`Backend`] implemented by the binary, so the
//! GUI calls the same command layer the terminal does. The rules it keeps are
//! the ones the contract makes non-negotiable: loopback only, a bearer token on
//! every API call, and a version on every object.
//!
//! The token is generated per process and printed once as part of the URL. It
//! lives in this process's memory only — not in the log, not in the UI's storage.
//! The page reads it from the URL fragment, which the browser does not send to a
//! server, and puts it in an `Authorization` header.

#![deny(unsafe_code)]

mod backend;
mod sessions;
mod stream;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use agent_cli::reason::WhyError;
use agent_kernel::inbox::{Inbox, scheduled_due_at};
use agent_ports::artifact_store::ArtifactStore;
use agent_ports::event_store::EventStore;
use agent_protocol::events::Actor;
use agent_protocol::ids::{EventSeq, InboxItemId, SessionId};
use agent_protocol::permissions::ArtifactRef;
use agent_store_artifact::FsArtifactStore;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::net::TcpListener;

pub use backend::{Backend, BackendError, Decision, LiveChunk, LiveKind, LiveSink};

/// The wire protocol this build speaks.
///
/// A future version is refused by a future build, not guessed at here; the same
/// rule C6.1 puts on every other cross-process protocol in the repository.
pub const PROTOCOL: u32 = 1;

/// What the page needs to know about the process it is talking to.
///
/// Filled in by the assembly (`apps/agent`), which is the only place that knows
/// which configuration was selected. Nothing here is a secret: the workspace is
/// a directory *label*, and the model name is the one the run would use.
#[derive(Debug, Clone)]
pub struct Meta {
    pub workspace: String,
    pub model: String,
    pub sandbox: String,
    pub judgment: String,
    /// The input window the configuration resolved to, when it named one. The
    /// page uses it only to draw a proportionate context bar; a missing value
    /// means "no budget was stated", not "zero".
    pub budget: Option<u32>,
    /// What this build can actually do. The UI greys out what is missing rather
    /// than offering a button that can only fail.
    pub capabilities: Vec<String>,
}

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("cannot bind 127.0.0.1:{port}: {source}")]
    Bind { port: u16, source: std::io::Error },
    #[error("the server stopped: {0}")]
    Serve(#[source] std::io::Error),
}

/// How a refusal is labelled on the wire.
#[derive(Debug, Clone, Copy)]
enum FailureKind {
    Unauthorized,
    Refused,
}

impl FailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Refused => "refused",
        }
    }
}

/// What the server needs from the process it runs in.
pub struct Config {
    pub port: u16,
    pub meta: Meta,
    pub store: Arc<dyn EventStore>,
    /// Where session directories live. The read model enumerates this directory;
    /// the store is the reader, so nothing here appends.
    pub data_dir: PathBuf,
    /// The process that can run a turn. `None` is a read-only server: the page
    /// still lists and reads sessions, and the write endpoints answer "this
    /// build cannot run turns" rather than pretending to accept one.
    pub backend: Option<Arc<dyn Backend>>,
}

/// The state every request shares.
struct AppState {
    token: String,
    meta: Meta,
    store: Arc<dyn EventStore>,
    data_dir: PathBuf,
    backend: Option<Arc<dyn Backend>>,
    /// Folded logs, keyed by `(size, mtime)`. The sidebar refresh and the
    /// transcript endpoint read through this; a turn that appends invalidates
    /// its entry by touching the file.
    reads: sessions::ReadModel,
    /// Live previews fan out from here. One channel per session, kept for the
    /// life of the process: a sender with no receivers is a few words of memory,
    /// and re-creating it would make a page that connects mid-turn miss the run.
    live: Mutex<HashMap<String, LiveSink>>,
    /// Sessions with a turn this process is driving. The durable lifecycle is
    /// the authority; this closes the window between accepting a turn and its
    /// first event landing, in which a second POST would otherwise look idle.
    running: Mutex<HashSet<String>>,
    /// Approval ids a request in this process is already settling. Two clicks on
    /// one card must not produce two resumes.
    settling: Mutex<HashSet<String>>,
}

impl AppState {
    /// The live fan-out for one session, created on first use.
    fn live_sink(&self, session: &str) -> LiveSink {
        let mut channels = lock(&self.live);
        channels
            .entry(session.to_string())
            .or_insert_with(|| tokio::sync::broadcast::channel(256).0)
            .clone()
    }
}

/// Lock a short-lived in-process set. A panic elsewhere should not turn the
/// server into a permanently unanswerable one; the data in these sets is an
/// optimization over the log, so recovering a poisoned guard is safe.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Refuse a session id before it reaches a store or a path.
fn check_id(id: &str) -> Result<(), ApiFailure> {
    if !sessions::safe_session_id(id) {
        return Err(ApiFailure::bad_request(format!("'{id}' is not a session id")));
    }
    Ok(())
}

/// Refuse a session id, or one this data directory has never seen.
fn check_session(state: &AppState, id: &str) -> Result<(), ApiFailure> {
    check_id(id)?;
    if !session_exists(state, id) {
        return Err(ApiFailure::not_found(format!("no session '{id}'")));
    }
    Ok(())
}

fn session_exists(state: &AppState, id: &str) -> bool {
    state.data_dir.join(id).join("events.jsonl").is_file()
}

/// Wait briefly for a session log that a turn is in the middle of creating.
///
/// The endpoint does not create it: that would make a read a writer. It only
/// gives the write a two-second head start, which is enough for the common
/// "accept a turn, then open the stream" sequence and cheap for a genuinely
/// unknown id.
async fn wait_for_session(state: &AppState, id: &str) -> bool {
    if session_exists(state, id) {
        return true;
    }
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if session_exists(state, id) {
            return true;
        }
    }
    false
}

/// A JSON body, or a refusal in the same envelope as everything else.
fn json_body(body: Result<Json<Value>, JsonRejection>) -> Result<Value, ApiFailure> {
    body.map(|Json(value)| value).map_err(|error| ApiFailure::bad_request(error.body_text()))
}

/// Refuse a request from a newer front end instead of guessing at its fields.
fn check_version(body: &Value) -> Result<(), ApiFailure> {
    match body.get("v").and_then(Value::as_u64) {
        Some(version) if version > u64::from(PROTOCOL) => Err(ApiFailure::version(format!(
            "this server speaks protocol {PROTOCOL}; the page sent version {version}"
        ))),
        _ => Ok(()),
    }
}

/// Read an optional sequence cursor.
fn parse_seq(value: Option<&String>) -> Result<Option<u64>, ApiFailure> {
    match value {
        Some(raw) => raw.parse::<u64>().map(Some).map_err(|_| {
            ApiFailure::bad_request(format!("cursor must be a whole number, not '{raw}'"))
        }),
        None => Ok(None),
    }
}

/// A bound, not-yet-serving server.
///
/// Binding and serving are separate so a test can learn the port (bind 0) and
/// the token before the accept loop starts — the alternative is a test that
/// races its own server.
pub struct Server {
    listener: TcpListener,
    addr: SocketAddr,
    token: String,
    app: Router,
}

impl Server {
    /// Bind the loopback interface and build the router.
    ///
    /// A port of 0 lets the operating system pick, which is what tests use. The
    /// address is never `0.0.0.0`: a GUI that answers the network is a different
    /// product with a different threat model.
    pub async fn bind(config: Config) -> Result<Self, WebError> {
        let Config { port, meta, store, data_dir, backend } = config;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|source| WebError::Bind { port, source })?;
        let addr = listener.local_addr().map_err(|source| WebError::Bind { port, source })?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let app = router(Arc::new(AppState {
            token: token.clone(),
            meta,
            store,
            data_dir,
            backend,
            reads: sessions::ReadModel::new(),
            live: Mutex::new(HashMap::new()),
            running: Mutex::new(HashSet::new()),
            settling: Mutex::new(HashSet::new()),
        }));
        Ok(Self { listener, addr, token, app })
    }

    /// The address actually bound.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The one-time token the page must present.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The URL to open: the fragment carries the token, the request does not.
    pub fn url(&self) -> String {
        format!("http://{}/#t={}", self.addr, self.token)
    }

    /// Serve until the process stops.
    pub async fn serve(self) -> Result<(), WebError> {
        axum::serve(self.listener, self.app).await.map_err(WebError::Serve)
    }
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/app.css", get(app_css))
        .route("/app.js", get(app_js))
        .route("/api/meta", get(meta))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}", get(session))
        .route("/api/sessions/{id}/transcript", get(transcript))
        .route("/api/sessions/{id}/events", get(events))
        .route("/api/sessions/{id}/diff", get(diff))
        .route("/api/sessions/{id}/context", get(context))
        .route("/api/sessions/{id}/plan", get(plan))
        .route("/api/sessions/{id}/subagents", get(subagents))
        .route("/api/sessions/{id}/schedules", get(schedules).post(schedule))
        .route("/api/sessions/{id}/schedules/{item_id}", axum::routing::delete(cancel_schedule))
        .route("/api/sessions/{id}/export", get(export))
        .route("/api/artifacts/{*locator}", get(artifact))
        .route("/api/sessions/{id}/turns", post(turn))
        .route("/api/sessions/{id}/approvals/{approval_id}", post(approve))
        .route("/api/sessions/{id}/resume", post(resume))
        .route("/api/sessions/{id}/fork", post(fork))
        .route("/api/sessions/{id}/steer", post(steer))
        .route("/api/sessions/{id}/interrupt", post(interrupt))
        .route("/api/sessions/{id}/why", get(why))
        .layer(middleware::from_fn_with_state(state.clone(), gate))
        .with_state(state)
}

/// The page shell.
///
/// The page is public because it holds no data; every `/api/` call is gated. The
/// CSP is the same one the contract promises: only this origin, and inline
/// styles because the glass and the drag handles are computed at runtime.
async fn index() -> impl IntoResponse {
    (
        [(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; \
             connect-src 'self' blob:; script-src 'self'",
        )],
        Html(include_str!("../static/index.html")),
    )
}

/// No icon is served; the page uses the inline mark. Answering 204 rather than
/// letting the browser log a 404 keeps the console useful for real failures.
async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// The stylesheet, embedded like the page so production runs no separate web
/// server.
async fn app_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../static/app.css"))
}

/// The page's script, embedded for the same reason.
async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../static/app.js"))
}

async fn meta(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "v": PROTOCOL,
        "ok": true,
        "protocol": PROTOCOL,
        "app": "yaoyao",
        "workspace": state.meta.workspace,
        "model": state.meta.model,
        "sandbox": state.meta.sandbox,
        "judgment": state.meta.judgment,
        "budget": state.meta.budget,
        "capabilities": state.meta.capabilities,
    }))
}

/// `GET /api/sessions`: every session under the data directory, newest first.
async fn sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    let list = sessions::list(state.store.as_ref(), &state.data_dir, &state.reads)
        .await
        .map_err(ApiFailure::read)?;
    Ok(Json(serde_json::json!({ "v": PROTOCOL, "sessions": list })))
}

/// `GET /api/sessions/{id}`: one summary, the same shape as one list entry.
async fn session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let list = sessions::list(state.store.as_ref(), &state.data_dir, &state.reads)
        .await
        .map_err(ApiFailure::read)?;
    let found = list
        .into_iter()
        .find(|row| row["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| ApiFailure::not_found(format!("no session '{id}'")))?;
    Ok(Json(json!({ "v": PROTOCOL, "session": found })))
}

/// `GET /api/sessions/{id}/transcript?since=<seq>`.
///
/// `since` is compared against each item's **last** change, not its first: a
/// tool that started before the cursor and finished after it is part of the
/// answer.
async fn transcript(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    if !sessions::safe_session_id(&id) {
        return Err(ApiFailure::bad_request(format!("'{id}' is not a session id")));
    }
    let since = match query.get("since") {
        Some(value) => Some(value.parse::<u64>().map_err(|_| {
            ApiFailure::bad_request(format!("since must be a whole number, not '{value}'"))
        })?),
        None => None,
    };
    if !state.data_dir.join(&id).join("events.jsonl").is_file() {
        return Err(ApiFailure::not_found(format!("no session '{id}'")));
    }
    let payload = sessions::transcript(
        state.store.as_ref(),
        &state.data_dir,
        &SessionId::from_string(&id),
        since,
        &state.reads,
    )
    .await
    .map_err(ApiFailure::read)?;
    Ok(Json(payload))
}

/// `GET /api/sessions/{id}/events?from=<seq>`: the session as a live stream.
///
/// Subscribe before reading. A turn that opens in between is then in both
/// places, and the dedup by sequence in [`stream::response`] makes it one item
/// on the wire. The other order — read, then subscribe — has a hole exactly the
/// width of a model's first token.
async fn events(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<impl IntoResponse, ApiFailure> {
    check_id(&id)?;
    // A brand-new session's log is created by its first turn, which the page
    // has just accepted. Wait a moment for that append rather than answering
    // 404 for a session that is already on its way; the retry in the page is
    // the fallback for a slower assembly, not the normal path.
    if !wait_for_session(&state, &id).await {
        return Err(ApiFailure::not_found(format!("no session '{id}'")));
    }
    // SSE's `from` is inclusive: `from=0` is the whole log, which is the only
    // spelling that can express "before the first event" (there is no seq -1).
    let from = parse_seq(query.get("from"))?.unwrap_or(0);
    let session = SessionId::from_string(&id);

    let subscription = state.store.subscribe(&session, EventSeq::ZERO).await.map_err(|error| {
        ApiFailure::read(sessions::ReadError::Session {
            session: id.clone(),
            message: error.to_string(),
        })
    })?;
    let history =
        state.store.read_stream(&session, EventSeq::ZERO, usize::MAX).await.map_err(|error| {
            ApiFailure::read(sessions::ReadError::Session {
                session: id.clone(),
                message: error.to_string(),
            })
        })?;
    let transcript = sessions::LiveTranscript::fold(&history).map_err(ApiFailure::read)?;
    let head = transcript.head();
    let initial = transcript.snapshot(from);
    let live = state.live_sink(&id).subscribe();

    // The local `subscription` is moved into the stream; the type is boxed so
    // the response's generics stay small.
    let stream = stream::response(id, head, initial, transcript, subscription, live);
    Ok(stream)
}

/// `GET /api/sessions/{id}/diff`: the newest `git.diff` observation.
///
/// A reader only. It does not run git and does not ask the workspace what it
/// looks like now: the page shows what the agent actually saw, which is the
/// same rule every other read endpoint follows.
async fn diff(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let report = sessions::diff(
        state.store.as_ref(),
        &state.data_dir,
        &SessionId::from_string(&id),
        &state.reads,
    )
    .await
    .map_err(ApiFailure::read)?;
    match report {
        Some(report) => Ok(Json(report)),
        None => Err(ApiFailure::diff_unavailable(
            "这一轮没有 git.diff 观察：模型还没看过改动，或工具调用没有完成",
        )),
    }
}

/// `GET /api/sessions/{id}/context`: what the last model request carried.
///
/// This is read back from `model/request.prepared`, not re-estimated from the
/// transcript. A request that was never prepared has no report; an empty
/// report would claim the context is empty, which is a different answer.
async fn context(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let report = sessions::context(
        state.store.as_ref(),
        &state.data_dir,
        &SessionId::from_string(&id),
        &state.reads,
    )
    .await
    .map_err(ApiFailure::read)?;
    match report {
        Some(report) => Ok(Json(report)),
        None => Err(ApiFailure::context_unavailable(
            "这个会话还没有记录过模型请求；跑完第一步之后再来看",
        )),
    }
}

/// `GET /api/sessions/{id}/plan`: the latest `plan/updated` ledger.
async fn plan(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let report = sessions::plan(
        state.store.as_ref(),
        &state.data_dir,
        &SessionId::from_string(&id),
        &state.reads,
    )
    .await
    .map_err(ApiFailure::read)?;
    match report {
        Some(report) => Ok(Json(report)),
        None => Err(ApiFailure::plan_unavailable(
            "这个会话还没有 plan.update；计划是模型自己写的，不是页面编的",
        )),
    }
}

/// `GET /api/sessions/{id}/subagents`: children this session delegated to.
async fn subagents(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let report = sessions::subagents(
        state.store.as_ref(),
        &state.data_dir,
        &SessionId::from_string(&id),
        &state.reads,
    )
    .await
    .map_err(ApiFailure::read)?;
    match report {
        Some(report) => Ok(Json(report)),
        None => Err(ApiFailure::subagents_unavailable(
            "这个会话还没有 subagent.run；子代理只在模型主动委派时产生",
        )),
    }
}

/// `GET /api/sessions/{id}/schedules`: pending scheduled turns.
async fn schedules(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let session = SessionId::from_string(&id);
    let inbox = Inbox::new(state.store.clone());
    let items =
        inbox.scheduled(&session).await.map_err(|error| ApiFailure::internal(error.to_string()))?;
    let now = Utc::now();
    let rows: Vec<Value> = items
        .iter()
        .map(|item| {
            let due = scheduled_due_at(item);
            let text = item.payload.get("text").and_then(Value::as_str);
            match due {
                Ok(due_at) => json!({
                    "item_id": item.item_id.as_str(),
                    "due_at": due_at.to_rfc3339(),
                    "overdue": due_at <= now,
                    "text": text,
                }),
                Err(reason) => json!({
                    "item_id": item.item_id.as_str(),
                    "due_at": null,
                    "overdue": false,
                    "text": text,
                    "error": reason,
                }),
            }
        })
        .collect();
    Ok(Json(json!({ "v": PROTOCOL, "session": id, "items": rows })))
}

/// `POST /api/sessions/{id}/schedules`: queue one future turn.
///
/// This deliberately does not assemble a runtime: the CLI keeps scheduling
/// independent of a model or sandbox that may currently be broken.
async fn schedule(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let body = json_body(body)?;
    check_version(&body)?;
    let when = body.get("when").and_then(Value::as_str).unwrap_or("").trim();
    let text = body.get("text").and_then(Value::as_str).unwrap_or("").trim();
    if when.is_empty() || text.is_empty() {
        return Err(ApiFailure::bad_request("when and text must both be present"));
    }
    let now = Utc::now();
    let due_at = agent_kernel::schedule::parse_when(when, now)
        .map_err(|error| ApiFailure::bad_request(error.to_string()))?;
    let session = SessionId::from_string(&id);
    let inbox = Inbox::new(state.store.clone());
    let item = inbox
        .schedule(&session, due_at, text, 0, Actor::User)
        .await
        .map_err(|error| ApiFailure::internal(error.to_string()))?;
    Ok(Json(json!({
        "v": PROTOCOL,
        "ok": true,
        "item_id": item.item_id.as_str(),
        "due_at": due_at.to_rfc3339(),
        "text": text,
    })))
}

/// `DELETE /api/sessions/{id}/schedules/{item_id}`: withdraw a scheduled turn.
async fn cancel_schedule(
    State(state): State<Arc<AppState>>,
    AxumPath((id, item_id)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let session = SessionId::from_string(&id);
    let inbox = Inbox::new(state.store.clone());
    inbox
        .cancel_scheduled(&session, &InboxItemId::from_string(&item_id))
        .await
        .map_err(|error| ApiFailure::internal(error.to_string()))?;
    Ok(Json(json!({ "v": PROTOCOL, "ok": true, "cancelled": item_id })))
}

/// `GET /api/sessions/{id}/export?format=md|json`.
///
/// The renderer lives in `agent-cli` because the terminal and the page must
/// export the same document. This handler only chooses the response headers.
async fn export(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiFailure> {
    check_session(&state, &id)?;
    let format = query.get("format").map(String::as_str).unwrap_or("md");
    if format != "md" && format != "json" {
        return Err(ApiFailure::bad_request("format must be md or json"));
    }
    let output = agent_cli::run_export(
        state.store.as_ref(),
        &SessionId::from_string(&id),
        None,
        false,
        format == "json",
    )
    .await
    .map_err(ApiFailure::internal)?;
    let text = match output {
        agent_cli::CliOutput::Text(text) => text,
        agent_cli::CliOutput::Json(json) => json.to_string(),
    };
    let (content_type, extension) = if format == "json" {
        ("application/json; charset=utf-8", "json")
    } else {
        ("text/markdown; charset=utf-8", "md")
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{id}.{extension}\""))
        .body(Body::from(text))
        .map_err(|error| ApiFailure::internal(error.to_string()))
}

/// `GET /api/artifacts/{locator}`: one content-addressed blob.
///
/// The hash is checked against the bytes before they are returned: a file at
/// the right path with the wrong content is a damaged store, not an artifact.
async fn artifact(
    State(state): State<Arc<AppState>>,
    AxumPath(locator): AxumPath<String>,
) -> Result<Response, ApiFailure> {
    let hash = ArtifactRef::hash_from_locator(&locator).unwrap_or(locator.as_str()).to_string();
    if !agent_ports::artifact_store::is_blob_name(&hash) {
        return Err(ApiFailure::bad_request(
            "artifact locator must be a 64-character lowercase hex hash",
        ));
    }
    let store = FsArtifactStore::new(state.data_dir.join("artifacts"));
    let reference =
        ArtifactRef { hash, mime_type: "application/octet-stream".to_string(), size: 0 };
    let bytes = store.get(&reference).map_err(|error| {
        let message = error.to_string();
        match error {
            agent_ports::artifact_store::ArtifactStoreError::InvalidHash(_) => {
                ApiFailure::bad_request(message)
            }
            agent_ports::artifact_store::ArtifactStoreError::NotFound(_) => {
                ApiFailure::not_found(message)
            }
            other => ApiFailure::internal(other.to_string()),
        }
    })?;
    // The store is content-addressed, not type-addressed. Sniff the common
    // image signatures so the page can show a preview instead of downloading a
    // `.blob`; everything else stays opaque.
    let content_type = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{}.blob\"", reference.hash),
        )
        .body(Body::from(bytes))
        .map_err(|error| ApiFailure::internal(error.to_string()))
}

/// `GET /api/sessions/{id}/why?turn=<turn>`.
///
/// A reader, not a run: it opens the log and answers from the nodes the kernel
/// recorded. Nothing here assembles a model or a sandbox, so it still answers
/// when the configuration that ran the turn has since broken.
async fn why(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    if !sessions::safe_session_id(&id) {
        return Err(ApiFailure::bad_request(format!("'{id}' is not a session id")));
    }
    if !state.data_dir.join(&id).join("events.jsonl").is_file() {
        return Err(ApiFailure::not_found(format!("no session '{id}'")));
    }
    let report = agent_cli::reason::why_report(
        state.store.as_ref(),
        &SessionId::from_string(&id),
        query.get("turn").map(String::as_str),
    )
    .await
    .map_err(ApiFailure::why)?;

    Ok(Json(serde_json::json!({
        "v": PROTOCOL,
        "ok": true,
        "session": report.session,
        "turn": report.turn,
        "nodes": report.nodes,
        "verdict": report.verdict,
        "stop": report.stop,
        "check": report.check,
    })))
}

/// `POST /api/sessions/{id}/turns`: accept a prompt and drive it in the
/// background.
///
/// `202` means the text is queued, not that it produced an answer. The work
/// happens on a task because a turn can take minutes; its progress reaches the
/// page through the same event stream as everything else.
async fn turn(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiFailure> {
    check_id(&id)?;
    let body = json_body(body)?;
    check_version(&body)?;
    let text = body.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if text.is_empty() {
        return Err(ApiFailure::bad_request("text must not be empty"));
    }
    let Some(backend) = state.backend.clone() else {
        return Err(ApiFailure::unavailable("this build has no turn driver"));
    };

    let session = SessionId::from_string(&id);
    // A session this data directory has not seen yet is a *new* session, not a
    // missing one: the turn's own inbox append creates its log. Only the checks
    // that need a history are skipped.
    if state.data_dir.join(&id).join("events.jsonl").is_file() {
        let folded = state
            .reads
            .fold(state.store.as_ref(), &state.data_dir, &session)
            .await
            .map_err(ApiFailure::read)?;
        if folded.yielded {
            return Err(ApiFailure::conflict(
                "awaiting_decision",
                "this session is waiting on an approval; answer it before starting another turn",
            ));
        }
        if folded.open_turn.is_some() {
            return Err(ApiFailure::conflict(
                "turn_in_progress",
                "a turn is already running in this session",
            ));
        }
    }
    {
        let mut running = lock(&state.running);
        if !running.insert(id.clone()) {
            return Err(ApiFailure::conflict(
                "turn_in_progress",
                "a turn is already running in this session",
            ));
        }
    }

    let state_for_task = state.clone();
    let session_for_task = session.clone();
    let sink = state.live_sink(&id);
    tokio::spawn(async move {
        let result = backend.run_turn(&session_for_task, &text, &sink).await;
        if let Err(error) = result {
            let _ = sink.send(LiveChunk::notice(format!(
                "这一轮没能跑起来（{}）：{}",
                error.code, error.message
            )));
        }
        lock(&state_for_task.running).remove(&id);
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "v": PROTOCOL,
            "ok": true,
            "accepted": true,
            "session": session.as_str(),
        })),
    ))
}

/// `POST /api/sessions/{id}/approvals/{approval_id}`: settle one wait.
///
/// The decision is recorded synchronously enough that the response can name the
/// state; carrying the turn forward happens on a task, because it is another
/// model call. A second click on the same card is refused before the first
/// resume has had a chance to append anything.
async fn approve(
    State(state): State<Arc<AppState>>,
    AxumPath((id, approval_id)): AxumPath<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let body = json_body(body)?;
    check_version(&body)?;
    let decision =
        body.get("decision").and_then(Value::as_str).and_then(Decision::from_name).ok_or_else(
            || ApiFailure::bad_request("decision must be one of once|turn|session|deny"),
        )?;
    let Some(backend) = state.backend.clone() else {
        return Err(ApiFailure::unavailable("this build has no turn driver"));
    };

    let session = SessionId::from_string(&id);
    let folded = state
        .reads
        .fold(state.store.as_ref(), &state.data_dir, &session)
        .await
        .map_err(ApiFailure::read)?;
    let now = Utc::now();
    let pending = folded.transcript.items.iter().any(|item| {
        matches!(
            &item.body,
            agent_projection::ItemBody::Approval {
                approval_id: row,
                state: agent_projection::ApprovalState::Pending,
                created_at,
                ..
            } if row.as_str() == approval_id
                && !sessions::approval_expired(*created_at, now)
        )
    });
    if !pending {
        return Err(ApiFailure::conflict(
            "approval_not_pending",
            "this approval is no longer waiting for a decision",
        ));
    }

    let key = format!("{id}\u{0}{approval_id}");
    {
        let mut settling = lock(&state.settling);
        if !settling.insert(key.clone()) {
            return Err(ApiFailure::conflict(
                "approval_not_pending",
                "this approval is already being settled",
            ));
        }
    }

    let state_for_task = state.clone();
    let sink = state.live_sink(&id);
    let approval_for_task = approval_id.clone();
    tokio::spawn(async move {
        let result = backend.settle_approval(&session, &approval_for_task, decision, &sink).await;
        if let Err(error) = result {
            let _ = sink.send(LiveChunk::notice(format!(
                "审批没有续跑（{}）：{}",
                error.code, error.message
            )));
        }
        lock(&state_for_task.settling).remove(&key);
    });

    Ok(Json(json!({
        "v": PROTOCOL,
        "ok": true,
        "approval_id": approval_id,
        "state": decision.approval_state(),
        "scope": decision.as_str(),
        "resuming": true,
    })))
}

/// `POST /api/sessions/{id}/resume`: carry a wait that ended on its own.
///
/// The only built-in way a wait ends without a click is the pending TTL. The
/// page cannot manufacture a decision for it, so this endpoint asks `Resume` to
/// read the durable status and record the denial the kernel derives. If a fresh
/// decision is still outstanding it answers `202` and the ordinary approval card
/// remains the next step.
async fn resume(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiFailure> {
    check_session(&state, &id)?;
    if let Ok(Json(body)) = body {
        check_version(&body)?;
    }
    let Some(backend) = state.backend.clone() else {
        return Err(ApiFailure::unavailable("this build has no turn driver"));
    };

    let session = SessionId::from_string(&id);
    let sink = state.live_sink(&id);
    tokio::spawn(async move {
        if let Err(error) = backend.resume(&session, &sink).await {
            let _ = sink.send(LiveChunk::notice(format!(
                "续跑没有起来（{}）：{}",
                error.code, error.message
            )));
        }
    });

    Ok((StatusCode::ACCEPTED, Json(json!({ "v": PROTOCOL, "ok": true, "resuming": true }))))
}

/// `POST /api/sessions/{id}/fork`: derive a new session from a closed turn.
///
/// Fork copies the source log up to a boundary, so the child id is needed in
/// the response; this one waits for the copy instead of returning 202.
async fn fork(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiFailure> {
    check_session(&state, &id)?;
    let body = json_body(body)?;
    check_version(&body)?;
    let Some(backend) = state.backend.clone() else {
        return Err(ApiFailure::unavailable("this build has no turn driver"));
    };
    let at = body.get("at").and_then(Value::as_str).map(str::to_string);
    let reason =
        body.get("reason").and_then(Value::as_str).unwrap_or("user fork").trim().to_string();
    let session = SessionId::from_string(&id);
    let child = backend
        .fork(&session, at.as_deref(), &reason)
        .await
        .map_err(|error| ApiFailure::internal(error.message))?;
    Ok(Json(json!({
        "v": PROTOCOL,
        "ok": true,
        "forked": child.is_some(),
        "session_id": child.as_ref().map(SessionId::as_str),
    })))
}

/// `POST /api/sessions/{id}/steer`: queue a sentence for the running turn.
async fn steer(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiFailure> {
    check_session(&state, &id)?;
    let body = json_body(body)?;
    check_version(&body)?;
    let text = body.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if text.is_empty() {
        return Err(ApiFailure::bad_request("text must not be empty"));
    }
    let Some(backend) = state.backend.clone() else {
        return Err(ApiFailure::unavailable("this build has no turn driver"));
    };

    let session = SessionId::from_string(&id);
    let sink = state.live_sink(&id);
    tokio::spawn(async move {
        if let Err(error) = backend.steer(&session, &text).await {
            let _ = sink.send(LiveChunk::notice(format!(
                "插话没有送进这一轮（{}）：{}",
                error.code, error.message
            )));
        }
    });

    Ok((StatusCode::ACCEPTED, Json(json!({ "v": PROTOCOL, "ok": true, "queued": true }))))
}

/// `POST /api/sessions/{id}/interrupt`: ask the turn in flight to stop.
///
/// The kernel decides whether there is anything to stop. The answer here is
/// optimistic for the same reason the command's own is: stopping is a queued
/// intent applied at the next step boundary, not an instant effect.
async fn interrupt(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiFailure> {
    check_session(&state, &id)?;
    // The interrupt endpoint has no required body; a stray `{}` is fine and an
    // absent body is fine too.
    if let Ok(Json(body)) = body {
        check_version(&body)?;
    }
    let Some(backend) = state.backend.clone() else {
        return Err(ApiFailure::unavailable("this build has no turn driver"));
    };

    let session = SessionId::from_string(&id);
    let sink = state.live_sink(&id);
    tokio::spawn(async move {
        match backend.interrupt(&session).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = sink.send(LiveChunk::notice("没有正在运行的回合可以打断".to_string()));
            }
            Err(error) => {
                let _ = sink.send(LiveChunk::notice(format!(
                    "打断没有送到内核（{}）：{}",
                    error.code, error.message
                )));
            }
        }
    });

    Ok((StatusCode::OK, Json(json!({ "v": PROTOCOL, "ok": true, "interrupting": true }))))
}

/// A handler's refusal, in the same envelope as the gate's.
struct ApiFailure {
    status: StatusCode,
    kind: &'static str,
    code: &'static str,
    message: String,
}

impl ApiFailure {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "bad_request",
            code: "bad_request",
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            code: "not_found",
            message: message.into(),
        }
    }

    /// No delegation is an answer about the session, not a broken server.
    fn subagents_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            code: "subagents_unavailable",
            message: message.into(),
        }
    }

    /// No plan is an answer about the session, not a broken server.
    fn plan_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            code: "plan_unavailable",
            message: message.into(),
        }
    }

    /// No prepared request is an answer about the session, not a broken server.
    fn context_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            code: "context_unavailable",
            message: message.into(),
        }
    }

    /// No observation is an answer about the session, not a broken server.
    fn diff_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "not_found",
            code: "diff_unavailable",
            message: message.into(),
        }
    }

    fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, kind: "busy", code, message: message.into() }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            kind: "refused",
            code: "no_backend",
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "internal",
            code: "internal",
            message: message.into(),
        }
    }

    fn version(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UPGRADE_REQUIRED,
            kind: "version",
            code: "version",
            message: message.into(),
        }
    }

    /// A chain that is not there is an answer (404); one that cannot be read is
    /// not (500). The two call for different next steps.
    fn why(error: WhyError) -> Self {
        match error {
            WhyError::Unavailable(message) => Self {
                status: StatusCode::NOT_FOUND,
                kind: "not_found",
                code: "why_unavailable",
                message,
            },
            other => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                kind: "internal",
                code: "why_failed",
                message: other.to_string(),
            },
        }
    }

    /// A read that could not be answered. Never "an empty list": the view would
    /// be shorter than the log, which is the one thing C2.5 forbids.
    fn read(error: sessions::ReadError) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            kind: "internal",
            code: "read_failed",
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({
                "v": PROTOCOL,
                "ok": false,
                "kind": self.kind,
                "code": self.code,
                "message": self.message,
            })),
        )
            .into_response()
    }
}

/// The two gates every request passes: where it came from, and who it is.
///
/// The host check is not CSRF protection by itself — a page on another origin
/// can still *send* a request — but combined with "no CORS headers" it means a
/// foreign page cannot read the answer, and a rebound DNS name cannot reach us.
async fn gate(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if !host_is_loopback(request.headers().get(header::HOST).and_then(|value| value.to_str().ok()))
    {
        return failure(FailureKind::Refused, "host_not_allowed", "只接受回环地址上的请求");
    }
    if request.uri().path().starts_with("/api/")
        && !has_token(
            &state.token,
            request.headers().get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()),
        )
    {
        return failure(FailureKind::Unauthorized, "unauthorized", "缺少或错误的令牌");
    }
    next.run(request).await
}

/// Whether a `Host` header names this machine.
///
/// Fail closed: a request without a host is not assumed local.
fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: the port follows the closing bracket.
        match rest.split_once(']') {
            Some((inside, _)) => format!("[{inside}]"),
            None => return false,
        }
    } else if let Some((name, _)) = host.rsplit_once(':') {
        name.to_string()
    } else {
        host.to_string()
    };
    matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// Whether the `Authorization` header carries the process's token.
fn has_token(expected: &str, header: Option<&str>) -> bool {
    let Some(value) = header else {
        return false;
    };
    value.strip_prefix("Bearer ").is_some_and(|token| token == expected)
}

fn failure(kind: FailureKind, code: &str, message: &str) -> Response {
    let status = match kind {
        FailureKind::Unauthorized => StatusCode::UNAUTHORIZED,
        FailureKind::Refused => StatusCode::FORBIDDEN,
    };
    (
        status,
        Json(serde_json::json!({
            "v": PROTOCOL,
            "ok": false,
            "kind": kind.as_str(),
            "code": code,
            "message": message,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_hosts_are_accepted() {
        for allowed in
            ["127.0.0.1", "127.0.0.1:7431", "localhost", "localhost:9", "[::1]", "[::1]:7431"]
        {
            assert!(host_is_loopback(Some(allowed)), "{allowed} should be local");
        }
        for refused in [
            "example.com",
            "example.com:443",
            "10.0.0.5:80",
            "127.0.0.1.evil.com",
            "localhost.evil.com",
            "",
        ] {
            assert!(!host_is_loopback(Some(refused)), "{refused} must not be local");
        }
        assert!(!host_is_loopback(None), "a missing Host is not local");
    }

    #[test]
    fn the_token_is_a_bearer_exactly_as_given() {
        assert!(has_token("abc", Some("Bearer abc")));
        assert!(!has_token("abc", Some("Bearer abd")));
        assert!(!has_token("abc", Some("abc")));
        assert!(!has_token("abc", Some("bearer abc")));
        assert!(!has_token("abc", None));
    }
}
