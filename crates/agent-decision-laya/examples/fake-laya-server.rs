//! A stand-in `laya-serve`, for tests that need a real socket.
//!
//! It is a **process**, not an in-process stub, for the same reason the MCP
//! fixture is one: the part of this adapter most likely to be wrong is the part
//! that speaks HTTP over a socket, and a fake that replaced the socket would
//! test everything except that.
//!
//! It answers in the shapes a live `laya 0.3.20` server answers in — verified
//! against one — and it picks its behaviour from the request's query string, so
//! one binary covers every case a test needs:
//!
//! ```text
//! /v1/systemone               a well-formed answer per question
//! /v1/systemone?mode=abstain  every answer confident at 0.01
//! /v1/systemone?mode=fail     HTTP 500, the way a bad question comes back
//! /v1/systemone?mode=slow     two seconds late
//! /v1/systemone?mode=garbage  200 with a body that is not JSON
//! /v1/systemone?mode=wrongtype  every question answered as `noul`
//! /v1/systemone?mode=partial   one question left unanswered
//! /v1/systemone?mode=truncate  headers, then a body cut short of its length
//! ```
//!
//! The chosen option for a `choice` question is the last key of its criteria —
//! deterministic, and deliberately not the first, so a test can tell this
//! backend's answer apart from a rule baseline that happens to pick the first
//! option.
//!
//! Anything but `/v1/systemone` is a 404, the way the real server answers. The
//! path is spelled out here rather than imported from the adapter on purpose:
//! an earlier version of this fixture ignored the path, and the adapter posting
//! to `/` therefore passed every test in the workspace while a live
//! `laya-serve` answered 404. A fixture that agrees with the code it tests by
//! construction cannot catch that.

/// The path the real server serves, from its own routing table.
const SYSTEMONE_PATH: &str = "/v1/systemone";

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let port = std::env::args()
        .skip_while(|arg| arg != "--port")
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    let default_mode = std::env::var("FAKE_LAYA_MODE").unwrap_or_else(|_| "ok".to_string());

    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind loopback");
    println!("listening {}", listener.local_addr().expect("a bound socket has an address"));
    std::io::stdout().flush().expect("stdout");

    loop {
        let (socket, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let mode = default_mode.clone();
        tokio::spawn(async move {
            let _ = serve(socket, &mode).await;
        });
    }
}

async fn serve(mut socket: TcpStream, default_mode: &str) -> std::io::Result<()> {
    let Some((head, body)) = read_request(&mut socket).await? else {
        return Ok(());
    };
    // The path, before the behaviour: a request to the wrong one is not a
    // question this server can answer, however well formed its body is.
    if path_of(&head).as_deref() != Some(SYSTEMONE_PATH) {
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        socket.write_all(response.as_bytes()).await?;
        return socket.shutdown().await;
    }

    let mode = mode_of(&head).unwrap_or_else(|| default_mode.to_string());

    // A response that arrives and then dies: the status line and headers are
    // real, so the client has to decide what a lost body means. The advertised
    // length is honest about what was promised; the connection closes early.
    if mode == "truncate" {
        let partial = r#"{"answers":{"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 512\r\n\
             Connection: close\r\n\r\n{partial}"
        );
        socket.write_all(response.as_bytes()).await?;
        socket.flush().await?;
        return socket.shutdown().await;
    }

    let (status, payload) = match mode.as_str() {
        "fail" => (500, json!({ "detail": "inference failed" })),
        "slow" => {
            tokio::time::sleep(Duration::from_secs(2)).await;
            (200, answer_body(&body, &mode))
        }
        _ => (200, answer_body(&body, &mode)),
    };

    let rendered = match mode.as_str() {
        "garbage" => "this is not json".to_string(),
        _ => payload.to_string(),
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{rendered}",
        rendered.len(),
        reason = if status == 200 { "OK" } else { "Internal Server Error" },
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    socket.shutdown().await
}

/// The answer document for a request body, in the mode's shape.
fn answer_body(body: &[u8], mode: &str) -> Value {
    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let questions =
        request.get("questions").and_then(Value::as_object).cloned().unwrap_or_default();

    let mut answers = serde_json::Map::new();
    for (index, (id, question)) in questions.iter().enumerate() {
        if mode == "partial" && index > 0 {
            break;
        }
        let mut answer = if mode == "wrongtype" {
            json!({ "type": "noul", "noul": 0.97, "answer_confidence": 0.97 })
        } else {
            match question.get("type").and_then(Value::as_str) {
                Some("choice") => {
                    // The last key of the criteria object: deterministic, and
                    // not the first option a baseline would pick.
                    let criteria = question.get("criteria").and_then(Value::as_object);
                    let choice = criteria
                        .and_then(|options| options.keys().next_back())
                        .cloned()
                        .unwrap_or_else(|| "none".to_string());
                    json!({ "type": "choice", "choice": choice, "answer_confidence": 0.93 })
                }
                // An expected level index, the way a live server reports one.
                Some("score") => {
                    json!({ "type": "score", "score": 1.0, "answer_confidence": 0.90 })
                }
                _ => json!({ "type": "noul", "noul": 0.0236, "answer_confidence": 0.97 }),
            }
        };
        if mode == "abstain"
            && let Some(object) = answer.as_object_mut()
        {
            object.insert("answer_confidence".to_string(), json!(0.01));
        }
        answers.insert(id.clone(), answer);
    }

    let model = request.get("model").and_then(Value::as_str).unwrap_or("fake-laya");
    let mut body: BTreeMap<String, Value> = BTreeMap::new();
    body.insert("model".to_string(), json!(model));
    body.insert("answers".to_string(), Value::Object(answers));
    body.insert("usage".to_string(), json!({ "input_tokens": 1, "output_tokens": 0 }));
    Value::Object(body.into_iter().collect())
}

/// The request target's path, without its query.
fn path_of(head: &str) -> Option<String> {
    let target = head.lines().next()?.split_whitespace().nth(1)?;
    Some(match target.split_once('?') {
        Some((path, _query)) => path.to_string(),
        None => target.to_string(),
    })
}

/// `/v1/systemone?mode=x` → `x`.
fn mode_of(head: &str) -> Option<String> {
    let request_line = head.lines().next()?;
    let target = request_line.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    query.split('&').find_map(|pair| pair.strip_prefix("mode=")).map(|value| value.to_string())
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

    let body = buffer[head_end..].to_vec();
    Ok(Some((head, body)))
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}
