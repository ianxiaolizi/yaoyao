//! agent-web: the loopback HTTP surface.
//!
//! This crate is a front door, not a second agent. It binds `127.0.0.1`, serves
//! the embedded UI, and answers `/api/meta`; every later endpoint will call into
//! the command layer rather than reimplement it. The rules it keeps are the ones
//! the contract makes non-negotiable: loopback only, a bearer token on every API
//! call, and a version on every object.
//!
//! The token is generated per process and printed once as part of the URL. It
//! lives in this process's memory only — not in the log, not in the UI's storage.
//! The page reads it from the URL fragment, which the browser does not send to a
//! server, and puts it in an `Authorization` header.

#![deny(unsafe_code)]

mod sessions;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use agent_cli::reason::WhyError;
use agent_ports::event_store::EventStore;
use agent_protocol::ids::SessionId;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tokio::net::TcpListener;

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
}

/// The state every request shares.
struct AppState {
    token: String,
    meta: Meta,
    store: Arc<dyn EventStore>,
    data_dir: PathBuf,
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
        let Config { port, meta, store, data_dir } = config;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|source| WebError::Bind { port, source })?;
        let addr = listener.local_addr().map_err(|source| WebError::Bind { port, source })?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let app = router(Arc::new(AppState { token: token.clone(), meta, store, data_dir }));
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
        .route("/api/meta", get(meta))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}/transcript", get(transcript))
        .route("/api/sessions/{id}/why", get(why))
        .layer(middleware::from_fn_with_state(state.clone(), gate))
        .with_state(state)
}

/// The placeholder page.
///
/// Replaced by the real UI in a later step; until then it is the smallest thing
/// that proves the whole path — page, token, header, JSON — works in a browser.
async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
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
        "capabilities": state.meta.capabilities,
    }))
}

/// `GET /api/sessions`: every session under the data directory, newest first.
async fn sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    let list =
        sessions::list(state.store.as_ref(), &state.data_dir).await.map_err(ApiFailure::read)?;
    Ok(Json(serde_json::json!({ "v": PROTOCOL, "sessions": list })))
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
    let payload = sessions::transcript(state.store.as_ref(), &SessionId::from_string(&id), since)
        .await
        .map_err(ApiFailure::read)?;
    Ok(Json(payload))
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
