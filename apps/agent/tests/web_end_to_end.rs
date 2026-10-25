//! `agent web`, end to end: the assembled binary, a real data directory, and
//! the HTTP surface the page talks to.
//!
//! `crates/agent-web` drives its handlers over a fake backend, and the browser
//! smoke drives the page. Neither runs the driver the binary builds for real,
//! so nothing else checks that a session started over HTTP is the session the
//! terminal would write, that the model choice lands in that session's log, or
//! that deleting one really removes it from the disk.
//!
//! The server is started the way a user starts it — `web --port 0`, a throwaway
//! data directory, the echo provider — and driven over loopback with plain
//! HTTP/1.1, so the test needs no client library and no browser.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

/// The configuration variables the binary documents. Removed for the same
/// reason `cli_end_to_end` removes them: a developer with a user configuration
/// exported would have every assertion here quietly run under it.
const ENV_USER_CONFIG: &str = "AGENT_USER_CONFIG";
const ENV_MANAGED_POLICY: &str = "AGENT_MANAGED_POLICY";

/// A `agent web` process and the two directories it was given.
struct Server {
    child: Child,
    addr: SocketAddr,
    token: String,
    data: TempDir,
    _workspace: TempDir,
}

impl Server {
    fn start() -> Server {
        let data = tempfile::tempdir().expect("data dir");
        let workspace = tempfile::tempdir().expect("workspace dir");
        let mut child = Command::new(env!("CARGO_BIN_EXE_agent"))
            .args(["web", "--port", "0"])
            .env("AGENT_DATA_DIR", data.path())
            .env("AGENT_WORKSPACE_DIR", workspace.path())
            .env("AGENT_MODEL_PROVIDER", "echo")
            .env_remove(ENV_USER_CONFIG)
            .env_remove(ENV_MANAGED_POLICY)
            .env_remove("AGENT_SANDBOX")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agent web");

        // Reading on a thread and timing the receive is what turns "the server
        // never printed a URL" into a failure instead of a hang.
        let stdout = child.stdout.take().expect("stdout");
        let (lines, printed) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        // The stderr is collected rather than sent to NUL: a server that refuses
        // to start explains itself there, and that explanation belongs in the
        // failure instead of a bare "exited". (It also keeps the child off the
        // NUL device, which not every machine lets a parent open.)
        let note = Arc::new(Mutex::new(String::new()));
        let sink = note.clone();
        let mut stderr = child.stderr.take().expect("stderr");
        std::thread::spawn(move || {
            let mut buffer = String::new();
            let _ = stderr.read_to_string(&mut buffer);
            if let Ok(mut sink) = sink.lock() {
                sink.push_str(&buffer);
            }
        });

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "the server never printed its URL: {}",
                note.lock().map(|note| note.clone()).unwrap_or_default()
            );
            let Ok(line) = printed.recv_timeout(left) else {
                panic!(
                    "the server exited before printing its URL: {}",
                    note.lock().map(|note| note.clone()).unwrap_or_default()
                );
            };
            let Some((addr, token)) = parse_url(&line) else { continue };
            return Server { child, addr, token, data, _workspace: workspace };
        }
    }

    fn data_dir(&self) -> &std::path::Path {
        self.data.path()
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, String) {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(60))).expect("read timeout");
        let payload = body.map(|body| body.to_string()).unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Connection: close\r\n",
            self.addr, self.token
        );
        if !payload.is_empty() {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                payload.len()
            ));
        }
        request.push_str("\r\n");
        request.push_str(&payload);
        stream.write_all(request.as_bytes()).expect("write");
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).expect("read");
        let text = String::from_utf8_lossy(&bytes).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {text:?}"));
        (status, text)
    }

    fn get(&self, path: &str) -> (u16, String) {
        self.call("GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> (u16, String) {
        self.call("POST", path, Some(body))
    }

    fn delete(&self, path: &str) -> (u16, String) {
        self.call("DELETE", path, None)
    }

    /// The beginning of an event stream: whatever arrives before it goes quiet
    /// or the read times out, which is the replay of the session and nothing
    /// more. An SSE response never ends on its own, so it is read in pieces
    /// rather than to EOF, and the socket is what closes it.
    fn stream_head(&self, path: &str) -> String {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(3))).expect("read timeout");
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Accept: text/event-stream\r\nConnection: close\r\n\r\n",
            self.addr, self.token
        );
        stream.write_all(request.as_bytes()).expect("write");
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        while let Ok(read) = stream.read(&mut buffer) {
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.windows(7).any(|window| window == b"event: ") && bytes.ends_with(b"\n\n") {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).to_string()
    }

    /// The transcript, once it says what the test is waiting for.
    ///
    /// Polling the projection rather than sleeping a fixed time: a turn ends
    /// when its log says so, and that is the only moment the answer is there.
    ///
    /// A turn is accepted before its first append lands, so for a moment the
    /// session has no log and the read is a 404. That is the race this waits
    /// out: the failure worth reporting is a turn that never writes, not one
    /// that has not written yet.
    fn transcript_until(&self, id: &str, wanted: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (status, text) = self.get(&format!("/api/sessions/{id}/transcript"));
            assert!(
                status == 200 || status == 404,
                "the transcript read answered {status}: {text}"
            );
            if status == 200 && text.contains(wanted) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "the turn never reached the transcript; wanted {wanted:?} in {text}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The address and token out of the line `agent web` prints.
fn parse_url(line: &str) -> Option<(SocketAddr, String)> {
    let rest = line.split_once("http://")?.1;
    let (host, fragment) = rest.split_once("/#t=")?;
    let token = fragment.split_whitespace().next()?.to_string();
    Some((host.parse().expect("the URL names a socket address"), token))
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or(response)
}

#[test]
fn a_session_lives_through_the_web_surface() {
    let server = Server::start();

    // A fresh data directory lists nothing, and says what this process can do.
    let (status, text) = server.get("/api/sessions");
    assert_eq!(status, 200);
    assert!(body(&text).contains("\"sessions\":[]"), "no sessions yet: {}", body(&text));
    let (status, text) = server.get("/api/meta");
    assert_eq!(status, 200);
    assert!(body(&text).contains("\"echo\""), "the echo provider is listed: {}", body(&text));

    // A turn on an id the directory has never seen: accepted, then written.
    let id = "web-e2e-1";
    let (status, text) = server.post(
        &format!("/api/sessions/{id}/turns"),
        json!({
            "v": 1,
            "text": "say hello",
        }),
    );
    assert_eq!(status, 202, "the turn was not accepted: {text}");
    let transcript = server.transcript_until(id, "echo:");
    assert!(transcript.contains("say hello"), "the prompt is in the transcript: {transcript}");
    assert!(
        server.data_dir().join(id).join("events.jsonl").is_file(),
        "the turn wrote the same log the terminal reads"
    );

    // The summary the page lists, and the event stream it keeps open: the same
    // session, read the two other ways the page reads it.
    let (status, text) = server.get(&format!("/api/sessions/{id}"));
    assert_eq!(status, 200, "the session summary failed: {text}");
    assert!(body(&text).contains("\"id\":\"web-e2e-1\""), "the summary names it: {text}");
    let replay = server.stream_head(&format!("/api/sessions/{id}/events?from=0"));
    assert!(replay.contains("event: item"), "the stream replays the session: {replay}");

    // The read surface the page opens next. The three that have nothing to show
    // for an echo turn say so by name — a session that never planned has no
    // plan, rather than an empty one — and the rest answer from the same log.
    for (path, expected) in
        [("/why", 200), ("/context", 200), ("/plan", 404), ("/subagents", 404), ("/diff", 404)]
    {
        let (status, text) = server.get(&format!("/api/sessions/{id}{path}"));
        assert_eq!(status, expected, "{path} answered {status}: {text}");
        assert!(body(&text).contains("\"v\":1"), "{path} answered in this API's shape: {text}");
    }
    let (status, text) = server.get(&format!("/api/sessions/{id}/export"));
    assert_eq!(status, 200, "the export failed: {text}");
    assert!(text.contains("# Session"), "the export is the session as a document: {text}");

    // Archiving is an event in that log, and the list folds it.
    let (status, text) = server.post(
        &format!("/api/sessions/{id}/archive"),
        json!({
            "v": 1,
            "archived": true,
        }),
    );
    assert_eq!(status, 200, "archiving failed: {text}");
    let (_, text) = server.get("/api/sessions");
    assert!(body(&text).contains("\"archived\":true"), "the list says archived: {}", body(&text));
    let (status, _) =
        server.post(&format!("/api/sessions/{id}/archive"), json!({ "v": 1, "archived": false }));
    assert_eq!(status, 200, "restoring failed");
    let (_, text) = server.get("/api/sessions");
    assert!(body(&text).contains("\"archived\":false"), "the list says restored: {}", body(&text));

    // A model this process can build is recorded; one it cannot is the
    // request's fault, and the session keeps running either way.
    let (status, text) =
        server.post(&format!("/api/sessions/{id}/model"), json!({ "v": 1, "provider": "echo" }));
    assert_eq!(status, 200, "choosing echo failed: {text}");
    assert!(body(&text).contains("\"provider\":\"echo\""), "the choice is echoed back: {text}");
    let (status, _) =
        server.post(&format!("/api/sessions/{id}/model"), json!({ "v": 1, "provider": "nope" }));
    assert_eq!(status, 400, "a provider this build cannot make is a bad request");

    // Deleting removes the log itself, and the id is gone from every view.
    let (status, text) = server.delete(&format!("/api/sessions/{id}"));
    assert_eq!(status, 200, "deleting failed: {text}");
    assert!(!server.data_dir().join(id).exists(), "the session directory is still there");
    let (status, _) = server.get(&format!("/api/sessions/{id}"));
    assert_eq!(status, 404, "a deleted session is not readable");
    let (_, text) = server.get("/api/sessions");
    assert!(body(&text).contains("\"sessions\":[]"), "the list is empty again: {}", body(&text));

    // Deleting what is already gone is a 404 about the id, not a crash.
    let (status, _) = server.delete(&format!("/api/sessions/{id}"));
    assert_eq!(status, 404);
}

#[test]
fn an_unknown_session_is_not_a_place_to_write() {
    let server = Server::start();
    let (status, text) = server.post(
        "/api/sessions/never-existed/archive",
        json!({
            "v": 1,
            "archived": true,
        }),
    );
    assert_eq!(status, 404, "archiving never creates a log: {text}");
    assert!(!server.data_dir().join("never-existed").exists(), "nothing was created");
    let (status, _) =
        server.post("/api/sessions/never-existed/archive", json!({ "archived": true }));
    assert_eq!(status, 404);
    assert_eq!(server.get("/api/sessions/never-existed").0, 404);
    assert_eq!(server.delete("/api/sessions/never-existed").0, 404);
    let dir = PathBuf::from(server.data_dir());
    assert!(dir.exists(), "the data directory is the one the process was given");
}
