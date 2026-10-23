//! A stand-in MCP server that speaks **Streamable HTTP**, for tests that need a
//! real socket, a real session, and a real SSE stream.
//!
//! It is a process, not an in-process double, for the same reason the stdio
//! fixture is one: the part of the adapter most likely to be wrong is the part
//! that speaks the transport, and a fake that replaced the socket would test
//! everything except that. It is also hand-rolled rather than built on `rmcp`'s
//! server half, so it cannot agree with the client by sharing its bugs.
//!
//! What it implements is the slice of the Streamable HTTP contract this
//! adapter's client actually depends on, written down here as facts about the
//! wire rather than read out of the SDK:
//!
//! ```text
//! POST /mcp   initialize                -> 200, application/json, `Mcp-Session-Id` set
//! POST /mcp   notifications/initialized -> 202 Accepted, empty  (any 2xx empty body will do)
//! POST /mcp   tools/list, tools/call    -> 200, application/json
//! GET  /mcp                             -> 200, text/event-stream, held open
//! DELETE /mcp                           -> 200                   (the server may also 405)
//! ```
//!
//! Three rules it enforces, each of which is a claim about the client that a
//! test can therefore check by consequence rather than by inspection:
//!
//!   * **every request must carry the configured header** (default
//!     `x-yaoyao-test: present`); without it the answer is 403. This is how the
//!     adapter's `headers` configuration is verified to arrive on the wire
//!     rather than merely to be parsed.
//!   * **`Accept` must name both media types** on POST and GET, which the
//!     Streamable HTTP spec requires of a client and this fixture therefore
//!     refuses with 406. It is the spec's requirement, not this fixture's
//!     invention, and a client that sent only `application/json` would be
//!     wrong in a way a permissive fixture would hide.
//!   * **a request naming a session must name a live one**, or it gets 404 —
//!     which is how a server reports an expired session, and how the client's
//!     re-initialization path is reached.
//!
//! Anything but `/mcp` is a 404, and the path is spelled out here rather than
//! imported from the adapter on purpose: an earlier fixture in this workspace
//! ignored the path, and the adapter posting to the wrong one therefore passed
//! every test while a live server answered 404. A fixture that agrees with the
//! code it tests by construction cannot catch that.
//!
//! The tool set carries the server's own state out to the test, so that what the
//! *client* did is observable from the client's side of the wire:
//!
//! ```text
//! echo        the argument, back
//! grow        add a tool and emit `notifications/tools/list_changed` on the stream
//! disconnect  drop the event stream the caller is holding
//! expire      forget the caller's session, so its next request is a 404
//! stats       the counters and the headers this server has actually seen
//! ```

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// The one path this server serves. Spelled out, as above.
const PATH: &str = "/mcp";

/// The session header, by the name the spec gives it.
const HEADER_SESSION: &str = "mcp-session-id";

/// The header a request must carry to be served, and its value.
struct Required {
    name: String,
    value: String,
}

/// What the last request carried, so a test can read back what arrived.
#[derive(Default, Clone)]
struct Observed {
    header: String,
    accept: String,
}

struct State {
    required: Required,
    grown: AtomicBool,
    next_session: AtomicUsize,
    sessions: Mutex<HashSet<String>>,
    inits: AtomicUsize,
    gets: AtomicUsize,
    posts: AtomicUsize,
    deletes: AtomicUsize,
    last: Mutex<Observed>,
    events: broadcast::Sender<String>,
    /// Fires to drop the event stream a client is currently holding.
    closes: broadcast::Sender<()>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();

    let port = flag(&argv, "--port").and_then(|value| value.parse::<u16>().ok()).unwrap_or(0);
    let required = flag(&argv, "--require-header")
        .and_then(|spec| {
            spec.split_once('=').map(|(name, value)| Required {
                name: name.to_ascii_lowercase(),
                value: value.to_string(),
            })
        })
        .unwrap_or_else(|| Required {
            name: "x-yaoyao-test".to_string(),
            value: "present".to_string(),
        });

    let (events, _) = broadcast::channel(32);
    let (closes, _) = broadcast::channel(16);
    let state = Arc::new(State {
        required,
        grown: AtomicBool::new(false),
        next_session: AtomicUsize::new(0),
        sessions: Mutex::new(HashSet::new()),
        inits: AtomicUsize::new(0),
        gets: AtomicUsize::new(0),
        posts: AtomicUsize::new(0),
        deletes: AtomicUsize::new(0),
        last: Mutex::new(Observed::default()),
        events,
        closes,
    });

    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind loopback");
    println!("listening {}", listener.local_addr().expect("a bound socket has an address"));
    std::io::stdout().flush().expect("stdout");

    loop {
        let (socket, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _ = serve(socket, state).await;
        });
    }
}

/// `--name value`, or the value it was given.
fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter().position(|arg| arg == name).and_then(|index| argv.get(index + 1).cloned())
}

async fn serve(mut socket: TcpStream, state: Arc<State>) -> std::io::Result<()> {
    let Some((head, body)) = read_request(&mut socket).await? else {
        return Ok(());
    };

    let method = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_default()
        .to_string();

    if path_of(&head).as_deref() != Some(PATH) {
        return send(&mut socket, 404, "Not Found", "text/plain", "no such resource\n", &[]).await;
    }

    // What arrived, recorded before anything decides what to do with it, so a
    // refusal still leaves the evidence of what was sent.
    {
        let mut last = state.last.lock().expect("the observed lock is never poisoned");
        last.header = header(&head, &state.required.name).unwrap_or_default();
        last.accept = header(&head, "accept").unwrap_or_default();
    }

    if header(&head, &state.required.name).as_deref() != Some(state.required.value.as_str()) {
        let refusal = json!({
            "error": format!(
                "this server requires the header '{}: {}' on every request",
                state.required.name, state.required.value
            )
        });
        return send(
            &mut socket,
            403,
            "Forbidden",
            "application/json",
            &format!("{refusal}\n"),
            &[],
        )
        .await;
    }

    match method.as_str() {
        "POST" => post(socket, state, &head, &body).await,
        "GET" => stream(socket, state, &head).await,
        "DELETE" => {
            state.deletes.fetch_add(1, Ordering::SeqCst);
            if let Some(id) = header(&head, HEADER_SESSION) {
                state.sessions.lock().expect("sessions lock").remove(&id);
            }
            send(&mut socket, 200, "OK", "application/json", "", &[]).await
        }
        _ => send(&mut socket, 405, "Method Not Allowed", "text/plain", "", &[]).await,
    }
}

async fn post(
    mut socket: TcpStream,
    state: Arc<State>,
    head: &str,
    body: &[u8],
) -> std::io::Result<()> {
    if !accepts_both(head) {
        return refuse_accept(&mut socket).await;
    }
    state.posts.fetch_add(1, Ordering::SeqCst);

    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return send(
            &mut socket,
            400,
            "Bad Request",
            "application/json",
            "{\"error\":\"not JSON-RPC\"}\n",
            &[],
        )
        .await;
    };

    let method = message.get("method").and_then(Value::as_str).unwrap_or_default().to_string();
    let id = message.get("id").cloned();
    let session = header(head, HEADER_SESSION);

    // `initialize` is how a session is made; everything else must name a live
    // one. A 404 here is the server saying "that session is gone", which is the
    // one thing the client has a defined recovery from.
    if method != "initialize" {
        let live = session
            .as_ref()
            .is_some_and(|id| state.sessions.lock().expect("sessions lock").contains(id));
        if !live {
            return send(&mut socket, 404, "Not Found", "application/json", "", &[]).await;
        }
    }

    match method.as_str() {
        "initialize" => {
            let session = format!("sess-{}", state.next_session.fetch_add(1, Ordering::SeqCst) + 1);
            state.sessions.lock().expect("sessions lock").insert(session.clone());
            state.inits.fetch_add(1, Ordering::SeqCst);

            let requested = message
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-11-25");
            let result = json!({
                "protocolVersion": requested,
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "fake-mcp-http-server", "version": "0.1.0" },
            });
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            send(
                &mut socket,
                200,
                "OK",
                "application/json",
                &reply.to_string(),
                &[(HEADER_SESSION, &session)],
            )
            .await
        }
        // The one notification the handshake sends. A notification gets no
        // reply, so 202 with an empty body is the whole answer.
        "notifications/initialized" => {
            send(&mut socket, 202, "Accepted", "application/json", "", &[]).await
        }
        "tools/list" => {
            let result = json!({ "tools": tool_list(state.grown.load(Ordering::SeqCst)) });
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            send(&mut socket, 200, "OK", "application/json", &reply.to_string(), &[]).await
        }
        "tools/call" => {
            let name = message.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
            let arguments =
                message.pointer("/params/arguments").cloned().unwrap_or_else(|| json!({}));

            let text = match name {
                "echo" => {
                    arguments.get("text").and_then(Value::as_str).unwrap_or_default().to_string()
                }
                "grow" => {
                    state.grown.store(true, Ordering::SeqCst);
                    // Server-initiated, and it can only reach the client over
                    // the SSE stream: a POST of ours has no reply channel.
                    let _ = state.events.send(
                        json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/tools/list_changed",
                        })
                        .to_string(),
                    );
                    "grew".to_string()
                }
                "disconnect" => {
                    let _ = state.closes.send(());
                    "disconnected".to_string()
                }
                "expire" => {
                    if let Some(id) = &session {
                        state.sessions.lock().expect("sessions lock").remove(id);
                    }
                    "expired".to_string()
                }
                "stats" => stats_text(&state, &session),
                other => {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": format!("unknown tool: {other}") },
                    });
                    return send(
                        &mut socket,
                        200,
                        "OK",
                        "application/json",
                        &reply.to_string(),
                        &[],
                    )
                    .await;
                }
            };

            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [ { "type": "text", "text": text } ],
                    "isError": false,
                },
            });
            send(&mut socket, 200, "OK", "application/json", &reply.to_string(), &[]).await
        }
        other => {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") },
            });
            send(&mut socket, 200, "OK", "application/json", &reply.to_string(), &[]).await
        }
    }
}

/// The server's half of the stream: answers `GET` with an event stream that
/// stays open, and pushes whatever the tool set does to it.
async fn stream(mut socket: TcpStream, state: Arc<State>, head: &str) -> std::io::Result<()> {
    if !accepts_both(head) {
        return refuse_accept(&mut socket).await;
    }

    let Some(session) = header(head, HEADER_SESSION) else {
        return send(&mut socket, 404, "Not Found", "application/json", "", &[]).await;
    };
    if !state.sessions.lock().expect("sessions lock").contains(&session) {
        return send(&mut socket, 404, "Not Found", "application/json", "", &[]).await;
    }

    // Subscribed before the response goes out: an event that arrives in the
    // window between the client's request and this server subscribing would
    // otherwise be dropped, and the client would be right to blame the server.
    // Both subscriptions are made before the counter moves, so a test that sees
    // the count has already seen a stream that will receive what is sent next.
    let mut events = state.events.subscribe();
    let mut closes = state.closes.subscribe();
    state.gets.fetch_add(1, Ordering::SeqCst);

    // `Transfer-Encoding: chunked` is not decoration: the frames below are
    // chunked, and a response that framed its body one way while declaring
    // another is a response the client is entitled to mis-read. Without it the
    // body is close-delimited and the chunk headers become payload.
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Transfer-Encoding: chunked\r\nCache-Control: no-cache\r\n\
              Connection: close\r\n\r\n",
        )
        .await?;
    socket.flush().await?;

    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Asked to drop this stream mid-flight, the way a proxy or a server
            // restart would. The socket closes where it stands: a truncated
            // chunked body is exactly what a client sees when the other end
            // disappears.
            Ok(()) = closes.recv() => break,
        };

        // Chunked, because an HTTP/1.1 stream with no `Content-Length` and no
        // framing is not a response any client can parse.
        let payload = format!("data: {event}\n\n");
        let chunk = format!("{:x}\r\n{payload}\r\n", payload.len());
        if socket.write_all(chunk.as_bytes()).await.is_err() {
            break;
        }
        if socket.flush().await.is_err() {
            break;
        }
    }

    let _ = socket.shutdown().await;
    Ok(())
}

/// The counters and the headers this server has seen, as a tool result.
fn stats_text(state: &State, session: &Option<String>) -> String {
    let last = state.last.lock().expect("the observed lock is never poisoned").clone();
    json!({
        "inits": state.inits.load(Ordering::SeqCst),
        "gets": state.gets.load(Ordering::SeqCst),
        "posts": state.posts.load(Ordering::SeqCst),
        "deletes": state.deletes.load(Ordering::SeqCst),
        "sessions": state.sessions.lock().expect("sessions lock").len(),
        "session": session.clone().unwrap_or_default(),
        "header": last.header,
        "accept": last.accept,
    })
    .to_string()
}

fn accepts_both(head: &str) -> bool {
    let accept = header(head, "accept").unwrap_or_default();
    accept.contains("text/event-stream") && accept.contains("application/json")
}

async fn refuse_accept(socket: &mut TcpStream) -> std::io::Result<()> {
    let accept = "a POST or GET must accept both text/event-stream and application/json";
    send(
        socket,
        406,
        "Not Acceptable",
        "application/json",
        &format!("{{\"error\":\"{accept}\"}}\n"),
        &[],
    )
    .await
}

fn tool_list(grown: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "echo",
            "description": "Echo the given text back",
            "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
            "annotations": { "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "grow",
            "description": "Add a tool to the server",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "disconnect",
            "description": "Drop the caller's event stream",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "expire",
            "description": "Forget the caller's session",
            "inputSchema": { "type": "object", "properties": {} },
        }),
        json!({
            "name": "stats",
            "description": "What this server has seen",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        }),
    ];
    if grown {
        tools.push(json!({
            "name": "grown",
            "description": "A tool that appeared after a list_changed notification",
            "inputSchema": { "type": "object", "properties": {} },
        }));
    }
    tools
}

/// One response, with `Content-Length` and the connection closed behind it.
async fn send(
    socket: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
    extra: &[(&str, &str)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.flush().await?;
    socket.shutdown().await
}

/// The request target's path, without its query.
fn path_of(head: &str) -> Option<String> {
    let target = head.lines().next()?.split_whitespace().nth(1)?;
    Some(match target.split_once('?') {
        Some((path, _query)) => path.to_string(),
        None => target.to_string(),
    })
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field.eq_ignore_ascii_case(name).then(|| value.trim().to_string())
    })
}

/// Read one HTTP/1.1 request: its head, and `Content-Length` bytes of body.
async fn read_request(socket: &mut TcpStream) -> std::io::Result<Option<(String, Vec<u8>)>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let length = content_length(&head).unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Ok(Some((head, buffer[head_end..].to_vec())))
}

fn content_length(head: &str) -> Option<usize> {
    header(head, "content-length").and_then(|value| value.parse().ok())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}
