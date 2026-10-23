//! The doctor's checks: what this build can say about the state it keeps.
//!
//! Each check answers one question about the durable state under the data
//! directory, one message per session, so a failure names the session and the
//! class rather than only the build. The classes are the corruption shapes the
//! store can actually produce (YA-45): a torn tail, a sequence gap, an entity
//! left open by a crash, a checkpoint whose state the log does not support, and
//! a configuration the profile cannot be resolved from. `build_schema_version`
//! is the sixth and `execution_sandbox` the seventh; both judge the build
//! rather than a log, and the last one reports what the selected backend
//! actually bounds.
//!
//! Two rules shape every session check. First, the session is **leased with a
//! zero wait** before it is read: a log another process is writing must not be
//! judged as torn, and opening it (which is what repairs a torn tail) must not
//! truncate a line that is in flight. A session whose lease cannot be taken is
//! `Skipped`, not `Failed` — there was no evidence, not bad evidence. Second,
//! the checks are read-only apart from that repair: the doctor reports what it
//! finds and never rewrites a log or drops a checkpoint, because a diagnostic
//! that repairs what it inspects cannot be run twice to the same answer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_cli::args::Args;
use agent_kernel::lifecycle;
use agent_ports::doctor::DoctorCheck;
use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::execution_environment::SandboxMode;
use agent_projection::{CheckpointVerdict, ModelContextProjection, Projection, SessionProjection};
use agent_protocol::diagnostics::{DiagnosticMessage, HealthCheckId, HealthStatus};
use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId};
use agent_store_jsonl::JsonlEventStore;
use agent_tools_fs::{
    MAX_FILE_BYTES, MAX_OUTPUT_BYTES, SHELL_TIMEOUT_MS, app_container_shell,
    app_container_shell_supports_and_and,
};
use agent_tools_git::GIT_TIMEOUT_MS;

use crate::{config, sandbox};

/// A doctor check that reports the binary's protocol/build schema version.
pub struct BuildHealthCheck {
    schema_version: SchemaVersion,
}

impl BuildHealthCheck {
    pub fn new(schema_version: SchemaVersion) -> Self {
        Self { schema_version }
    }
}

#[async_trait::async_trait]
impl DoctorCheck for BuildHealthCheck {
    fn id(&self) -> HealthCheckId {
        HealthCheckId::new("build_schema_version")
    }

    async fn run(&self) -> Vec<DiagnosticMessage> {
        vec![DiagnosticMessage {
            check_id: self.id(),
            status: HealthStatus::Passed,
            message: format!("schema_version={}", self.schema_version),
        }]
    }
}

/// The session ids under `data_dir`: every directory that holds an
/// `events.jsonl`.
///
/// The store's own layout is the index — there is no registry that could drift
/// from it — and a directory without a log is not a session, because the store
/// creates the log the first time it opens one.
fn session_ids(data_dir: &Path) -> std::io::Result<Vec<SessionId>> {
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if !path.is_dir() || !path.join("events.jsonl").is_file() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            ids.push(SessionId::from_string(name));
        }
    }
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(ids)
}

/// Bytes of the log ending at the last complete line.
///
/// A trailing fragment without a newline is not a record: the store treats it
/// as a torn tail and drops it, so the sequence check must not read it either.
fn complete_len(bytes: &[u8]) -> usize {
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(last_newline) => last_newline + 1,
        None => 0,
    }
}

/// One check over every session under the data directory.
pub struct SessionHealthCheck {
    id: HealthCheckId,
    store: Arc<JsonlEventStore>,
    data_dir: PathBuf,
    kind: Kind,
}

#[derive(Clone, Copy)]
enum Kind {
    LogTail,
    SeqContiguity,
    OpenEntities,
    CheckpointConsistency,
}

impl SessionHealthCheck {
    /// Whether the log carried a torn trailing fragment.
    ///
    /// The store repairs a torn tail when it opens the log, so this must be the
    /// first check to open a session: it reports the repair the open performed.
    /// A repair another process already made is invisible — a dropped fragment
    /// leaves nothing to read back — and that limit is the store's to state.
    pub fn log_tail(store: Arc<JsonlEventStore>, data_dir: impl Into<PathBuf>) -> Self {
        Self::new("log_tail", store, data_dir.into(), Kind::LogTail)
    }

    /// Whether the durable sequence numbers are 0, 1, 2, … with no gap.
    pub fn seq_contiguity(store: Arc<JsonlEventStore>, data_dir: impl Into<PathBuf>) -> Self {
        Self::new("seq_contiguity", store, data_dir.into(), Kind::SeqContiguity)
    }

    /// Whether any run, turn, or step is open with no terminal event.
    pub fn open_entities(store: Arc<JsonlEventStore>, data_dir: impl Into<PathBuf>) -> Self {
        Self::new("open_entities", store, data_dir.into(), Kind::OpenEntities)
    }

    /// Whether each projection checkpoint's state is what folding the log up to
    /// its own sequence produces.
    pub fn checkpoint_consistency(
        store: Arc<JsonlEventStore>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self::new("checkpoint_consistency", store, data_dir.into(), Kind::CheckpointConsistency)
    }

    fn new(id: &str, store: Arc<JsonlEventStore>, data_dir: PathBuf, kind: Kind) -> Self {
        Self { id: HealthCheckId::new(id), store, data_dir, kind }
    }

    fn message(
        &self,
        status: HealthStatus,
        session: &SessionId,
        detail: String,
    ) -> DiagnosticMessage {
        DiagnosticMessage {
            check_id: self.id.clone(),
            status,
            message: format!("session '{}': {detail}", session.as_str()),
        }
    }

    async fn judge(&self, session: &SessionId) -> DiagnosticMessage {
        // Zero-wait lease: a live writer is "not judged", never "judged torn".
        // The guard lives to the end of this method, so the read below cannot
        // race an append.
        let _lease = match self.store.lease(session).await {
            Ok(lease) => lease,
            Err(EventStoreError::ConcurrentWriter(_)) => {
                return self.message(
                    HealthStatus::Skipped,
                    session,
                    "skipped, another process holds the session lease".to_string(),
                );
            }
            Err(e) => {
                return self.message(
                    HealthStatus::Failed,
                    session,
                    format!("cannot take the session lease: {e}"),
                );
            }
        };

        match self.kind {
            Kind::LogTail => self.judge_tail(session),
            Kind::SeqContiguity => self.judge_seq(session),
            Kind::OpenEntities => self.judge_entities(session).await,
            Kind::CheckpointConsistency => self.judge_checkpoints(session).await,
        }
    }

    fn judge_tail(&self, session: &SessionId) -> DiagnosticMessage {
        match self.store.repaired_tail(session) {
            Ok(None) => self.message(HealthStatus::Passed, session, "tail intact".to_string()),
            Ok(Some(bytes)) => self.message(
                HealthStatus::Failed,
                session,
                format!(
                    "the log ended in a torn fragment; {bytes} bytes were dropped and the \
                     file truncated (a write was interrupted)"
                ),
            ),
            Err(e) => {
                self.message(HealthStatus::Failed, session, format!("cannot open the log: {e}"))
            }
        }
    }

    fn judge_seq(&self, session: &SessionId) -> DiagnosticMessage {
        let path = self.data_dir.join(session.as_str()).join("events.jsonl");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return self.message(
                    HealthStatus::Failed,
                    session,
                    format!("cannot read the log: {e}"),
                );
            }
        };

        let mut expected = 0u64;
        for (index, line) in bytes[..complete_len(&bytes)]
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            let seq = serde_json::from_slice::<serde_json::Value>(line)
                .ok()
                .and_then(|event| event.get("seq").and_then(|seq| seq.as_u64()));
            match seq {
                Some(seq) if seq == expected => expected += 1,
                Some(seq) => {
                    return self.message(
                        HealthStatus::Failed,
                        session,
                        format!(
                            "seq gap: line {} carries seq {seq}, expected {expected}",
                            index + 1
                        ),
                    );
                }
                None => {
                    return self.message(
                        HealthStatus::Failed,
                        session,
                        format!("line {} is not a readable event", index + 1),
                    );
                }
            }
        }

        let detail = match expected {
            0 => "no events recorded".to_string(),
            count => format!("{count} events, seq 0..{} contiguous", count - 1),
        };
        self.message(HealthStatus::Passed, session, detail)
    }

    async fn judge_entities(&self, session: &SessionId) -> DiagnosticMessage {
        let events = match self.store.read_stream(session, EventSeq::ZERO, usize::MAX).await {
            Ok(events) => events,
            Err(e) => {
                return self.message(
                    HealthStatus::Failed,
                    session,
                    format!("cannot read the log: {e}"),
                );
            }
        };
        let state = lifecycle::fold(&events);

        // A yielded turn is paused on purpose: recovery leaves its step, turn,
        // and run open for `resume` to carry forward, so reporting them here
        // would call a session waiting for a person a crash.
        if let Some(turn) = state.yielded_turn {
            return self.message(
                HealthStatus::Skipped,
                session,
                format!("paused at turn {}, waiting for a decision", turn.as_str()),
            );
        }

        let mut open = Vec::new();
        if let Some(step) = state.open_step {
            open.push(format!("step {}", step.as_str()));
        }
        if let Some(turn) = state.open_turn {
            open.push(format!("turn {}", turn.as_str()));
        }
        if let Some(run) = state.open_run {
            open.push(format!("run {}", run.as_str()));
        }
        if open.is_empty() {
            self.message(HealthStatus::Passed, session, "no entity left open".to_string())
        } else {
            self.message(
                HealthStatus::Failed,
                session,
                format!(
                    "open with no terminal event: {} (a crash, or recovery never ran)",
                    open.join(", ")
                ),
            )
        }
    }

    async fn judge_checkpoints(&self, session: &SessionId) -> DiagnosticMessage {
        let store: Arc<dyn EventStore> = self.store.clone();
        let session_projection =
            Projection::new(SessionProjection::new(), store.clone(), self.data_dir.clone());
        let context_projection =
            Projection::new(ModelContextProjection::new(), store, self.data_dir.clone());

        let verdicts = [
            ("session", session_projection.check_checkpoint(session).await),
            ("model-context", context_projection.check_checkpoint(session).await),
        ];

        let mut failures = Vec::new();
        let mut details = Vec::new();
        for (key, verdict) in verdicts {
            match verdict {
                Ok(CheckpointVerdict::Diverges { as_of_seq }) => failures.push(format!(
                    "{key} checkpoint diverges at seq {}: the log does not support its state",
                    described_seq(as_of_seq)
                )),
                Ok(verdict) => details.push(format!("{key}: {}", describe_verdict(&verdict))),
                Err(e) => failures.push(format!("cannot compare the {key} checkpoint: {e}")),
            }
        }

        if failures.is_empty() {
            self.message(
                HealthStatus::Passed,
                session,
                format!("checkpoints agree ({})", details.join(", ")),
            )
        } else {
            self.message(HealthStatus::Failed, session, failures.join("; "))
        }
    }
}

#[async_trait::async_trait]
impl DoctorCheck for SessionHealthCheck {
    fn id(&self) -> HealthCheckId {
        self.id.clone()
    }

    async fn run(&self) -> Vec<DiagnosticMessage> {
        let sessions = match session_ids(&self.data_dir) {
            Ok(sessions) => sessions,
            Err(e) => {
                return vec![DiagnosticMessage {
                    check_id: self.id.clone(),
                    status: HealthStatus::Failed,
                    message: format!("cannot list sessions under {}: {e}", self.data_dir.display()),
                }];
            }
        };
        if sessions.is_empty() {
            return vec![DiagnosticMessage {
                check_id: self.id.clone(),
                status: HealthStatus::Skipped,
                message: format!("no sessions under {}", self.data_dir.display()),
            }];
        }

        let mut messages = Vec::with_capacity(sessions.len());
        for session in &sessions {
            messages.push(self.judge(session).await);
        }
        messages
    }
}

fn describe_verdict(verdict: &CheckpointVerdict) -> String {
    match verdict {
        CheckpointVerdict::Absent => "absent".to_string(),
        CheckpointVerdict::Unusable => "unusable, ignored".to_string(),
        CheckpointVerdict::Ungrounded { claimed } => {
            format!("ungrounded at seq {}, ignored", claimed.as_u64())
        }
        CheckpointVerdict::Matches { as_of_seq } => {
            format!("matches at seq {}", described_seq(*as_of_seq))
        }
        CheckpointVerdict::Diverges { as_of_seq } => {
            format!("diverges at seq {}", described_seq(*as_of_seq))
        }
    }
}

fn described_seq(seq: Option<EventSeq>) -> String {
    match seq {
        Some(seq) => seq.as_u64().to_string(),
        None => "start".to_string(),
    }
}

/// A doctor check that reports whether the configuration resolves.
pub struct ProfileHealthCheck {
    workspace_root: String,
    args: Args,
}

impl ProfileHealthCheck {
    /// The doctor judges the same configuration every command assembles under.
    pub fn new(workspace_root: impl Into<String>, args: Args) -> Self {
        Self { workspace_root: workspace_root.into(), args }
    }
}

#[async_trait::async_trait]
impl DoctorCheck for ProfileHealthCheck {
    fn id(&self) -> HealthCheckId {
        HealthCheckId::new("profile_resolution")
    }

    async fn run(&self) -> Vec<DiagnosticMessage> {
        let (status, message) = match config::load(&self.workspace_root, &self.args) {
            Ok(resolved) => {
                (HealthStatus::Passed, format!("profile '{}' resolves", resolved.profile_id))
            }
            Err(e) => (HealthStatus::Failed, format!("the configuration does not resolve: {e}")),
        };
        vec![DiagnosticMessage { check_id: self.id(), status, message }]
    }
}

/// A doctor check that reports the execution backend the host selected.
///
/// The mode is a host decision (`AGENT_SANDBOX`), so the doctor reads it the
/// same way a run does and says what it means for this build: the numbers come
/// from the constants the environment applies, and the message names what the
/// mode does **not** isolate. A run that selected a backend this build does not
/// have is a failure — the run would refuse to start, and a doctor that passed
/// it would be describing a machine that cannot run.
pub struct ExecutionSandboxCheck {
    selection: Result<SandboxMode, sandbox::SandboxError>,
    network: Result<bool, sandbox::SandboxError>,
}

impl ExecutionSandboxCheck {
    /// The backend and network policy the host environment selects.
    pub fn from_environment() -> Self {
        Self {
            selection: sandbox::from_environment(),
            network: sandbox::network_from_environment(),
        }
    }
}

#[async_trait::async_trait]
impl DoctorCheck for ExecutionSandboxCheck {
    fn id(&self) -> HealthCheckId {
        HealthCheckId::new("execution_sandbox")
    }

    async fn run(&self) -> Vec<DiagnosticMessage> {
        let (status, message) = match &self.selection {
            Ok(SandboxMode::ReadOnly) => (
                HealthStatus::Passed,
                format!(
                    "mode=read-only; file reads and memory search only; refused: fs.write \
                     process.spawn network.request; limits: file_cap_bytes={MAX_FILE_BYTES} \
                     (no command, so no shell or git timeout applies)"
                ),
            ),
            Ok(SandboxMode::LocalProcess) => (
                HealthStatus::Passed,
                format!(
                    "mode=local; file tools and command cwd are confined to the workspace and \
                     the child environment is allowlisted; not isolated: a command runs as this \
                     user, so absolute paths it opens and the network are not bounded; limits: \
                     shell_timeout_ms={SHELL_TIMEOUT_MS} git_timeout_ms={GIT_TIMEOUT_MS} \
                     output_cap_bytes={MAX_OUTPUT_BYTES} file_cap_bytes={MAX_FILE_BYTES}"
                ),
            ),
            Ok(SandboxMode::WorkspaceSandbox) => {
                let network = match &self.network {
                    Ok(allowed) => sandbox::network_label(*allowed),
                    Err(error) => {
                        return vec![DiagnosticMessage {
                            check_id: self.id(),
                            status: HealthStatus::Failed,
                            message: error.to_string(),
                        }];
                    }
                };
                // Read-only probe: derive the SID and read the null-device DACL.
                // It creates no profile and spawns nothing, so running the
                // doctor cannot change the machine it is examining.
                match agent_sandbox_win::status(agent_sandbox_win::PROFILE_NAME) {
                    Ok(status) if status.nul_granted => (
                        HealthStatus::Passed,
                        format!(
                            "mode=app-container; commands run in an AppContainer package \
                             (profile={} sid={}) with a kill-on-close job object and an \
                             allowlisted environment; reads and writes outside the granted \
                             roots are refused by the kernel (files Windows itself opens to \
                             every AppContainer, some system files, remain readable); \
                             network={network}; shell={} ({}); \
                             the file tools answer from a helper process inside the container; \
                             not isolated: container commands can write anywhere they were \
                             granted; limits: \
                             shell_timeout_ms={SHELL_TIMEOUT_MS} git_timeout_ms={GIT_TIMEOUT_MS} \
                             output_cap_bytes={MAX_OUTPUT_BYTES} file_cap_bytes={MAX_FILE_BYTES}",
                            status.profile,
                            status.sid,
                            app_container_shell(),
                            if app_container_shell_supports_and_and() {
                                "supports && and ||"
                            } else {
                                "PowerShell 5.1: no && or ||"
                            },
                        ),
                    ),
                    Ok(status) => (
                        HealthStatus::Failed,
                        format!(
                            "mode=app-container; profile={} sid={} but its setup is not in \
                             place: the null-device grant is missing. Windows resets that \
                             device's security descriptor at every boot, so a machine set up \
                             before needs `agent sandbox setup` run again from an elevated \
                             prompt, then the doctor again",
                            status.profile, status.sid
                        ),
                    ),
                    Err(error) => (HealthStatus::Failed, format!("mode=app-container; {error}")),
                }
            }
            Ok(other) => (
                HealthStatus::Failed,
                format!(
                    "mode={}; this build has no backend for it, so runs selecting it will not start",
                    sandbox::label(*other)
                ),
            ),
            Err(error) => (HealthStatus::Failed, error.to_string()),
        };
        vec![DiagnosticMessage { check_id: self.id(), status, message }]
    }
}
