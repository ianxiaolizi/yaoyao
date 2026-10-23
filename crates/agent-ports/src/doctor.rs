//! DoctorCheck trait — health check port.

use agent_protocol::diagnostics::DiagnosticMessage;

/// A single health check that can be run by the kernel doctor.
#[async_trait::async_trait]
pub trait DoctorCheck: Send + Sync {
    /// Return a stable identifier for this check.
    fn id(&self) -> agent_protocol::diagnostics::HealthCheckId;

    /// Run the check and return diagnostic messages.
    async fn run(&self) -> Vec<DiagnosticMessage>;
}
