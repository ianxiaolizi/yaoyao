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

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
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

/// The state every request shares.
struct AppState {
    token: String,
    meta: Meta,
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
    /// `port` 0 lets the operating system pick, which is what tests use. The
    /// address is never `0.0.0.0`: a GUI that answers the network is a different
    /// product with a different threat model.
    pub async fn bind(port: u16, meta: Meta) -> Result<Self, WebError> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|source| WebError::Bind { port, source })?;
        let addr = listener.local_addr().map_err(|source| WebError::Bind { port, source })?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let app = router(Arc::new(AppState { token: token.clone(), meta }));
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
