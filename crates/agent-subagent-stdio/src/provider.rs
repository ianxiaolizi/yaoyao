//! A child agent as a child process, spoken to in JSON lines.
//!
//! One process per child. The parent hands it a request on stdin, reads its
//! lines, and is the one that decides when the child is over: the process is
//! killed as soon as the child answers, when the deadline passes, or when the
//! parent asks. Nothing here waits for a child to decide to stop on its own,
//! because a parent that could be held open by a child's unwillingness to exit
//! has no way to make the child stop — and being able to is this backend's
//! whole job.
//!
//! ## What this backend honours, and how
//!
//! One of the features it advertises it **enforces**: the deadline, because it
//! owns the process. The rest it **carries**: the tool allowlist, the model
//! route and the output schema go to the child in named fields, and whether the
//! child obeys is the child's business — but a child that was *told* is in a
//! different position from one that never heard, and that difference is what a
//! caller can rely on.
//!
//! It does not advertise `budget`, and refuses a request that asks for one. A
//! budget is enforced by the loop that spends it, an external process runs its
//! own loop, and passing a spend limit down the wire would leave the parent
//! believing in a bound that nothing holds. Refusing is the whole point of the
//! negotiation.
//!
//! ## Why a pump task per child
//!
//! A child's process and its two pipes have to be owned by something that
//! outlives any single call: `spawn` returns while the child is still running,
//! `wait` and `cancel` may come from different tasks, and a `dispose` may come
//! at any time. One task per child — holding the `Child`, driving the stdout
//! reader, the deadline and the cancel signal, and publishing the outcome
//! through a `watch` — makes "how did it end" a single value every one of those
//! calls reads, rather than a race between whoever happens to poll first.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_ports::process_env::child_environment_with;
use agent_ports::subagent::{
    SubagentError, SubagentFeature, SubagentFeatures, SubagentHandle, SubagentOutcome,
    SubagentProvider, SubagentRequest,
};
use agent_protocol::ids::{SessionId, TurnId};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, timeout, timeout_at};

use crate::wire::{self, ChildMessage};

/// How long a child has when neither its request nor its configuration says.
///
/// There always is a ceiling: a backend with none would let a wedged child hold
/// a parent open forever. Five minutes is long enough that a real delegation
/// does not hit it and short enough that a wedged one does.
pub const DEFAULT_CEILING: Duration = Duration::from_secs(300);

/// How long a child is given to accept its request before the delivery counts
/// as failed. It is one line into a pipe; a child that has not read it in this
/// long is not going to.
const DELIVER_BOUND: Duration = Duration::from_secs(10);

/// How long to wait for a child to be gone after killing it, for a stop the
/// parent asked for to be confirmed, and for a dead child's stderr to finish
/// arriving.
const STOP_BOUND: Duration = Duration::from_secs(10);
const STDERR_BOUND: Duration = Duration::from_millis(500);

/// How much of a child's stderr is kept, when it is quoted into a failure: the
/// **last** `MAX_STDERR` bytes of it. A child that dies without answering is
/// read by what it said on the way out, not by the part that went fine.
const MAX_STDERR: usize = 4_096;

/// A provider that runs each child as a child process.
pub struct StdioSubagentProvider {
    name: String,
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    ceiling: Duration,
    children: Mutex<HashMap<String, ChildState>>,
}

/// What the provider holds for one child.
struct ChildState {
    cancel: mpsc::Sender<()>,
    /// `None` while the child runs; `Some` once it has ended, for as long as
    /// the handle is held.
    outcome: watch::Receiver<Option<SubagentOutcome>>,
}

impl StdioSubagentProvider {
    /// A provider that runs `program` for each child.
    ///
    /// `name` is what appears in records and refusals — the name the
    /// configuration gave this backend, not the program's own name, because two
    /// configured backends may run the same binary with different arguments and
    /// a record has to say which one was asked.
    pub fn new(name: impl Into<String>, program: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            ceiling: DEFAULT_CEILING,
            children: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// The environment a child is started with: the allowlist plus this
    /// backend's configured variables, with `AGENT_*` refused even when
    /// configured (`agent_ports::process_env` says why).
    fn child_environment(&self) -> Vec<(OsString, OsString)> {
        let explicit: Vec<(OsString, OsString)> =
            self.env.iter().map(|(key, value)| (key.into(), value.into())).collect();
        child_environment_with(&std::env::vars_os().collect::<Vec<_>>(), &explicit)
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    /// The longest a child of this backend may run.
    ///
    /// A ceiling rather than a default: a request may ask for less and never for
    /// more, because the party that configured the backend is the party that
    /// knows what a child is allowed to cost.
    pub fn with_ceiling(mut self, ceiling: Duration) -> Self {
        self.ceiling = ceiling;
        self
    }

    /// The deadline this request will actually be held to.
    fn effective_timeout(&self, request: &SubagentRequest) -> Duration {
        match request.timeout_ms {
            // Zero is read as "unset": a child given no time at all is never
            // what a caller meant, and one who did mean it can say so by
            // configuring the backend.
            Some(0) | None => self.ceiling,
            Some(ms) => Duration::from_millis(ms).min(self.ceiling),
        }
    }

    /// The child table, or the error a poisoned lock means.
    ///
    /// A poisoned lock says one thing — a panic happened while the table was
    /// held — and not another: that the table is half-written. Neither is safe
    /// to assume it away from. The kernel's live bus reads through a poison,
    /// because its readers can report nothing else; every caller here returns a
    /// `Result`, so an error is the honest answer. Reading through would be
    /// worse than merely uncertain: a poisoned table answers `Unknown` for a
    /// child that is there, and a caller deciding whether to reuse what the
    /// child touched cannot tell that from the truth.
    fn children(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, ChildState>>, SubagentError> {
        self.children.lock().map_err(|_| SubagentError::Lost {
            provider: self.name.clone(),
            message: "a panic elsewhere in the process poisoned the child table".to_string(),
        })
    }

    /// The outcome channel for `handle`, or `Unknown` if this provider holds
    /// nothing for that child.
    fn receiver(
        &self,
        handle: &SubagentHandle,
    ) -> Result<watch::Receiver<Option<SubagentOutcome>>, SubagentError> {
        self.children()?
            .get(handle.child_session_id.as_str())
            .map(|state| state.outcome.clone())
            .ok_or_else(|| SubagentError::Unknown {
                provider: self.name.clone(),
                child_session_id: handle.child_session_id.clone(),
            })
    }
}

#[async_trait::async_trait]
impl SubagentProvider for StdioSubagentProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn supported(&self) -> SubagentFeatures {
        [
            SubagentFeature::ToolAllowlist,
            SubagentFeature::ModelRoute,
            SubagentFeature::OutputSchema,
            SubagentFeature::Timeout,
        ]
        .into_iter()
        .collect()
    }

    async fn spawn(
        &self,
        child_session_id: &SessionId,
        parent_turn_id: Option<&TurnId>,
        request: SubagentRequest,
    ) -> Result<SubagentHandle, SubagentError> {
        // Checked here as well as in the caller, because this is the line the
        // port draws: a backend that cannot honour a request refuses it without
        // starting anything. A caller that skipped its own check gets the same
        // answer, and a child that was never going to be right never runs.
        let missing = self.supported().missing_from(&request.requested_features());
        if !missing.is_empty() {
            return Err(SubagentError::Unsupported {
                provider: self.name.clone(),
                features: missing.into_iter().collect(),
            });
        }

        let deadline_in = self.effective_timeout(&request);
        let line = wire::RequestMessage::of(&request, deadline_in.as_millis() as u64)
            .line()
            .map_err(|error| SubagentError::Wire {
                message: format!("the request could not be encoded: {error}"),
            })?;

        let env = self.child_environment();
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If this provider is dropped while a child runs, the child goes
            // with it: a process nobody holds is a process nobody can stop.
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|error| SubagentError::Spawn {
            child_session_id: child_session_id.clone(),
            message: format!("'{}' could not be started: {error}", self.program),
        })?;

        if let Err(message) = deliver(&mut child, &line).await {
            // The child is running but was never told what to do. It is put down
            // here rather than handed back, because a handle to a child that
            // never heard its request is a handle to nothing.
            stop(&mut child).await;
            return Err(SubagentError::Wire { message });
        }

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let handle = SubagentHandle::new(
            self.name.clone(),
            child_session_id.clone(),
            parent_turn_id.cloned(),
        );

        // The table is locked before the child is started, so a poison is
        // answered before anything runs. Locking it after the spawn — as this
        // used to — would leave a child running that nothing here could reach
        // if the insert never happened: the handle to stop it is the thing that
        // failed to land.
        let mut children = self.children()?;
        let (cancel, cancelled) = mpsc::channel(1);
        let (outcomes, outcome) = watch::channel(None);
        tokio::spawn(supervise(child, stdout, stderr, deadline_in, cancelled, outcomes));

        children.insert(child_session_id.to_string(), ChildState { cancel, outcome });

        Ok(handle)
    }

    async fn wait(&self, handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError> {
        let mut outcome = self.receiver(handle)?;
        settle(&mut outcome).await
    }

    async fn cancel(&self, handle: &SubagentHandle) -> Result<SubagentOutcome, SubagentError> {
        // Read out what is needed and let the table go: nothing below may hold
        // the lock across an await.
        let (sender, mut outcome) = {
            let children = self.children()?;
            let state = children.get(handle.child_session_id.as_str()).ok_or_else(|| {
                SubagentError::Unknown {
                    provider: self.name.clone(),
                    child_session_id: handle.child_session_id.clone(),
                }
            })?;
            (state.cancel.clone(), state.outcome.clone())
        };

        // A child that has already ended is not asked to stop: it cannot, and
        // the honest answer to "make it stop" is how it stopped. An answer that
        // arrived a moment before the request is reported as itself rather than
        // relabelled.
        if outcome.borrow().is_none() {
            // A closed channel means the supervisor is already finishing, which
            // means an outcome is on its way — waiting for it is the answer.
            let _ = sender.send(()).await;
        }
        settle(&mut outcome).await
    }

    async fn dispose(&self, handle: &SubagentHandle) -> Result<(), SubagentError> {
        let state = self.children()?.remove(handle.child_session_id.as_str());

        // Nothing held for this child: already disposed, or never this
        // provider's. Idempotent by design — the only reason to dispose twice is
        // not knowing whether the first call landed.
        let Some(state) = state else {
            return Ok(());
        };

        if state.outcome.borrow().is_none() {
            let _ = state.cancel.send(()).await;
            let mut outcome = state.outcome.clone();
            // Bounded, and the handle is released whether or not the stop is
            // confirmed. Reporting an unconfirmed stop is `cancel`'s job and it
            // has a word for it; this call's job is to leave nothing held.
            let _ = timeout(STOP_BOUND, settle(&mut outcome)).await;
        }
        Ok(())
    }
}

/// Wait until the child has ended, and say how.
///
/// The supervisor publishes one outcome and then stops, so a waiter either gets
/// that value or outlives the supervisor — and a supervisor that vanished
/// without a value is itself a fact about the child.
async fn settle(
    outcome: &mut watch::Receiver<Option<SubagentOutcome>>,
) -> Result<SubagentOutcome, SubagentError> {
    loop {
        let current = (*outcome.borrow()).clone();
        if let Some(outcome) = current {
            return Ok(outcome);
        }
        if outcome.changed().await.is_err() {
            return Ok(SubagentOutcome::Failed {
                message: "the child's supervisor stopped without saying how it ended".to_string(),
                side_effects: Vec::new(),
            });
        }
    }
}

/// Write the request, then close stdin.
///
/// Closing is half of the message: the protocol says a child gets one line, and
/// a child waiting for a second one would wait forever.
async fn deliver(child: &mut Child, line: &str) -> Result<(), String> {
    let Some(mut stdin) = child.stdin.take() else {
        return Err("the child's stdin was not piped".to_string());
    };
    let written = async {
        stdin.write_all(line.as_bytes()).await.map_err(|error| error.to_string())?;
        stdin.flush().await.map_err(|error| error.to_string())?;
        drop(stdin);
        Ok::<(), String>(())
    };

    match timeout(DELIVER_BOUND, written).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "the child did not accept its request within {} ms",
            DELIVER_BOUND.as_millis()
        )),
    }
}

/// Own one child from the request it was given to the moment it is reaped.
async fn supervise(
    mut child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    deadline_in: Duration,
    mut cancelled: mpsc::Receiver<()>,
    outcomes: watch::Sender<Option<SubagentOutcome>>,
) {
    // A child's stderr is drained off to the side, capped, so that a talkative
    // child cannot block on a full pipe while its stdout is being read.
    let kept = Arc::new(Mutex::new(Vec::new()));
    let reader = tokio::spawn(drain(stderr, kept.clone()));

    let ending = read_until_ended(&mut child, stdout, deadline_in, &mut cancelled).await;

    // Whatever happened, the child is over: it answered, or it misbehaved, or
    // its time ran out, or the parent said stop. Stopping it here is also the
    // last moment the whole process tree can still be reached — a child that has
    // answered has nothing left to be running for, and anything it started goes
    // with it.
    let status = stop(&mut child).await;

    let outcome = match ending {
        Ending::Answer(outcome) => outcome,
        ending => {
            // The child is gone, so its stderr pipe is closing. The reader is
            // given a moment and then whatever it has: a grandchild on a
            // platform that cannot kill the tree may still hold the pipe open,
            // and failing a child is not a reason to hang waiting for a
            // diagnosis.
            let _ = timeout(STDERR_BOUND, reader).await;
            let note = stderr_note(&kept);

            match ending {
                Ending::Exited => SubagentOutcome::Failed {
                    message: format!(
                        "the child exited ({}) without a result{note}",
                        describe_status(status)
                    ),
                    side_effects: Vec::new(),
                },
                Ending::Violation(problem) => {
                    SubagentOutcome::Failed { message: problem, side_effects: Vec::new() }
                }
                Ending::TimedOut => timeout_outcome(status, deadline_in, &note),
                Ending::Cancelled => cancelled_outcome(status, &note),
                // Taken by the arm above; spelled out so this match is total
                // without an unreachable.
                Ending::Answer(outcome) => outcome,
            }
        }
    };

    let _ = outcomes.send(Some(outcome));
}

/// How a child's stdout ended.
enum Ending {
    /// The child said how it ended.
    Answer(SubagentOutcome),
    /// The child's stdout closed with no answer in it.
    Exited,
    /// The child wrote something this wire does not define, or its output could
    /// not be read. Both are reported the same way because both mean the same
    /// thing: this delegation's answer is not knowable from what arrived.
    Violation(String),
    /// The deadline passed with the child still working.
    TimedOut,
    /// The parent asked the child to stop, and it had not answered yet.
    Cancelled,
}

/// Read the child's stdout until it ends in one of the ways this wire defines.
async fn read_until_ended(
    child: &mut Child,
    stdout: ChildStdout,
    deadline_in: Duration,
    cancelled: &mut mpsc::Receiver<()>,
) -> Ending {
    let deadline = Instant::now() + deadline_in;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let next = timeout_at(deadline, lines.next_line());
        tokio::pin!(next);
        tokio::select! {
            // The child's own answer is preferred to a cancellation that arrives
            // at the same moment: if the answer is already in hand, the child
            // finished before it was asked to stop, and calling that
            // "cancelled" would throw away a result nobody ever learns was
            // there.
            biased;
            read = &mut next => match read {
                Err(_elapsed) => return Ending::TimedOut,
                Ok(Ok(Some(line))) => match wire::parse(&line) {
                    Ok(ChildMessage::Progress { .. }) => continue,
                    Ok(ChildMessage::Result(outcome)) => return Ending::Answer(outcome),
                    Err(problem) => {
                        return Ending::Violation(format!(
                            "protocol violation: {problem} (the child wrote: {line})"
                        ))
                    }
                },
                Ok(Err(error)) => {
                    return Ending::Violation(format!(
                        "protocol violation: the child's output could not be read: {error}"
                    ))
                }
                Ok(Ok(None)) => {
                    let _ = child.try_wait();
                    return Ending::Exited;
                }
            },
            _ = cancelled.recv() => return Ending::Cancelled,
        }
    }
}

/// Kill the child and everything it started, then reap it.
///
/// Returns the exit status when there is one to have: a child that had already
/// exited when the kill was attempted still reports how it went, which is what
/// the "exited without a result" failure quotes.
async fn stop(child: &mut Child) -> Option<ExitStatus> {
    if let Ok(Some(status)) = child.try_wait() {
        return Some(status);
    }

    kill_tree(child).await;
    match timeout(STOP_BOUND, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
    }
}

/// Kill a child and, where the platform allows it, everything it started.
///
/// A delegated agent may start its own tools, and one still running when the
/// child is put down would outlive it. Killing only the direct child would
/// report the delegation over while its descendants are still working, so on
/// Windows the whole tree goes.
async fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let Some(pid) = child.id() else {
            return;
        };
        let killed = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false);
        if !killed {
            let _ = child.kill().await;
        }
    }
    #[cfg(not(windows))]
    {
        // Reaching the whole process group needs a POSIX API this workspace does
        // not allow itself (unsafe is denied), so on other platforms only the
        // direct child dies and a grandchild may outlive it. The same gap M5
        // recorded for `shell.run`; containment is not a claim this build makes.
        let _ = child.kill().await;
    }
}

/// Drain a pipe into a capped buffer, so a talkative child cannot block on it.
///
/// The cap is a window over the **end** of the stream: once the buffer is full,
/// every chunk that arrives pushes out exactly as many of the oldest bytes. So
/// the cap is a cap, and what a reader is left holding is the child's last
/// words rather than its first ones.
async fn drain(mut pipe: impl tokio::io::AsyncRead + Unpin, kept: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Read through a poison rather than unwrapping it. The buffer
                // holds the child's last words, and the reader that matters
                // (`stderr_note`) must not become a second panic because of a
                // first one elsewhere. Appending bytes to a `Vec` cannot leave
                // the buffer worse than "some bytes arrived", so there is
                // nothing a poison would be protecting; the kernel's live bus
                // reads its own state the same way. (YA-24)
                let mut buffer = kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.len() > MAX_STDERR {
                    let excess = buffer.len() - MAX_STDERR;
                    buffer.drain(..excess);
                }
            }
        }
    }
}

/// A child's own last words, when it left any.
///
/// What `drain` kept is the end of the child's stderr, so this is what it said
/// last: a child that dies mid-sentence is diagnosed by that and not by the
/// opening lines that plainly worked. When the child wrote more than the window
/// holds, the earlier output is gone rather than summarised — the note is the
/// bytes that survived, quoted as the child's own. The window is cut at a byte
/// boundary, so the first character here may be half of one; it is decoded
/// lossily instead of being assumed away.
fn stderr_note(kept: &Arc<Mutex<Vec<u8>>>) -> String {
    // As in `drain`: the buffer is read through a poison. This returns a plain
    // `String`, so there is no error to hand back and nothing to gain from
    // panicking — and the child's last words are what it exists to keep.
    let bytes = kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    if text.is_empty() { String::new() } else { format!("; it said: {text}") }
}

/// What this backend can say about a child, given whether it saw it stop.
///
/// `status` is [`stop`]'s answer. `Some(_)` is the child seen to end, and the
/// message is the ending's own observation and nothing more. `None` is the other
/// case: the kill went out and `wait` did not return inside `STOP_BOUND`, so
/// whether the child — or a grandchild this build cannot reach — is gone is not
/// something this backend can say. Both endings that stop a child end with that
/// admission when it is true. Only the cancellation did before YA-11's sibling,
/// and the timeout claimed a stop it had not confirmed; one function for the
/// sentence is what keeps the two from drifting apart again.
fn stop_message(status: Option<ExitStatus>, observation: &str, note: &str) -> String {
    match status {
        Some(_) => format!("{observation}{note}"),
        None => format!(
            "{observation}; whether it stopped could not be confirmed within {} ms{note}",
            STOP_BOUND.as_millis()
        ),
    }
}

/// How a child that ran out of time reads, once the kill was attempted.
///
/// The deadline leads the sentence because it is this backend's own fact — it
/// enforced the clock. Whether the child then stopped is [`stop_message`]'s to
/// say, and this ending owes a reader the same admission the cancellation does
/// (YA-11's sibling).
fn timeout_outcome(
    status: Option<ExitStatus>,
    deadline_in: Duration,
    note: &str,
) -> SubagentOutcome {
    SubagentOutcome::TimedOut {
        message: stop_message(
            status,
            &format!("the child was still working after {} ms", deadline_in.as_millis()),
            note,
        ),
    }
}

/// How a cancellation reads, once the kill has been attempted.
///
/// The port's contract is the authority here: a backend that put the child down
/// says `Cancelled`, and one that could not confirm it says that instead —
/// "rather than claiming a stop it did not see" (`agent-ports`,
/// `SubagentProvider::cancel`). `None` is the second case, and [`stop_message`]
/// is where both endings that stop a child agree on how to say it (YA-11).
fn cancelled_outcome(status: Option<ExitStatus>, note: &str) -> SubagentOutcome {
    match status {
        Some(_) => SubagentOutcome::Cancelled,
        None => SubagentOutcome::CancelledUncertain {
            message: stop_message(
                None,
                "the child was still running when the parent asked it to stop",
                note,
            ),
        },
    }
}

/// How a child's exit is described when it did not say how it ended.
fn describe_status(status: Option<ExitStatus>) -> String {
    match status {
        Some(status) => match status.code() {
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_string(),
        },
        None => "still running when it was stopped".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use agent_ports::subagent::SubagentStatus;

    use super::*;

    #[test]
    fn a_child_is_given_its_own_variables_and_not_the_agents() {
        let provider = StdioSubagentProvider::new("child", "program").with_env(vec![
            ("CHILD_MODE".to_string(), "test".to_string()),
            ("AGENT_DATA_DIR".to_string(), "/tmp/agent".to_string()),
        ]);

        let env = provider.child_environment();
        let names: Vec<String> =
            env.iter().map(|(name, _)| name.to_string_lossy().into_owned()).collect();

        assert!(names.iter().any(|name| name == "CHILD_MODE"));
        assert!(
            !names.iter().any(|name| name == "AGENT_DATA_DIR"),
            "a configured child was handed the agent's data directory: {names:?}"
        );
    }

    /// A cancellation is `Cancelled` only when this backend saw the child go.
    ///
    /// Why this is a unit test and not one of the live ones next to it: the
    /// branch it pins needs a child that survives being killed for the whole
    /// stop bound, and no fixture can promise that portably — on Windows
    /// `taskkill /T /F` essentially always wins, so a live probe would be
    /// either impossible or a ten-second flake. The *confirmed* side stays
    /// pinned by the live test that cancels a real child
    /// (`tests/stdio_backend.rs`, `cancelling_a_child_takes_what_it_started_with_it`),
    /// so the two halves are covered between them.
    #[test]
    fn a_cancellation_this_backend_could_not_confirm_says_so() {
        let outcome = cancelled_outcome(None, "; it said: still here");

        let SubagentOutcome::CancelledUncertain { message } = outcome else {
            panic!("an unconfirmed stop must not be reported as a stop");
        };
        assert!(
            message.contains(&STOP_BOUND.as_millis().to_string()),
            "it says how long it waited: {message}"
        );
        assert!(message.contains("still here"), "and keeps the child's last words: {message}");

        // And the word reaches the two readers that matter: the fold that names
        // the event's status, and the text a model is shown.
        let outcome = cancelled_outcome(None, "");
        assert_eq!(outcome.status(), SubagentStatus::CancelledUncertain);
        assert!(outcome.describe().contains("unconfirmed"), "{}", outcome.describe());
    }

    /// A timeout whose stop was not confirmed reads as one (YA-11's sibling).
    ///
    /// Same shape as the cancellation above, and pinned here for the same reason:
    /// the deadline branch of `supervise` needs a child that outlives its deadline
    /// *and* survives being killed for the whole stop bound, which no portable
    /// fixture can promise. What it must not do is read like a timeout that ended
    /// cleanly — a caller about to reuse what the child touched reads this
    /// sentence to decide.
    #[test]
    fn a_timeout_this_backend_could_not_confirm_says_so() {
        let outcome = timeout_outcome(None, Duration::from_millis(400), "");
        let SubagentOutcome::TimedOut { ref message } = outcome else {
            panic!("a deadline that passed is a timeout, whatever the stop did");
        };
        assert_eq!(outcome.status(), SubagentStatus::TimedOut);
        assert!(
            message.contains("could not be confirmed within"),
            "an unconfirmed stop is stated, not glossed over: {message}"
        );
        assert!(message.contains("400"), "and the fact it did observe leads: {message}");
    }

    /// A poisoned child table is reported as an error, not taken again as a
    /// panic. Reading through it — the other choice — would answer `Unknown`
    /// for a child that is still there, and a caller deciding whether to reuse
    /// what the child touched cannot tell that from the truth. (YA-24)
    #[test]
    fn a_poisoned_child_table_is_an_error_not_a_panic() {
        let provider = StdioSubagentProvider::new("fake", "no-such-program");

        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = provider.children().expect("a fresh table");
            panic!("a thread died while holding the child table");
        }));
        assert!(died.is_err(), "the premise: the table is poisoned now");

        let handle = SubagentHandle::new("fake", SessionId::from_string("child"), None);
        let refused = provider.receiver(&handle).unwrap_err();
        assert!(matches!(refused, SubagentError::Lost { .. }), "{refused:?}");
    }

    /// The stderr buffer is read through a poison instead: `stderr_note` returns
    /// a plain `String`, so there is no error to hand back — and the child's
    /// last words are the one thing it exists to keep. (YA-24)
    #[test]
    fn a_poisoned_stderr_buffer_still_gives_the_childs_last_words() {
        let kept = Arc::new(Mutex::new(b"boom".to_vec()));

        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = kept.lock().unwrap();
            panic!("a thread died while holding the stderr buffer");
        }));
        assert!(died.is_err(), "the premise: the buffer is poisoned now");

        assert!(stderr_note(&kept).contains("boom"));
    }

    /// And the writing half: `drain` runs in the supervised task, so a poison
    /// there must not kill the task whose job is to keep a talkative child's
    /// stderr from filling its pipe. (YA-24)
    #[tokio::test]
    async fn a_poisoned_stderr_buffer_does_not_stop_the_drain() {
        let kept = Arc::new(Mutex::new(Vec::new()));

        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = kept.lock().unwrap();
            panic!("a thread died while holding the stderr buffer");
        }));
        assert!(died.is_err(), "the premise: the buffer is poisoned now");

        drain(&b"last words"[..], kept.clone()).await;

        let text = kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(text.as_slice(), b"last words");
    }

    /// The window drops the oldest bytes, so what a failure quotes is the end of
    /// what the child said — which is the claim `stderr_note`'s own name makes.
    ///
    /// A unit test rather than a live one for the same reason as the two above:
    /// it needs a child that writes past the cap and then dies, and the fixture
    /// is a real process whose output nothing here controls. What is pinned is
    /// the direction of the window and that the cap is one.
    #[tokio::test]
    async fn a_talkative_childs_buffer_keeps_the_end_not_the_start() {
        let kept = Arc::new(Mutex::new(Vec::new()));

        let mut spoken = b"opening chatter, the part that worked".to_vec();
        spoken.extend_from_slice(&[b'.'; MAX_STDERR]);
        spoken.extend_from_slice(b"the last thing it said");

        drain(&spoken[..], kept.clone()).await;

        let bytes = kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.ends_with("the last thing it said"), "{text}");
        assert!(!text.contains("opening chatter"), "what it said first is gone: {text}");
        assert_eq!(bytes.len(), MAX_STDERR, "and the cap is a cap, not twice one");
    }
}
