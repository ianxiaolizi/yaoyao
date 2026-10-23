//! A stand-in OpenAI-compatible endpoint, for tests that need a real socket and
//! a real HTTP response the agent's own adapter has to decode.
//!
//! It is a process rather than an in-process transport double for the reason the
//! MCP fixtures are: the byte-level half of the adapter — an SSE stream framed
//! and decoded, a request body serialized and sent — is exactly the half a
//! fake that replaced the socket would leave untested. It is hand-rolled rather
//! than built on `axum`/`hyper` so that it cannot agree with the client by
//! sharing a framework's bugs.
//!
//! Three things it does, and each exists because a test needs it:
//!
//! ```text
//! --script <path>     answer each request from this file, in order
//! --log <path>        append every request body it receives, one per line
//! --hold-after <n>    after answering n entries, stop until the release file appears
//! --release <path>    the file whose existence releases the hold
//! ```
//!
//! ## Why the request log is the point
//!
//! A test about *what a run offered the model* must not ask the run. The bodies
//! this server received are the world: they were produced by the loop, sent over
//! a socket, and written down by a different process that has no idea what the
//! test hopes to find. A tool manifest that disappeared from the second step of
//! a turn, or an extra tool that crept into the first, shows up here as a
//! difference between two recorded bodies and nowhere else.
//!
//! ## Why the hold exists
//!
//! A control has to be applied at a boundary, so a test that injects one has to
//! be able to place it *inside* a turn deterministically. Sleeping long enough
//! and hoping is how such a test becomes flaky on a loaded machine. Instead the
//! server stops mid-turn — after answering entry `n`, before answering `n + 1` —
//! and says so on stdout; the test reads that line, does its work, and writes the
//! release file. Nothing here times anything.
//!
//! The script is a JSON array whose entries are answered in order:
//!
//! ```json
//! [
//!   {"tool_call": {"id": "call_1", "name": "mcp__one__echo", "arguments": {"text": "hi"}}},
//!   {"answer": "done"}
//! ]
//! ```
//!
//! Running past the end of the script is a 500 with the reason in the body
//! rather than a hang: a test that expected two model calls and got three should
//! fail with a sentence, not with a timeout.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The one path this endpoint serves. Spelled out rather than imported from the
/// adapter, so that a request sent to the wrong path is a 404 here instead of
/// being answered by a fixture that agrees with the code by construction.
const PATH: &str = "/chat/completions";

/// One scripted reply.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Step {
    /// The model asks for a tool.
    ToolCall {
        id: String,
        /// The *wire* name, as the endpoint would echo it back — dots already
        /// projected to underscores, because that is what a real vendor would
        /// have received and would return.
        name: String,
        #[serde(default)]
        arguments: Value,
    },
    /// The model answers and the turn ends. A bare string, because an answer is
    /// one thing and a `{"text": ...}` wrapper around it would be a shape this
    /// file invented rather than one a caller wants to write.
    Answer(String),
}

struct State {
    script: Vec<Step>,
    served: AtomicUsize,
    log: PathBuf,
    /// Answer this many entries, then hold every later request.
    hold_after: Option<usize>,
    release: Option<PathBuf>,
    model: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let argv: Vec<String> = std::env::args().collect();

    let port = flag(&argv, "--port").and_then(|value| value.parse::<u16>().ok()).unwrap_or(0);
    let script_path = flag(&argv, "--script").expect("--script <path> is required");
    let log = PathBuf::from(flag(&argv, "--log").expect("--log <path> is required"));
    let hold_after = flag(&argv, "--hold-after").and_then(|value| value.parse::<usize>().ok());
    let release = flag(&argv, "--release").map(PathBuf::from);
    let model = flag(&argv, "--model").unwrap_or_else(|| "fake-model".to_string());

    let text = std::fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("cannot read script {script_path}: {e}"));
    let script: Vec<Step> =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("script is not a JSON array: {e}"));

    // The log is opened now, not at the first request, so a test can tell "the
    // endpoint was never reached" from "it was reached and did not answer".
    std::fs::write(&log, b"").unwrap_or_else(|e| panic!("cannot create {}: {e}", log.display()));

    let state =
        Arc::new(State { script, served: AtomicUsize::new(0), log, hold_after, release, model });

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

/// `--name value`, or nothing.
fn flag(argv: &[String], name: &str) -> Option<String> {
    argv.iter().position(|arg| arg == name).and_then(|index| argv.get(index + 1).cloned())
}

async fn serve(mut socket: TcpStream, state: Arc<State>) -> std::io::Result<()> {
    let Some((head, body)) = read_request(&mut socket).await? else {
        return Ok(());
    };

    if path_of(&head).as_deref() != Some(PATH) {
        return send(&mut socket, 404, "Not Found", "application/json", "{}").await;
    }

    // Recorded before anything else can fail: what arrived is evidence whether
    // or not this server manages to answer it.
    append(&state.log, &body);

    // The hold is taken *before* the entry is chosen, so the request that is
    // being held is one the test can see arrive. Announcing it here is what
    // makes the placement of a control deterministic instead of a race.
    let held = state.hold_after.is_some_and(|after| state.served.load(Ordering::SeqCst) >= after);
    if held {
        let waiting = state.served.load(Ordering::SeqCst) + 1;
        println!("holding request {waiting}");
        std::io::stdout().flush().expect("stdout");
        let release = state.release.clone().expect("--hold-after without --release");
        while !release.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        println!("released request {waiting}");
        std::io::stdout().flush().expect("stdout");
    }

    let index = state.served.fetch_add(1, Ordering::SeqCst);
    let Some(step) = state.script.get(index) else {
        let body = json!({
            "error": { "message": format!("the script has no entry {index}; it has {}", state.script.len()) }
        })
        .to_string();
        return send(&mut socket, 500, "Internal Server Error", "application/json", &body).await;
    };

    let stream = render(step, &state.model);
    println!("answered {}", index + 1);
    std::io::stdout().flush().expect("stdout");
    send(&mut socket, 200, "OK", "text/event-stream", &stream).await
}

/// One entry, as the SSE frames a real endpoint would send.
///
/// The shapes are the ones the adapter's decoder reads, and they are written out
/// here rather than built through the adapter's own types: a fixture that
/// serialized its answers with the code under test could not disagree with it.
fn render(step: &Step, model: &str) -> String {
    let mut out = String::new();
    match step {
        Step::Answer(text) => {
            frame(
                &mut out,
                json!({
                    "model": model,
                    "choices": [{ "index": 0, "delta": { "role": "assistant", "content": text }, "finish_reason": null }],
                }),
            );
            // The terminal chunk, with the usage block a real endpoint sends
            // when the request asked for one.
            frame(
                &mut out,
                json!({
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                }),
            );
            frame(
                &mut out,
                json!({
                    "model": model,
                    "choices": [],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
                }),
            );
        }
        Step::ToolCall { id, name, arguments } => {
            // `arguments` travels as a *string* on the wire, not as an object —
            // that is the shape the accumulator parses back, and sending an
            // object here would pass a fake that could not catch the difference.
            frame(
                &mut out,
                json!({
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "tool_calls": [{
                                "index": 0,
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": arguments.to_string() },
                            }],
                        },
                        "finish_reason": null,
                    }],
                }),
            );
            frame(
                &mut out,
                json!({
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
                }),
            );
        }
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn frame(out: &mut String, payload: Value) {
    out.push_str("data: ");
    out.push_str(&payload.to_string());
    out.push_str("\n\n");
}

/// Append one request body as a line, and get it on disk before returning.
///
/// Flushed rather than buffered: a test reads this file while the run that wrote
/// it is still going, and a body sitting in a `BufWriter` is a body the test
/// cannot see yet.
fn append(path: &PathBuf, body: &[u8]) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|e| panic!("cannot open {}: {e}", path.display()));
    file.write_all(body).expect("write the request body");
    file.write_all(b"\n").expect("write the newline");
    file.flush().expect("flush the request body");
}

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
    let length = header(&head, "content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Ok(Some((head, buffer[head_end..].to_vec())))
}

async fn send(
    socket: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.flush().await?;
    socket.shutdown().await
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).take_while(|line| !line.trim().is_empty()).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim().eq_ignore_ascii_case(name)).then(|| value.trim().to_string())
    })
}

fn path_of(head: &str) -> Option<String> {
    let target = head.lines().next()?.split_whitespace().nth(1)?;
    Some(match target.split_once('?') {
        Some((path, _query)) => path.to_string(),
        None => target.to_string(),
    })
}
