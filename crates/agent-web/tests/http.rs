//! The loopback surface: what needs the token, what does not, and what a wrong
//! host or path gets.
//!
//! The client here is the smallest thing that can speak HTTP/1.1 over a socket.
//! A TLS-capable client would pull a native crypto build into a crate whose
//! subject is a plaintext loopback listener, and the test would then be about
//! the machine's C toolchain.

use std::net::SocketAddr;

use agent_web::{Meta, Server};
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

/// Bind on an ephemeral port and start serving.
async fn serve() -> (SocketAddr, String, tokio::task::JoinHandle<()>) {
    let server = Server::bind(0, meta()).await.expect("bind loopback");
    let addr = server.addr();
    let token = server.token().to_string();
    let handle = tokio::spawn(async move {
        let _ = server.serve().await;
    });
    (addr, token, handle)
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

/// The body half of a response.
fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or(response)
}

#[tokio::test]
async fn meta_requires_the_process_token() {
    let (addr, token, handle) = serve().await;

    let (status, response) = request(addr, "/api/meta", None, None).await;
    assert_eq!(status, 401, "{response}");
    assert!(body(&response).contains("\"code\":\"unauthorized\""), "{response}");

    let (status, response) = request(addr, "/api/meta", Some(&token), None).await;
    assert_eq!(status, 200, "{response}");
    let payload = body(&response);
    assert!(payload.contains("\"v\":1"), "{payload}");
    assert!(payload.contains("\"workspace\":\"yaoyao-public\""), "{payload}");
    assert!(payload.contains("\"model\":\"deepseek-flash\""), "{payload}");
    handle.abort();
}

#[tokio::test]
async fn the_page_is_public_and_names_the_product() {
    let (addr, _, handle) = serve().await;
    let (status, response) = request(addr, "/", None, None).await;
    assert_eq!(status, 200, "{response}");
    let page = body(&response);
    assert!(page.contains("爻爻"), "the page names the product");
    assert!(page.contains("/api/meta"), "and the endpoint it talks to");
    handle.abort();
}

#[tokio::test]
async fn a_foreign_host_is_refused_before_the_token_is_read() {
    let (addr, token, handle) = serve().await;
    let (status, response) = request(addr, "/api/meta", Some(&token), Some("example.com")).await;
    assert_eq!(status, 403, "{response}");
    assert!(body(&response).contains("\"code\":\"host_not_allowed\""), "{response}");
    handle.abort();
}

#[tokio::test]
async fn an_unknown_api_path_is_not_found() {
    let (addr, token, handle) = serve().await;
    let (status, _) = request(addr, "/api/nope", Some(&token), None).await;
    assert_eq!(status, 404);
    handle.abort();
}
