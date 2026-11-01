//! Recovery points around a call that may change the workspace (B8).
//!
//! The gateway is the one layer holding the action *and* the tool's declared
//! side effect *and* the execution boundary, which is exactly what deciding "is
//! this call worth a snapshot, and of what" needs. This module makes that
//! decision, asks the snapshotter, writes the fact, and — after the call —
//! compares the workspace against what was captured.
//!
//! The rule the whole slice rests on: **a call that cannot be recovered from
//! does not run**. A capture that could not be taken, could not be stored, or
//! could not be complete is a refusal here, before the provider is invoked.
//! There is no fallback to "run it anyway without a recovery point", because
//! that fallback is the failure mode the recovery point exists to prevent.
//!
//! What this module deliberately does not do: copy bytes (the adapter does),
//! decide policy (the permission chain already did), or treat a read-only call
//! as needing a snapshot. A capture whose scope is empty because nothing can
//! change is not a refusal — it is not a capture at all.

use agent_ports::tool_gateway::ToolGatewayError;
use agent_ports::tool_provider::{ToolInvocation, ToolSideEffect};
use agent_ports::workspace_snapshot::{SnapshotRequest, WorkspaceSnapshotter};
use agent_protocol::events::EventType;
use agent_protocol::ids::SessionId;
use agent_protocol::permission::{ActionFacts, ActionTarget, RiskLevel};
use agent_protocol::workspace_snapshot::{SnapshotManifest, WorkspaceSnapshotScope};
use chrono::Utc;

use crate::permission::{PermissionService, WorkspaceRecord};
use crate::resource_guard::ResourceGuard;

/// What one call's recovery point needs: everything captured before the
/// provider runs and compared after it.
///
/// A bundle rather than four arguments twice: the two halves must be the same
/// snapshotter, ledger and session, and two argument lists are two places to
/// pass one of them wrong.
pub(crate) struct RecoveryContext<'a> {
    pub snapshotter: Option<&'a dyn WorkspaceSnapshotter>,
    pub permissions: Option<&'a PermissionService>,
    pub guard: &'a ResourceGuard,
    pub session_id: &'a SessionId,
}

/// What a capture left behind for the call to settle afterwards.
///
/// Held by value between the two halves: the manifest is the only thing the
/// diff needs, and a caller that dropped it could not describe what changed.
pub(crate) struct RecoveryPoint {
    manifest: SnapshotManifest,
}

/// The scope one action needs, or `None` when nothing can change.
///
/// Three answers, and the order between them is the whole rule:
///
/// * A call that declares a path is recovering exactly those paths. This is the
///   common case (`fs.write`), and it keeps the snapshot small: one file, not
///   the workspace.
/// * A call that runs a command names no path it will touch, so the bound is
///   the writable roots the guard enforces — the same list, not a second
///   derivation of it (CONTRACT §7 #182).
/// * Anything else — a network call, a memory write, a read — touches no file
///   the workspace guard governs, and gets no snapshot. Taking one would be
///   work that answers nothing.
pub(crate) fn scope_for(
    action: &ActionFacts,
    guard: &ResourceGuard,
) -> Option<WorkspaceSnapshotScope> {
    let paths: Vec<String> = action
        .targets
        .iter()
        .filter_map(|target| match target {
            ActionTarget::WorkspacePath { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    if !paths.is_empty() {
        return Some(WorkspaceSnapshotScope::Paths { paths });
    }
    let runs_a_command =
        action.targets.iter().any(|target| matches!(target, ActionTarget::Command { .. }));
    if !runs_a_command {
        return None;
    }
    let roots: Vec<String> = if guard.writable_roots().is_empty() {
        vec![guard.workspace_root().display().to_string()]
    } else {
        guard.writable_roots().iter().map(|root| root.display().to_string()).collect()
    };
    Some(WorkspaceSnapshotScope::WritableRoots { roots })
}

impl<'a> RecoveryContext<'a> {
    /// Take the recovery point, and write down that it was taken.
    ///
    /// Runs before the provider is invoked. `dispatch` is the tool's own
    /// declared side effect, read from the provider rather than from the tool's
    /// name: a read-only tool gets no snapshot even when it carries a path
    /// argument, and a provider that answers `None` is treated as one that may
    /// write — the same conservative reading the path boundary applies.
    pub(crate) async fn capture(
        &self,
        action: &ActionFacts,
        invocation: &ToolInvocation,
        dispatch: Option<ToolSideEffect>,
    ) -> Result<Option<RecoveryPoint>, ToolGatewayError> {
        let Some(snapshotter) = self.snapshotter else {
            return Ok(None);
        };
        if !may_change_files(action, dispatch) {
            return Ok(None);
        }
        let Some(scope) = scope_for(action, self.guard) else {
            return Ok(None);
        };
        let tool = invocation.tool_name.clone();
        let manifest = snapshotter
            .capture(SnapshotRequest {
                session_id: self.session_id.to_string(),
                task_id: action.task_id.clone(),
                action_digest: action.digest.clone(),
                scope,
            })
            .await
            .map_err(|error| ToolGatewayError::SnapshotRefused {
                tool: tool.clone(),
                reason: error.to_string(),
            })?;
        if let Some(permissions) = self.permissions {
            let record = WorkspaceRecord {
                event_type: EventType::WORKSPACE_SNAPSHOT_CREATED,
                payload: serde_json::to_value(&manifest).map_err(|error| {
                    ToolGatewayError::PermissionAudit { tool, reason: error.to_string() }
                })?,
                recorded_at: Utc::now(),
            };
            permissions.record_workspace(self.session_id, record).await.map_err(|error| {
                ToolGatewayError::PermissionAudit {
                    tool: action.tool_name.clone(),
                    reason: error.to_string(),
                }
            })?;
        }
        Ok(Some(RecoveryPoint { manifest }))
    }

    /// Compare the workspace against the recovery point, and write down what
    /// changed.
    ///
    /// Runs whatever the tool answered: a failed call may still have written
    /// half a file, and "the call failed" is not a claim about the workspace.
    pub(crate) async fn settle(
        &self,
        point: Option<RecoveryPoint>,
        tool: &str,
    ) -> Result<(), ToolGatewayError> {
        let (Some(point), Some(snapshotter)) = (point, self.snapshotter) else {
            return Ok(());
        };
        let changes = snapshotter.diff(&point.manifest).await.map_err(|error| {
            ToolGatewayError::SnapshotRefused {
                tool: tool.to_string(),
                reason: format!("the changes this call made could not be compared: {error}"),
            }
        })?;
        if let Some(permissions) = self.permissions {
            let record = WorkspaceRecord {
                event_type: EventType::WORKSPACE_CHANGES_RECORDED,
                payload: serde_json::to_value(&changes).map_err(|error| {
                    ToolGatewayError::PermissionAudit {
                        tool: tool.to_string(),
                        reason: error.to_string(),
                    }
                })?,
                recorded_at: Utc::now(),
            };
            permissions.record_workspace(self.session_id, record).await.map_err(|error| {
                ToolGatewayError::PermissionAudit {
                    tool: tool.to_string(),
                    reason: error.to_string(),
                }
            })?;
        }
        Ok(())
    }
}

/// Whether this call can change a file at all.
///
/// `Pure` and `Read` are the only answers that say it cannot; an answer the
/// provider did not give is read as "it might", which is the same rule the
/// path boundary uses for writes.
fn may_change_files(action: &ActionFacts, dispatch: Option<ToolSideEffect>) -> bool {
    if action.risk == RiskLevel::L0 {
        return false;
    }
    !matches!(dispatch, Some(ToolSideEffect::Pure) | Some(ToolSideEffect::Read))
}
