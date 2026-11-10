//! Tests for the recovery point a call that may change files is made under.
//!
//! The subject is an ordering again, one layer over the secret tests: the
//! snapshot is *taken* before the tool runs, *compared* after it, and a capture
//! that could not be taken stops the call before the provider sees it. None of
//! these tests touch a real directory — the adapter has its own suite — they
//! drive the gateway against a snapshotter that only records what it was asked.

use super::*;
use agent_ports::event_store::EventStore;
use agent_ports::workspace_snapshot::{SnapshotError, SnapshotRequest, WorkspaceSnapshotter};
use agent_protocol::events::EventType;
use agent_protocol::ids::{EventSeq, SnapshotId};
use agent_protocol::workspace_snapshot::{
    ChangeManifest, RollbackReport, SnapshotManifest, WorkspaceSnapshotScope,
};
use chrono::Utc;

/// A snapshotter that records the calls and answers with a manifest the gateway
/// could have built itself.
struct RecordingSnapshotter {
    order: Arc<Mutex<Vec<&'static str>>>,
    captures: Mutex<Vec<SnapshotRequest>>,
    diffs: Mutex<usize>,
    refuse: Mutex<Option<String>>,
}

impl RecordingSnapshotter {
    fn new(order: Arc<Mutex<Vec<&'static str>>>) -> Arc<Self> {
        Arc::new(Self {
            order,
            captures: Mutex::new(Vec::new()),
            diffs: Mutex::new(0),
            refuse: Mutex::new(None),
        })
    }

    fn refusing(order: Arc<Mutex<Vec<&'static str>>>, reason: &str) -> Arc<Self> {
        let snapshotter = Self::new(order);
        *snapshotter.refuse.lock().unwrap() = Some(reason.to_string());
        snapshotter
    }

    fn captures(&self) -> Vec<SnapshotRequest> {
        self.captures.lock().unwrap().clone()
    }

    fn diffs(&self) -> usize {
        *self.diffs.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl WorkspaceSnapshotter for RecordingSnapshotter {
    async fn capture(&self, request: SnapshotRequest) -> Result<SnapshotManifest, SnapshotError> {
        if let Some(reason) = self.refuse.lock().unwrap().clone() {
            return Err(SnapshotError::OverBudget { reason });
        }
        self.order.lock().unwrap().push("capture");
        self.captures.lock().unwrap().push(request.clone());
        Ok(SnapshotManifest {
            snapshot_id: SnapshotId::new(),
            session_id: request.session_id,
            task_id: request.task_id,
            action_digest: request.action_digest,
            manifest_hash: SnapshotManifest::digest_scope(&request.scope, &[]),
            scope: request.scope,
            entries: Vec::new(),
            unrecoverable_scope: Vec::new(),
            total_bytes: 0,
            complete: true,
            created_at: Utc::now(),
        })
    }

    async fn diff(&self, manifest: &SnapshotManifest) -> Result<ChangeManifest, SnapshotError> {
        self.order.lock().unwrap().push("diff");
        *self.diffs.lock().unwrap() += 1;
        Ok(ChangeManifest {
            snapshot_id: manifest.snapshot_id.clone(),
            session_id: manifest.session_id.clone(),
            task_id: manifest.task_id.clone(),
            action_digest: manifest.action_digest.clone(),
            changes: Vec::new(),
            changes_hash: ChangeManifest::digest_changes(&[]),
            complete: true,
            created_at: Utc::now(),
        })
    }

    async fn restore(&self, snapshot_id: &str) -> Result<RollbackReport, SnapshotError> {
        Err(SnapshotError::NotFound { snapshot_id: snapshot_id.to_string() })
    }
}

/// A provider that records when it ran, so the order against the snapshotter is
/// one list rather than two clocks.
struct OrderedTools {
    order: Arc<Mutex<Vec<&'static str>>>,
    defs: Vec<ToolDefinition>,
}

#[async_trait::async_trait]
impl ToolProvider for OrderedTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.defs.clone()
    }

    fn side_effect(&self, tool_name: &str) -> Option<ToolSideEffect> {
        self.defs.iter().find(|d| d.name == tool_name).map(|d| d.side_effect)
    }

    async fn invoke(&self, _: &ToolInvocation) -> Result<ToolResult, ToolProviderError> {
        self.order.lock().unwrap().push("invoke");
        Ok(ToolResult::completed("ran"))
    }
}

fn snapshot_harness(
    tools: Arc<OrderedTools>,
    snapshotter: Arc<RecordingSnapshotter>,
    capability: &str,
) -> (GuardedToolProvider, Arc<FakeEventStore>) {
    let store = Arc::new(FakeEventStore::new());
    let gateway = GuardedToolProvider::new(
        tools,
        allowing(capability),
        Arc::new(ApprovalService::new(store.clone())),
        ResourceGuard::new(workspace()),
        Arc::new(agent_testkit::InMemoryArtifactStore::default()),
    )
    .with_snapshotter(snapshotter)
    .with_permission_service(Arc::new(crate::permission::PermissionService::new(store.clone())));
    (gateway, store)
}

fn write_tools(order: Arc<Mutex<Vec<&'static str>>>) -> Arc<OrderedTools> {
    Arc::new(OrderedTools {
        order,
        defs: vec![def("fs.write", &[Capability::FS_WRITE], ToolSideEffect::Write)],
    })
}

fn read_tools(order: Arc<Mutex<Vec<&'static str>>>) -> Arc<OrderedTools> {
    Arc::new(OrderedTools {
        order,
        defs: vec![def("fs.read", &[Capability::FS_READ], ToolSideEffect::Read)],
    })
}

fn write_invocation() -> ToolInvocation {
    ToolInvocation::proposed("fs.write", serde_json::json!({ "path": "notes.txt", "content": "x" }))
}

async fn payloads(store: &Arc<FakeEventStore>, event_type: &str) -> Vec<serde_json::Value> {
    let events = store.read_stream(&sid(), EventSeq::ZERO, usize::MAX).await.unwrap();
    events
        .iter()
        .filter(|stored| stored.event.event_type.as_str() == event_type)
        .map(|stored| stored.event.payload.clone())
        .collect()
}

#[tokio::test]
async fn a_write_is_snapshotted_before_it_runs() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let snapshotter = RecordingSnapshotter::new(order.clone());
    let (gateway, store) =
        snapshot_harness(write_tools(order.clone()), snapshotter.clone(), Capability::FS_WRITE);

    gateway.execute(&sid(), &write_invocation()).await.unwrap();

    assert_eq!(
        *order.lock().unwrap(),
        vec!["capture", "invoke", "diff"],
        "the recovery point comes first and the comparison last"
    );
    let captures = snapshotter.captures();
    assert_eq!(captures.len(), 1);
    assert_eq!(
        captures[0].scope,
        WorkspaceSnapshotScope::Paths { paths: vec!["notes.txt".to_string()] }
    );
    let created = payloads(&store, EventType::WORKSPACE_SNAPSHOT_CREATED).await;
    assert_eq!(created.len(), 1, "the capture is written down before the call");
    assert_eq!(created[0]["session_id"], sid().to_string());
    assert_eq!(
        created[0]["action_digest"].as_str().map(str::len),
        Some(64),
        "the snapshot is bound to the action digest"
    );
}

#[tokio::test]
async fn a_read_is_not_snapshotted() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let snapshotter = RecordingSnapshotter::new(order.clone());
    let (gateway, _store) =
        snapshot_harness(read_tools(order.clone()), snapshotter.clone(), Capability::FS_READ);

    gateway
        .execute(
            &sid(),
            &ToolInvocation::proposed("fs.read", serde_json::json!({ "path": "a.txt" })),
        )
        .await
        .unwrap();

    assert!(snapshotter.captures().is_empty());
    assert_eq!(snapshotter.diffs(), 0);
}

#[tokio::test]
async fn a_recovery_point_that_cannot_be_taken_stops_the_call() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let snapshotter =
        RecordingSnapshotter::refusing(order.clone(), "more than 4096 entries under the workspace");
    let tools = write_tools(order.clone());
    let (gateway, store) = snapshot_harness(tools.clone(), snapshotter, Capability::FS_WRITE);

    let refused = gateway.execute(&sid(), &write_invocation()).await.unwrap_err();
    assert!(
        matches!(refused, ToolGatewayError::SnapshotRefused { ref reason, .. } if reason.contains("4096")),
        "{refused:?}"
    );
    assert_eq!(*order.lock().unwrap(), Vec::<&'static str>::new(), "the tool never ran");
    assert!(
        payloads(&store, EventType::WORKSPACE_SNAPSHOT_CREATED).await.is_empty(),
        "a refused capture records no snapshot"
    );
}

#[tokio::test]
async fn what_the_call_changed_is_recorded_after_it_ran() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let snapshotter = RecordingSnapshotter::new(order.clone());
    let (gateway, store) =
        snapshot_harness(write_tools(order.clone()), snapshotter.clone(), Capability::FS_WRITE);

    gateway.execute(&sid(), &write_invocation()).await.unwrap();

    assert_eq!(snapshotter.diffs(), 1);
    let recorded = payloads(&store, EventType::WORKSPACE_CHANGES_RECORDED).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0]["changes_hash"].as_str().map(str::len),
        Some(16),
        "the change list carries its own digest"
    );
    assert_eq!(recorded[0]["task_id"], "manual:guarded-session");
}

#[tokio::test]
async fn a_command_snapshots_the_writable_roots_not_the_named_paths() {
    // No provider involved: the scope rule is the thing under test.
    let tools = Arc::new(ScriptedTools::echo(vec![def(
        "shell.run",
        &[Capability::PROCESS_SPAWN],
        ToolSideEffect::Destructive,
    )]));
    let guard = ResourceGuard::new(Workspace {
        workspace_root: std::env::current_dir().unwrap().display().to_string(),
        writable_roots: vec!["sub".to_string()],
        external_directories: vec![],
    });
    let gateway = GuardedToolProvider::new(
        tools,
        allowing(Capability::PROCESS_SPAWN),
        Arc::new(ApprovalService::new(Arc::new(FakeEventStore::new()))),
        guard.clone(),
        Arc::new(agent_testkit::InMemoryArtifactStore::default()),
    );
    let invocation =
        ToolInvocation::proposed("shell.run", serde_json::json!({ "command": "touch x" }));
    let action = gateway.action_facts("manual:s", 0, &invocation);

    let scope = crate::snapshot::scope_for(&action, &guard).expect("a command gets a snapshot");
    let WorkspaceSnapshotScope::WritableRoots { roots } = scope else {
        panic!("a command must snapshot the roots, got {scope:?}");
    };
    assert_eq!(roots.len(), 1);
    assert!(roots[0].ends_with("sub"), "the guard's own roots, not a second list: {roots:?}");
}
