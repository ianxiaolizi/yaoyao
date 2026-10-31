//! The `web.request` tool: one HTTP call, bounded in where it may go, how long
//! it may take, and how much it may bring back.
//!
//! A page is read with one tool call here, not with a download followed by a
//! shell command that reads it: the airbench run that motivated this tool
//! submitted a font file with `fs.write` plus `shell.run`, two steps for one
//! act, and the second one could fail on its own.
//!
//! Everything the model chooses is checked. The scheme is http or https, the
//! host resolves to public addresses (see [`crate::guard`]), each redirect hop
//! is checked again with the same rules, the body stops at
//! [`MAX_RESPONSE_BYTES`], and the call has a deadline the definition states so
//! the model knows what it is racing.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use agent_ports::tool_provider::{
    ToolDefinition, ToolInvocation, ToolProvider, ToolProviderError, ToolResult, ToolSideEffect,
    ToolSource,
};
use agent_protocol::permissions::Capability;
use agent_protocol::tool::ToolErrorKind;
use futures_util::StreamExt;
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, LOCATION};

use crate::guard::{self, Addresses, GuardError};

/// The tool's name on the wire.
pub const WEB_TOOL_NAME: &str = "web.request";

/// How long one call may take when the model states no deadline of its own.
pub const WEB_TIMEOUT_MS: u64 = 30_000;

/// The longest deadline a call may state. A font scan that needs two minutes
/// says so; nothing waits forever.
pub const MAX_WEB_TIMEOUT_MS: u64 = 120_000;

/// How much of a response body one call keeps.
pub const MAX_RESPONSE_BYTES: usize = 1_000_000;

/// How many redirect hops one call follows.
const MAX_REDIRECTS: usize = 5;

/// How much of a response header value the tool quotes back.
const MAX_HEADER_CHARS: usize = 200;

/// Methods a call may state. Anything else — CONNECT, TRACE — is not something
/// a model asking to read a page means, and is refused rather than forwarded.
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// The one HTTP tool, with its guards.
pub struct WebToolProvider {
    addresses: Addresses,
    max_bytes: usize,
    timeout: Duration,
}

impl Default for WebToolProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl WebToolProvider {
    /// The tool as the assembled binary offers it: public addresses only.
    pub fn new() -> Self {
        Self {
            addresses: Addresses::public_only(),
            max_bytes: MAX_RESPONSE_BYTES,
            timeout: Duration::from_millis(WEB_TIMEOUT_MS),
        }
    }

    /// The tool as a fixture offers it: any address, so a test server on
    /// loopback is reachable. Named so the wiring says which one it built.
    pub fn allowing_local_addresses() -> Self {
        Self { addresses: Addresses::any(), ..Self::new() }
    }

    /// Keep at most this many body bytes.
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Let a call without a stated deadline wait this long.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn required_str<'a>(
        invocation: &'a ToolInvocation,
        key: &str,
    ) -> Result<&'a str, ToolProviderError> {
        invocation
            .arguments
            .get(key)
            .and_then(|value| value.as_str())
            .ok_or_else(|| ToolProviderError::failed(WEB_TOOL_NAME, format!("missing '{key}'")))
    }

    /// The request as the arguments describe it, checked.
    fn request(&self, invocation: &ToolInvocation) -> Result<Request, ToolProviderError> {
        let method = match invocation.arguments.get("method").and_then(|value| value.as_str()) {
            Some(method) => method.to_ascii_uppercase(),
            None => "GET".to_string(),
        };
        if !METHODS.contains(&method.as_str()) {
            return Err(ToolProviderError::failed(
                WEB_TOOL_NAME,
                format!("unsupported method '{method}'; use one of {}", METHODS.join(", ")),
            ));
        }
        let headers = match invocation.arguments.get("headers") {
            None => Vec::new(),
            Some(serde_json::Value::Object(map)) => {
                let mut headers = Vec::with_capacity(map.len());
                for (name, value) in map {
                    let Some(value) = value.as_str() else {
                        return Err(ToolProviderError::failed(
                            WEB_TOOL_NAME,
                            format!("header '{name}' is not a string"),
                        ));
                    };
                    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                        ToolProviderError::failed(WEB_TOOL_NAME, format!("invalid header '{name}'"))
                    })?;
                    // A header value is one line: a newline in one would let a
                    // model add a header of its own choosing past the check
                    // above.
                    let value = HeaderValue::from_str(value).map_err(|_| {
                        ToolProviderError::failed(
                            WEB_TOOL_NAME,
                            format!("header '{name}' has a value that is not a valid header"),
                        )
                    })?;
                    headers.push((name, value));
                }
                headers
            }
            Some(_) => {
                return Err(ToolProviderError::failed(
                    WEB_TOOL_NAME,
                    "'headers' must be an object",
                ));
            }
        };
        let body = match invocation.arguments.get("body") {
            None => None,
            Some(serde_json::Value::String(body)) => Some(body.clone()),
            Some(_) => {
                return Err(ToolProviderError::failed(WEB_TOOL_NAME, "'body' must be a string"));
            }
        };
        let timeout = match invocation.arguments.get("timeout_ms") {
            None => self.timeout,
            Some(value) => {
                let stated = value.as_u64().ok_or_else(|| {
                    ToolProviderError::failed(WEB_TOOL_NAME, "'timeout_ms' must be an integer")
                })?;
                Duration::from_millis(stated.clamp(1, MAX_WEB_TIMEOUT_MS))
            }
        };
        Ok(Request {
            url: Self::required_str(invocation, "url")?.to_string(),
            method,
            headers,
            body,
            timeout,
        })
    }

    /// Send one hop, with the client pinned to the addresses the guard checked.
    #[allow(clippy::too_many_arguments)]
    async fn hop(
        &self,
        request: &Request,
        method: &str,
        url: &Url,
        host: &str,
        addresses: &[IpAddr],
        port: u16,
        body: Option<&str>,
    ) -> Result<reqwest::Response, ToolProviderError> {
        let mut builder = reqwest::Client::builder()
            // Redirects are followed here, one hop at a time, because each
            // hop is a new URL the model did not write and the guard has to
            // see it before the client does.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(request.timeout)
            .timeout(request.timeout);
        // Pinned to what was checked: the client cannot resolve the name again
        // and land somewhere the guard never saw.
        for address in addresses {
            builder = builder.resolve(host, SocketAddr::new(*address, port));
        }
        let client = builder.build().map_err(|error| {
            ToolProviderError::classified(
                ToolErrorKind::Unavailable,
                WEB_TOOL_NAME,
                error.to_string(),
            )
        })?;

        let verb = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| {
            ToolProviderError::failed(WEB_TOOL_NAME, format!("unsupported method '{method}'"))
        })?;
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            headers.insert(name.clone(), value.clone());
        }
        let mut outgoing = client.request(verb, url.clone()).headers(headers);
        if let Some(body) = body {
            outgoing = outgoing.body(body.to_string());
        }
        outgoing.send().await.map_err(|error| {
            if error.is_timeout() {
                ToolProviderError::classified(
                    ToolErrorKind::Timeout,
                    WEB_TOOL_NAME,
                    format!("no answer within {} ms", request.timeout.as_millis()),
                )
            } else {
                ToolProviderError::classified(
                    ToolErrorKind::Unavailable,
                    WEB_TOOL_NAME,
                    error.to_string(),
                )
            }
        })
    }
}

/// One call, checked but not sent.
struct Request {
    url: String,
    method: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Option<String>,
    timeout: Duration,
}

#[async_trait::async_trait]
impl ToolProvider for WebToolProvider {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: WEB_TOOL_NAME.to_string(),
            description: "Fetch a URL over HTTP(S) and return the status, the content type and \
                          the body text. `url` must be an absolute http or https URL to a public \
                          host; loopback, private and link-local addresses are refused. `method` \
                          defaults to GET; `headers` and `body` are sent as given. \
                          `timeout_ms` bounds the whole call (default 30000, at most 120000). \
                          The body is cut at 1000000 bytes and the result says so. Use this to \
                          read a page or an API response in one call, instead of downloading it \
                          with one tool and reading it with another."
                .to_string(),
            source: ToolSource::Builtin,
            side_effect: ToolSideEffect::Network,
            capability_requirements: vec![Capability::new(Capability::NETWORK_REQUEST)],
            // A GET is safe to repeat; a POST is not, and the class is one
            // number for every call this tool can make.
            idempotent: false,
            timeout_ms: Some(WEB_TIMEOUT_MS),
            input_schema: serde_json::json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string" },
                    "method": {
                        "type": "string",
                        "enum": METHODS
                    },
                    "headers": { "type": "object" },
                    "body": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_WEB_TIMEOUT_MS }
                }
            }),
        }]
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        (tool_name == WEB_TOOL_NAME).then_some(ToolSideEffect::Network)
    }

    /// No argument of this tool names a workspace path: the workspace
    /// boundary has nothing to check here, and the address guard is what
    /// bounds the call instead.
    fn path_arguments(&self, tool_name: &str) -> Vec<String> {
        let _ = tool_name;
        Vec::new()
    }

    async fn invoke(&self, invocation: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        if invocation.tool_name != WEB_TOOL_NAME {
            return Err(ToolProviderError::UnknownTool {
                tool: invocation.tool_name.clone(),
                available: vec![WEB_TOOL_NAME.to_string()],
            });
        }
        let request = self.request(invocation)?;
        let mut url = request.url.clone();
        let mut method = request.method.clone();
        let mut body = request.body.clone();

        let mut redirects = 0;
        loop {
            let target = guard::parse(&url).map_err(refused)?;
            let addresses = guard::resolve(&target, self.addresses).await.map_err(refused)?;
            let response = self
                .hop(
                    &request,
                    &method,
                    &target.url,
                    &target.host,
                    &addresses,
                    target.port,
                    body.as_deref(),
                )
                .await?;

            let status = response.status();
            if status.is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(ToolProviderError::failed(
                        WEB_TOOL_NAME,
                        format!("more than {MAX_REDIRECTS} redirects"),
                    ));
                }
                let location = location.to_str().map_err(|_| {
                    ToolProviderError::classified(
                        ToolErrorKind::Protocol,
                        WEB_TOOL_NAME,
                        "the redirect location is not readable text".to_string(),
                    )
                })?;
                let next = target.url.join(location).map_err(|error| {
                    ToolProviderError::classified(
                        ToolErrorKind::Protocol,
                        WEB_TOOL_NAME,
                        format!("the redirect location is not a URL: {error}"),
                    )
                })?;
                // The method follows the redirect the way browsers do: 303 asks
                // for a GET, and so do 301/302 (the historically universal
                // reading); 307/308 keep the method and the body.
                if status == reqwest::StatusCode::SEE_OTHER
                    || status == reqwest::StatusCode::MOVED_PERMANENTLY
                    || status == reqwest::StatusCode::FOUND
                {
                    method = "GET".to_string();
                    body = None;
                }
                url = next.to_string();
                continue;
            }

            return self.render(&url, status, response).await;
        }
    }
}

impl WebToolProvider {
    /// What the model reads: the status line, the headers that say what the
    /// body is, and the body itself — cut at the cap with the cut named.
    async fn render(
        &self,
        url: &str,
        status: reqwest::StatusCode,
        response: reqwest::Response,
    ) -> Result<ToolResult, ToolProviderError> {
        let mut head = format!(
            "{WEB_TOOL_NAME} {url}\nHTTP {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        );
        for name in ["content-type", "content-length", "content-encoding"] {
            if let Some(value) = response.headers().get(name) {
                let value = value.to_str().unwrap_or("<binary>");
                head.push_str(&format!("\n{name}: {}", truncate(value, MAX_HEADER_CHARS)));
            }
        }

        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        let mut truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                if error.is_timeout() {
                    ToolProviderError::classified(
                        ToolErrorKind::Timeout,
                        WEB_TOOL_NAME,
                        "the body did not finish arriving in time".to_string(),
                    )
                } else {
                    ToolProviderError::classified(
                        ToolErrorKind::Unavailable,
                        WEB_TOOL_NAME,
                        error.to_string(),
                    )
                }
            })?;
            let room = self.max_bytes.saturating_sub(body.len());
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }

        let text = String::from_utf8_lossy(&body);
        let content = if truncated {
            format!(
                "{head}\n\nbody: {bytes} bytes kept, cut at {}\n\n{text}",
                self.max_bytes,
                bytes = body.len()
            )
        } else {
            format!("{head}\n\n{text}")
        };
        // An HTTP error status is still an answer: the body that came with it
        // is what the model needs to read, so the failure carries the same
        // content a success would.
        let result = if status.is_success() {
            ToolResult::completed(content)
        } else {
            ToolResult::failed(content)
        };
        Ok(if truncated { result.truncated() } else { result })
    }
}

/// A guard refusal is the call's own answer: the request was never sent.
fn refused(error: GuardError) -> ToolProviderError {
    ToolProviderError::failed(WEB_TOOL_NAME, format!("refused: {error}"))
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}
