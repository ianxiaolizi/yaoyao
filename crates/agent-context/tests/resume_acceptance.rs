//! Acceptance: a call held for approval survives the process that held it.
//!
//! The shape of the test is the shape of the failure it guards against. A turn
//! stops on an approval, the process exits, a *different* process starts over
//! the same log, the decision is made there, and the work finishes — with the
//! side effect having happened exactly once.
//!
//! Two things make this an acceptance test rather than a unit test. The log is
//! a real JSONL file on disk and the second half reads it with nothing in
//! memory from the first, so "durable" is being tested rather than assumed. And
//! the call is a real `fs.edit` on a real file in a real workspace, so "exactly
//! once" is checked against the world instead of against the log's own account
//! of itself. The tool helps here: `fs.edit` replaces exactly one occurrence and
//! refuses when there is nothing left to replace, so a second run of the same
//! call could not silently produce the same result.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_context::ProjectionContextEngine;
use agent_kernel::approval::ApprovalService;
use agent_kernel::guarded::GuardedToolProvider;
use agent_kernel::policy::{PolicyEngine, PolicyRule};
use agent_kernel::recovery::Recovery;
use agent_kernel::resource_guard::ResourceGuard;
use agent_kernel::resume::{Disposition, Resume, ResumeError, ResumeOutcome};
use agent_kernel::scheduler::{RunReport, Scheduler};
use agent_loop::DefaultAgentLoop;
use agent_ports::agent_loop::StopReason;
use agent_ports::event_store::{EventStore, EventStoreError};
use agent_ports::tool_gateway::ToolGateway;
use agent_ports::tool_provider::ToolProvider;
use agent_protocol::events::{Actor, EventType};
use agent_protocol::ids::{ApprovalId, EventSeq, SessionId};
use agent_protocol::inbox::InboxKind;
use agent_protocol::model::MessageRole;
use agent_protocol::model::ModelResponse;
use agent_protocol::permissions::{ApprovalScope, Capability, Workspace};
use agent_protocol::tool::ToolStatus;
use agent_store_jsonl::JsonlEventStore;
use agent_testkit::{FakeModelProvider, InMemoryArtifactStore, one_model_router};
use agent_tools_fs::{FsToolProvider, LocalExecutionEnvironment};
use chrono::Utc;

/// How long a store here waits for a lease a peer is holding.
///
/// The one test that contends holds the lease for the whole of the competing
/// call, so the refusal is certain whatever the wait; a short one only makes it
/// observable sooner. Every other test acquires on the first attempt, where the
/// wait never comes into it.
const LEASE_WAIT: Duration = Duration::from_millis(50);

/// Everything one process assembles, over a log and a workspace that outlive it.
struct Process {
    store: Arc<JsonlEventStore>,
    model: Arc<FakeModelProvider>,
    approvals: Arc<ApprovalService>,
    recovery: Recovery,
    resume: Resume,
    scheduler: Scheduler,
}

/// Assemble a process the way `apps/agent` does, over shared paths.
///
/// The model script is per-process, and that is the honest part: history comes
/// from the log, never from the provider, so the second process knows what the
/// first one was doing only by reading it back.
fn process(data_dir: &Path, workspace: &Path, model: Vec<ModelResponse>) -> Process {
    let store = Arc::new(JsonlEventStore::new(data_dir).with_lease_wait(LEASE_WAIT));
    let approvals = Arc::new(ApprovalService::new(store.clone()));
    let tools: Arc<dyn ToolProvider> =
        Arc::new(FsToolProvider::new(Arc::new(LocalExecutionEnvironment::new(workspace))));
    // A strict configuration rather than the default one: writing is what this
    // session asks about. Everything here is the real gateway, so the approval
    // the test reads was raised by the same code the product runs.
    let gateway = Arc::new(GuardedToolProvider::new(
        tools.clone(),
        PolicyEngine::new()
            .with_rules([PolicyRule::ask(Capability::new(Capability::FS_WRITE))])
            .with_sandbox([Capability::new(Capability::FS_WRITE)]),
        approvals.clone(),
        ResourceGuard::new(Workspace {
            workspace_root: workspace.display().to_string(),
            writable_roots: vec![],
            external_directories: vec![],
        }),
        Arc::new(InMemoryArtifactStore::default()),
    ));
    let suite: Arc<dyn ToolGateway> = gateway.clone();
    let model = Arc::new(FakeModelProvider::new(model));
    // The subject is the approval and resume boundary; the model is a fixed
    // script counted in calls. The thinking pipeline would hold the scripted
    // edit for a branch record the script never writes, which would measure
    // its premise instead of the call surviving a process boundary.
    let loop_ = Arc::new(
        DefaultAgentLoop::new(
            model.clone(),
            suite,
            Arc::new(ProjectionContextEngine::new(store.clone(), data_dir)),
            one_model_router(),
        )
        .without_judgment(),
    );
    Process {
        scheduler: Scheduler::new(store.clone(), loop_, 8),
        recovery: Recovery::new(store.clone(), tools),
        resume: Resume::new(store.clone(), approvals.clone(), gateway),
        store,
        model,
        approvals,
    }
}

impl Process {
    async fn ask(&self, session_id: &SessionId, text: &str) -> Option<RunReport> {
        self.scheduler
            .inbox()
            .append(
                session_id,
                InboxKind::NextTurn,
                serde_json::json!({ "text": text }),
                0,
                Actor::User,
            )
            .await
            .expect("append input");
        self.scheduler.run_pending(session_id, Utc::now()).await.expect("run pending")
    }

    async fn log(&self, session_id: &SessionId) -> Vec<agent_protocol::events::StoredEvent> {
        self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await.expect("read the log")
    }

    async fn count(&self, session_id: &SessionId, event_type: &str) -> usize {
        self.log(session_id)
            .await
            .into_iter()
            .filter(|e| e.event.event_type.as_str() == event_type)
            .count()
    }
}

/// The tool call's answer, as the model was shown it.
///
/// A tool message is a `tool_result` block rather than text, so reading it as
/// text would find nothing and pass for the wrong reason.
fn tool_answer(messages: &[agent_protocol::model::ModelMessage]) -> (String, bool) {
    messages
        .iter()
        .find(|m| m.role == MessageRole::Tool)
        .and_then(|m| m.content.iter().find_map(|b| b.as_tool_result()))
        .map(|(_, _, content, is_error)| (content.to_string(), is_error))
        .expect("the proposed call is answered in the history the model was given")
}

fn notes(workspace: &Path) -> String {
    std::fs::read_to_string(workspace.join("notes.txt")).expect("read notes.txt")
}

#[tokio::test]
async fn a_call_held_for_approval_survives_the_process_that_held_it() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.txt"), "placeholder\n").expect("seed");
    let session_id = SessionId::from_string("resume-acceptance");

    // --- Process one: the model proposes an edit, the policy holds it. ---
    let first = process(
        data_dir.path(),
        workspace.path(),
        vec![FakeModelProvider::tool_call_with_id(
            "fs.edit",
            serde_json::json!({
                "path": "notes.txt",
                "oldString": "placeholder",
                "newString": "done",
            }),
            "call_edit".to_string(),
        )],
    );
    let held = first.ask(&session_id, "mark the note done").await.expect("a run");
    assert_eq!(held.stop_reason, StopReason::NeedsUserInput);
    assert_eq!(notes(workspace.path()), "placeholder\n", "waiting is not running");

    let pending = first.approvals.pending(&session_id).await.expect("pending list");
    assert_eq!(pending.len(), 1, "the call is held, and the decision is durable");
    let approval_id = pending[0].approval_id.clone();
    drop(first); // the process exits with the call still unanswered

    // --- Process two: a fresh assembly over the same log and workspace. ---
    let second = process(
        data_dir.path(),
        workspace.path(),
        vec![FakeModelProvider::answer("the note says done now")],
    );
    second.recovery.recover(&session_id).await.expect("recovery");
    second
        .approvals
        .grant(&session_id, &approval_id, ApprovalScope::AllowOnce, "user")
        .await
        .expect("grant");

    let outcome = second.resume.resume(&session_id, "carry on").await.expect("resume");
    assert!(
        matches!(
            outcome,
            ResumeOutcome::Settled {
                disposition: Disposition::Ran { status: ToolStatus::Completed },
                ..
            }
        ),
        "the granted call should run: {outcome:?}"
    );

    // The world, not the self-report: the file changed, once. A second run of
    // this call would have found no `placeholder` left to replace.
    assert_eq!(notes(workspace.path()), "done\n");
    assert_eq!(second.count(&session_id, EventType::TOOL_COMPLETED).await, 1);
    assert_eq!(
        second.count(&session_id, EventType::TOOL_OUTCOME_UNKNOWN).await,
        0,
        "nothing was left unsettled"
    );

    // --- The turn carries on, reading a transcript the call is answered in. ---
    let report =
        second.scheduler.run_pending(&session_id, Utc::now()).await.expect("run").expect("a run");
    assert_eq!(report.stop_reason, StopReason::FinalAnswer);
    assert_eq!(report.final_content.as_deref(), Some("the note says done now"));

    let request = second.model.requests().pop().expect("a model request");
    let (content, is_error) = tool_answer(&request.messages);
    assert_eq!(content, "edited notes.txt", "the answer is the real result, not a summary of it");
    assert!(!is_error, "and it is a result, not an error");

    // And the wait is over for good: resuming again does nothing at all.
    assert_eq!(
        second.resume.resume(&session_id, "carry on").await.expect("resume"),
        ResumeOutcome::NothingToResume
    );
    assert_eq!(notes(workspace.path()), "done\n");
}

/// Put a session in the state this file is about: one call held for approval.
async fn held_call(data_dir: &Path, workspace: &Path, session_id: &SessionId) -> ApprovalId {
    let holder = process(
        data_dir,
        workspace,
        vec![FakeModelProvider::tool_call_with_id(
            "fs.edit",
            serde_json::json!({
                "path": "notes.txt",
                "oldString": "placeholder",
                "newString": "done",
            }),
            "call_edit".to_string(),
        )],
    );
    holder.ask(session_id, "mark the note done").await.expect("a run");
    let approval_id =
        holder.approvals.pending(session_id).await.expect("pending")[0].approval_id.clone();
    drop(holder);
    approval_id
}

/// A resume cannot act while another holder has the session.
///
/// Deterministic on purpose. A test that starts two resumes and hopes they
/// overlap passes by luck when they do not — and this is the one property that
/// must not be left to luck. Holding the lease by hand *is* the competing
/// process, with no scheduling to be lucky about.
///
/// The race was written the lucky way first, with two threads and a real
/// `std::thread::scope`. It passed both with and without the lease in `resume`:
/// the winner finished before the loser was scheduled, so the window it was
/// aimed at never opened. It was deleted rather than kept green. Removing the
/// lease from `resume` is what this test fails on.
#[tokio::test]
async fn a_resume_does_not_act_while_another_holder_has_the_session() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.txt"), "placeholder\n").expect("seed");
    let session_id = SessionId::from_string("resume-lease");
    let approval_id = held_call(data_dir.path(), workspace.path(), &session_id).await;

    let process = process(data_dir.path(), workspace.path(), vec![]);
    process
        .approvals
        .grant(&session_id, &approval_id, ApprovalScope::AllowOnce, "user")
        .await
        .expect("grant");

    // Someone else is mid-critical-section. The competing holder has to be a
    // *separate* store instance — a lease is re-entrant within one, which is
    // what lets a resume append while holding its own.
    let other = JsonlEventStore::new(data_dir.path().to_path_buf()).with_lease_wait(LEASE_WAIT);
    let lease = other.lease(&session_id).await.expect("lease");
    let refused = process.resume.resume(&session_id, "carry on").await;
    assert!(
        matches!(refused, Err(ResumeError::Store(EventStoreError::ConcurrentWriter(_)))),
        "a resume must not run a side effect under someone else's lease: {refused:?}"
    );
    assert_eq!(notes(workspace.path()), "placeholder\n", "nothing ran, so nothing changed");
    assert_eq!(process.count(&session_id, EventType::TOOL_COMPLETED).await, 0);

    // Let go, and the very same call goes through — so the refusal above was
    // the lease and not some unrelated failure to resume.
    drop(lease);
    let settled = process.resume.resume(&session_id, "carry on").await.expect("resume");
    assert!(matches!(settled, ResumeOutcome::Settled { .. }), "{settled:?}");
    assert_eq!(notes(workspace.path()), "done\n");
}

#[tokio::test]
async fn a_denied_call_is_answered_in_the_history_and_never_touches_the_world() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.txt"), "placeholder\n").expect("seed");
    let session_id = SessionId::from_string("resume-denied");

    let first = process(
        data_dir.path(),
        workspace.path(),
        vec![FakeModelProvider::tool_call_with_id(
            "fs.edit",
            serde_json::json!({
                "path": "notes.txt",
                "oldString": "placeholder",
                "newString": "done",
            }),
            "call_edit".to_string(),
        )],
    );
    first.ask(&session_id, "mark the note done").await.expect("a run");
    let approval_id =
        first.approvals.pending(&session_id).await.expect("pending")[0].approval_id.clone();
    drop(first);

    // The second process is told no. The turn must still be able to continue:
    // the model proposed a call, and something has to answer it.
    let second = process(
        data_dir.path(),
        workspace.path(),
        vec![FakeModelProvider::answer("understood, leaving it alone")],
    );
    second.recovery.recover(&session_id).await.expect("recovery");
    second.approvals.deny(&session_id, &approval_id, "user").await.expect("deny");
    second.resume.resume(&session_id, "the user said no").await.expect("resume");

    assert_eq!(notes(workspace.path()), "placeholder\n", "a refusal changes nothing");
    let report =
        second.scheduler.run_pending(&session_id, Utc::now()).await.expect("run").expect("a run");
    assert_eq!(report.stop_reason, StopReason::FinalAnswer);

    let request = second.model.requests().pop().expect("a model request");
    let (content, is_error) = tool_answer(&request.messages);
    assert!(content.contains("denied"), "the model is told what happened: {content}");
    assert!(is_error, "a refusal is not a result");

    // The whole session ran with the tool never invoked: the log shows the
    // dispatch the loop recorded, and no outcome that could only come from a run.
    assert_eq!(second.count(&session_id, EventType::TOOL_COMPLETED).await, 0);
}
