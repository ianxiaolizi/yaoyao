//! CLI approve/deny integration: record approval decisions through the command
//! runner, backed by a fake store.

use std::sync::Arc;

use agent_cli::{CliOutput, run_approve, run_deny};
use agent_kernel::approval::ApprovalService;
use agent_kernel::inbox::Inbox;
use agent_protocol::ids::SessionId;
use agent_protocol::inbox::InboxKind;
use agent_protocol::permissions::Capability;
use agent_testkit::FakeEventStore;

fn approvals() -> (ApprovalService, Inbox, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let approvals = ApprovalService::new(store.clone());
    let inbox = Inbox::new(store);
    let sid = SessionId::from_string("approval-cli-session");
    (approvals, inbox, sid)
}

#[tokio::test]
async fn approve_records_a_grant() {
    let (approvals, inbox, sid) = approvals();
    let req = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "src/main.rs")
        .await
        .unwrap();

    let output = run_approve(&approvals, &inbox, &sid, req.approval_id.as_str(), "once", false)
        .await
        .unwrap();
    match output {
        CliOutput::Text(t) => assert!(t.contains("approved"), "unexpected output: {t}"),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn approve_json_reports_decision_and_scope() {
    let (approvals, inbox, sid) = approvals();
    let req = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    let output = run_approve(&approvals, &inbox, &sid, req.approval_id.as_str(), "turn", true)
        .await
        .unwrap();
    match output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert_eq!(value["decision"], "grant");
            assert_eq!(value["scope"], "allow_for_turn");
        }
        _ => panic!("expected json output"),
    }
}

#[tokio::test]
async fn deny_records_a_deny() {
    let (approvals, inbox, sid) = approvals();
    let req = approvals
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "rm -rf /")
        .await
        .unwrap();

    let output =
        run_deny(&approvals, &inbox, &sid, req.approval_id.as_str(), false, false).await.unwrap();
    match output {
        CliOutput::Text(t) => assert!(t.contains("denied"), "unexpected output: {t}"),
        _ => panic!("expected text output"),
    }
}

#[tokio::test]
async fn approve_enqueues_a_wakeup_item() {
    let (approvals, inbox, sid) = approvals();
    let req = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    run_approve(&approvals, &inbox, &sid, req.approval_id.as_str(), "once", false).await.unwrap();

    let pending = inbox.pending(&sid).await.unwrap();
    let wakeups: Vec<_> = pending.iter().filter(|i| i.kind == InboxKind::Wakeup).collect();
    assert_eq!(wakeups.len(), 1);
    assert_eq!(wakeups[0].payload["approval_id"].as_str(), Some(req.approval_id.as_str()));
    assert_eq!(wakeups[0].payload["decision"].as_str(), Some("grant"));
}

#[tokio::test]
async fn approve_with_unknown_scope_errors() {
    let (approvals, inbox, sid) = approvals();
    let req = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    let err = run_approve(&approvals, &inbox, &sid, req.approval_id.as_str(), "forever", false)
        .await
        .unwrap_err();
    assert!(err.contains("unknown scope"), "unexpected error: {err}");
}

#[tokio::test]
async fn session_scope_is_rejected_when_not_configured() {
    let (approvals, inbox, sid) = approvals();
    let req = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    let err = run_approve(&approvals, &inbox, &sid, req.approval_id.as_str(), "session", false)
        .await
        .unwrap_err();
    assert!(err.contains("allow_for_session"), "unexpected error: {err}");
}

/// A service configured the way `apps/agent` configures it — the session scope
/// is what `always` needs, so a test of `always` has to run against one that
/// permits it.
fn session_scoped_approvals() -> (ApprovalService, Inbox, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let approvals = ApprovalService::new(store.clone()).with_session_scope(true);
    let inbox = Inbox::new(store);
    (approvals, inbox, SessionId::from_string("approval-cli-always"))
}

#[tokio::test]
async fn approve_always_settles_the_same_kind_and_wakes_each_one() {
    let (approvals, inbox, sid) = session_scoped_approvals();
    let first = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "a")
        .await
        .unwrap();
    let second = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "b")
        .await
        .unwrap();

    let output = run_approve(&approvals, &inbox, &sid, first.approval_id.as_str(), "always", true)
        .await
        .unwrap();
    match output {
        CliOutput::Json(j) => {
            let value: serde_json::Value = serde_json::from_str(j.as_str()).unwrap();
            assert_eq!(value["scope"], "allow_for_session");
            let settled: Vec<&str> =
                value["settled"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
            assert_eq!(settled.len(), 2, "{settled:?}");
            assert!(settled.contains(&second.approval_id.as_str()));
        }
        _ => panic!("expected json output"),
    }

    let wakeups: Vec<_> = inbox
        .pending(&sid)
        .await
        .unwrap()
        .into_iter()
        .filter(|i| i.kind == InboxKind::Wakeup)
        .collect();
    assert_eq!(wakeups.len(), 2, "each settled request wakes the paused turn");
}

#[tokio::test]
async fn deny_cascades_to_the_sessions_pending_requests() {
    let (approvals, inbox, sid) = approvals();
    let first = approvals
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();
    let second = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    let output =
        run_deny(&approvals, &inbox, &sid, first.approval_id.as_str(), false, false).await.unwrap();
    match output {
        CliOutput::Text(t) => assert!(t.contains("1 other"), "unexpected output: {t}"),
        _ => panic!("expected text output"),
    }
    assert!(approvals.pending(&sid).await.unwrap().is_empty());
    assert!(approvals.lease_for(&sid, &second.approval_id).await.unwrap().is_none());
}

#[tokio::test]
async fn deny_only_leaves_the_other_requests_pending() {
    let (approvals, inbox, sid) = approvals();
    let first = approvals
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();
    let second = approvals
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    let output =
        run_deny(&approvals, &inbox, &sid, first.approval_id.as_str(), true, false).await.unwrap();
    match output {
        CliOutput::Text(t) => assert!(!t.contains("other"), "unexpected output: {t}"),
        _ => panic!("expected text output"),
    }
    let pending = approvals.pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].approval_id, second.approval_id);
}
