//! `web.request` against a real listener.
//!
//! Every test here starts a server on loopback and calls it through the tool.
//! The guard is what the tool is for, so the tests that matter most are the
//! ones where it says no — loopback under the assembled policy, a redirect to
//! another scheme, a header value that would smuggle a second header — and the
//! rest are the ordinary reads that must still work.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use agent_ports::tool_provider::{ToolInvocation, ToolProvider, ToolProviderError};
use agent_protocol::tool::ToolErrorKind;
use agent_tools_web::{NetworkAllowlist, WEB_TOOL_NAME, WebToolProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

struct Seen {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn answer(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut text = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n", body.len());
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    let mut bytes = text.into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// A server on loopback whose answers depend on the path, as the tests name
/// them. It runs until the test process ends; the listener is dropped with the
/// runtime.
async fn serve() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
    let address = listener.local_addr().expect("a bound address");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let seen = read_request(&mut stream).await;
                let bytes = match seen.path.as_str() {
                    "/ok" => {
                        answer("200 OK", &[("content-type", "text/plain")], "hello from the server")
                    }
                    "/big" => {
                        answer("200 OK", &[("content-type", "text/plain")], &"x".repeat(4_000))
                    }
                    "/echo" => answer(
                        "200 OK",
                        &[("content-type", "text/plain")],
                        &format!(
                            "{} {} body={} test={}",
                            seen.method,
                            seen.path,
                            seen.body,
                            seen.header("x-test").unwrap_or("<none>")
                        ),
                    ),
                    "/redir" => answer("302 Found", &[("location", "/ok")], ""),
                    // A hop to an origin the list does not name: the port is
                    // the only difference, and the allowlist compares ports.
                    "/elsewhere" => {
                        answer("302 Found", &[("location", "http://127.0.0.1:1/x")], "")
                    }
                    "/outward" => answer("302 Found", &[("location", "file:///etc/passwd")], ""),
                    "/loop" => answer("302 Found", &[("location", "/loop")], ""),
                    "/slow" => {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        answer("200 OK", &[], "late")
                    }
                    "/slow-redirect-one" => {
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        answer("302 Found", &[("location", "/slow-redirect-two")], "")
                    }
                    "/slow-redirect-two" => {
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        answer("200 OK", &[], "late but finished")
                    }
                    "/missing" => {
                        answer("404 Not Found", &[("content-type", "text/plain")], "no such page")
                    }
                    _ => answer("404 Not Found", &[], "unrouted"),
                };
                let _ = stream.write_all(&bytes).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    address
}

async fn read_request(stream: &mut TcpStream) -> Seen {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.expect("a read");
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = find(&buffer, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_string();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0usize);
        if buffer.len() >= end + 4 + length {
            break;
        }
    }
    let end = find(&buffer, b"\r\n\r\n").expect("a complete request head");
    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    let body = String::from_utf8_lossy(&buffer[end + 4..]).to_string();
    Seen { method, path, headers, body }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn call(arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation::proposed(WEB_TOOL_NAME, arguments)
}

fn local() -> WebToolProvider {
    WebToolProvider::allowing_local_addresses()
}

#[tokio::test]
async fn a_get_returns_the_status_the_type_and_the_body() {
    let address = serve().await;
    let provider = local();
    let result = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/ok") })))
        .await
        .expect("an answer");
    assert!(result.content.starts_with("web.request "), "{}", result.content);
    assert!(result.content.contains("HTTP 200 OK"), "{}", result.content);
    assert!(result.content.contains("content-type: text/plain"), "{}", result.content);
    assert!(result.content.ends_with("hello from the server"), "{}", result.content);
    assert!(!result.truncated, "a short body is not cut");
}

#[tokio::test]
async fn a_get_sends_the_headers_it_was_given() {
    let address = serve().await;
    let provider = local();
    let result = provider
        .invoke(&call(serde_json::json!({
            "url": format!("http://{address}/echo"),
            "headers": { "x-test": "one" },
        })))
        .await
        .expect("an answer");
    assert!(result.content.contains("GET /echo body= test=one"), "{}", result.content);
}

#[tokio::test]
async fn a_post_request_is_rejected_in_phase_b() {
    // The writing methods are off the surface entirely, not merely gated: a
    // POST is refused by the tool, so nothing downstream has to decide what a
    // write to the internet means (CONTRACT §7 #180).
    let address = serve().await;
    let provider = local();
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let error = provider
            .invoke(&call(serde_json::json!({
                "url": format!("http://{address}/echo"),
                "method": method,
            })))
            .await
            .expect_err("this tool reads only");
        let ToolProviderError::Failed { message, .. } = &error else {
            panic!("a refused method is the call's answer: {error}");
        };
        assert!(message.contains("GET, HEAD"), "{method}: {message}");
    }
}

#[tokio::test]
async fn a_request_body_is_refused_rather_than_dropped() {
    // A dropped body would make a model believe it sent something it did not.
    let address = serve().await;
    let provider = local();
    let error = provider
        .invoke(&call(serde_json::json!({
            "url": format!("http://{address}/echo"),
            "body": "payload",
        })))
        .await
        .expect_err("a read carries no body");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("an argument this tool cannot send is the call's answer: {error}");
    };
    assert!(message.contains("no request body"), "{message}");
}

#[tokio::test]
async fn a_request_outside_the_allowlist_is_denied_before_dns() {
    // `.invalid` never resolves, so a call that got as far as DNS would fail
    // with "does not resolve" instead. The allowlist check comes first.
    let provider = WebToolProvider::allowing_local_addresses().with_allowlist(
        NetworkAllowlist::from_origins_allowing_loopback(&["https://docs.rs".to_string()])
            .expect("a list"),
    );
    let error = provider
        .invoke(&call(serde_json::json!({ "url": "https://unlisted.invalid/page" })))
        .await
        .expect_err("an unlisted site is not reachable");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("an allowlist refusal is the call's answer: {error}");
    };
    assert!(message.contains("not in the configured network allowlist"), "{message}");
    assert!(!message.contains("does not resolve"), "the refusal came before DNS: {message}");
}

#[tokio::test]
async fn a_redirect_outside_the_allowlist_is_refused() {
    let address = serve().await;
    // Only the test server's own origin is named; the hop it redirects to is a
    // different port, so it is a different origin.
    let provider = WebToolProvider::allowing_local_addresses().with_allowlist(
        NetworkAllowlist::from_origins_allowing_loopback(&[format!("http://{address}")])
            .expect("a list"),
    );
    let error = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/elsewhere") })))
        .await
        .expect_err("a redirect cannot leave the list");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("a refused hop is the call's answer: {error}");
    };
    assert!(message.contains("not in the configured network allowlist"), "{message}");
}

#[tokio::test]
async fn a_web_result_is_marked_untrusted() {
    let address = serve().await;
    let provider = local();
    let result = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/ok") })))
        .await
        .expect("an answer");
    assert_eq!(
        result.trust,
        agent_protocol::tool::TrustLevel::Untrusted,
        "a page was written outside this session"
    );

    // A failed call carries a page too — an error status still brings a body.
    let failed = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/missing") })))
        .await
        .expect("an answer");
    assert_eq!(failed.trust, agent_protocol::tool::TrustLevel::Untrusted);
}

#[tokio::test]
async fn a_long_body_is_cut_at_the_cap_and_says_so() {
    let address = serve().await;
    let provider = local().with_max_bytes(64);
    let result = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/big") })))
        .await
        .expect("an answer");
    assert!(result.truncated, "the cut is stated on the result");
    assert!(result.content.contains("body: 64 bytes kept, cut at 64"), "{}", result.content);
    assert!(!result.content.contains(&"x".repeat(100)), "nothing past the cap reaches the model");
}

#[tokio::test]
async fn an_error_status_is_still_an_answer_with_its_body() {
    let address = serve().await;
    let provider = local();
    let result = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/missing") })))
        .await
        .expect("an answer");
    assert_eq!(result.status, agent_protocol::tool::ToolStatus::Failed);
    assert!(result.content.contains("HTTP 404 Not Found"), "{}", result.content);
    assert!(result.content.contains("no such page"), "{}", result.content);
}

#[tokio::test]
async fn loopback_is_refused_before_the_request_is_sent() {
    let address = serve().await;
    let provider = WebToolProvider::new();
    let error = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/ok") })))
        .await
        .expect_err("loopback is not a public address");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("a guard refusal is the call's answer, not a classified transport failure: {error}");
    };
    assert!(message.contains("refused:"), "{message}");
    assert!(message.contains("127.0.0.1"), "{message}");
}

#[tokio::test]
async fn a_redirect_is_followed_to_its_final_answer() {
    let address = serve().await;
    let provider = local();
    let result = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/redir") })))
        .await
        .expect("an answer");
    assert!(result.content.contains("HTTP 200 OK"), "{}", result.content);
    assert!(result.content.contains("hello from the server"), "{}", result.content);
}

#[tokio::test]
async fn every_redirect_hop_is_checked_again() {
    let address = serve().await;
    let provider = local();
    let error = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/outward") })))
        .await
        .expect_err("a redirect to another scheme is not a target");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("a refused hop is the call's answer: {error}");
    };
    assert!(message.contains("only http and https"), "{message}");
}

#[tokio::test]
async fn a_redirect_loop_stops_at_the_hop_limit() {
    let address = serve().await;
    let provider = local();
    let error = provider
        .invoke(&call(serde_json::json!({ "url": format!("http://{address}/loop") })))
        .await
        .expect_err("a loop is not an answer");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("a call that never ends is the call's answer: {error}");
    };
    assert!(message.contains("more than 5 redirects"), "{message}");
}

#[tokio::test]
async fn the_call_states_its_own_deadline() {
    let address = serve().await;
    let provider = local();
    let started = Instant::now();
    let error = provider
        .invoke(&call(serde_json::json!({
            "url": format!("http://{address}/slow"),
            "timeout_ms": 100,
        })))
        .await
        .expect_err("the server is slower than the deadline");
    assert!(
        matches!(error, ToolProviderError::Classified { kind: ToolErrorKind::Timeout, .. }),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the deadline ended the call, not the server"
    );
}

#[tokio::test]
async fn a_redirect_chain_shares_one_deadline() {
    let address = serve().await;
    let provider = local();
    let started = Instant::now();
    let error = provider
        .invoke(&call(serde_json::json!({
            "url": format!("http://{address}/slow-redirect-one"),
            "timeout_ms": 180,
        })))
        .await
        .expect_err("the whole redirect chain is slower than one call deadline");
    assert!(
        matches!(error, ToolProviderError::Classified { kind: ToolErrorKind::Timeout, .. }),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the deadline belongs to the whole call, not one hop"
    );
}

#[tokio::test]
async fn a_header_value_cannot_carry_a_second_header() {
    let provider = local();
    let error = provider
        .invoke(&call(serde_json::json!({
            "url": "http://example.com/",
            "headers": { "x-test": "one\r\nx-injected: two" },
        })))
        .await
        .expect_err("a header value is one line");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("an argument this tool cannot send is the call's answer: {error}");
    };
    assert!(message.contains("not a valid header"), "{message}");
}

#[tokio::test]
async fn an_unknown_method_is_refused() {
    let provider = local();
    let error = provider
        .invoke(&call(serde_json::json!({ "url": "http://example.com/", "method": "TRACE" })))
        .await
        .expect_err("TRACE is not a read");
    let ToolProviderError::Failed { message, .. } = &error else {
        panic!("an unsupported method is the call's answer: {error}");
    };
    assert!(message.contains("unsupported method"), "{message}");
}

#[tokio::test]
async fn the_definition_names_the_deadline_and_the_cap() {
    let provider = local();
    let definitions = provider.definitions();
    assert_eq!(definitions.len(), 1);
    let definition = &definitions[0];
    assert_eq!(definition.name, WEB_TOOL_NAME);
    assert_eq!(definition.timeout_ms, Some(agent_tools_web::WEB_TIMEOUT_MS));
    assert!(definition.description.contains("120000"), "{}", definition.description);
    assert!(definition.description.contains("1000000"), "{}", definition.description);
    assert!(definition.description.contains("allowlist"), "{}", definition.description);
    assert!(definition.description.contains("untrusted"), "{}", definition.description);
    assert!(definition.idempotent, "both methods this tool can state are reads");
    let schema = &definition.input_schema;
    assert_eq!(schema["properties"]["method"]["enum"], serde_json::json!(["GET", "HEAD"]));
    assert!(
        schema["properties"].get("body").is_none(),
        "a read-only tool does not offer an outbound body: {schema}"
    );
    assert_eq!(
        definition.capability_requirements.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        vec!["network.request"]
    );
}
