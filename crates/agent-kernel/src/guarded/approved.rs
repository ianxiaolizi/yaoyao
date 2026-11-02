//! Reconstructing the exact action one approval covered.
//!
//! A classifier may have raised the risk after the provider's defaults were
//! read, and the person approved the raised action. Resume therefore starts
//! from the action stored in the decision ledger, not from a fresh provider
//! reading; the fresh reading is used only to prove the arguments still hash to
//! the approved fingerprint.

use agent_protocol::ids::{ApprovalId, SessionId};
use agent_protocol::permission::AuthorizationSubject;
use agent_protocol::tool::ToolInvocation;

use super::{GuardedToolProvider, ToolGatewayError};

impl GuardedToolProvider {
    pub(super) async fn approved_action(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        invocation: &ToolInvocation,
    ) -> Result<agent_protocol::permission::ActionFacts, ToolGatewayError> {
        let stored = match self.permissions.as_deref() {
            Some(permissions) => permissions
                .action_for_approval(session_id, approval_id)
                .await
                .map_err(|error| ToolGatewayError::PermissionAudit {
                    tool: invocation.tool_name.clone(),
                    reason: error.to_string(),
                })?,
            None => None,
        };
        if let Some(approved) = stored {
            let rebuilt = self
                .action_facts(&approved.task_id, approved.task_revision, invocation)
                .raised_to(approved.risk);
            return Ok(if rebuilt.digest == approved.digest { approved } else { rebuilt });
        }
        let subject = invocation
            .authorization_subject
            .clone()
            .unwrap_or_else(|| AuthorizationSubject::manual(session_id.as_str()));
        Ok(self.action_facts(&subject.task_id, subject.task_revision, invocation))
    }
}
