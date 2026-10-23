//! KernelDoctor — orchestrates health checks injected through the DoctorCheck port.

use agent_ports::doctor::DoctorCheck;
use agent_protocol::diagnostics::{HealthCheckId, HealthReport};
use std::collections::HashSet;

/// Orchestrator that runs injected health checks and aggregates results.
pub struct KernelDoctor {
    checks: Vec<Box<dyn DoctorCheck>>,
}

impl std::fmt::Debug for KernelDoctor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelDoctor").field("checks_count", &self.checks.len()).finish()
    }
}

impl KernelDoctor {
    /// Create a new doctor with the given checks.
    ///
    /// Returns an error if duplicate check IDs are detected.
    pub fn new(checks: Vec<Box<dyn DoctorCheck>>) -> Result<Self, DoctorError> {
        let mut seen = HashSet::new();
        for check in &checks {
            let id = check.id();
            if !seen.insert(id.clone()) {
                return Err(DoctorError::DuplicateCheckId(id));
            }
        }
        Ok(Self { checks })
    }

    /// Run all checks and return an aggregated health report.
    pub async fn report(&self, schema_version: agent_protocol::ids::SchemaVersion) -> HealthReport {
        let mut results = Vec::new();
        for check in &self.checks {
            let mut messages = check.run().await;
            results.append(&mut messages);
        }
        let aggregate_status = HealthReport::aggregate(&results);
        HealthReport { schema_version, checks: results, aggregate_status }
    }
}

/// Errors from kernel doctor construction or execution.
#[derive(Debug, thiserror::Error)]
pub enum DoctorError {
    #[error("duplicate check ID: {0}")]
    DuplicateCheckId(HealthCheckId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::doctor::DoctorCheck;
    use agent_protocol::diagnostics::{DiagnosticMessage, HealthCheckId, HealthStatus};
    use agent_protocol::ids::SchemaVersion;

    struct PassingCheck {
        id: HealthCheckId,
    }

    #[async_trait::async_trait]
    impl DoctorCheck for PassingCheck {
        fn id(&self) -> HealthCheckId {
            self.id.clone()
        }
        async fn run(&self) -> Vec<DiagnosticMessage> {
            vec![DiagnosticMessage {
                check_id: self.id.clone(),
                status: HealthStatus::Passed,
                message: "ok".to_string(),
            }]
        }
    }

    struct FailingCheck {
        id: HealthCheckId,
    }

    #[async_trait::async_trait]
    impl DoctorCheck for FailingCheck {
        fn id(&self) -> HealthCheckId {
            self.id.clone()
        }
        async fn run(&self) -> Vec<DiagnosticMessage> {
            vec![DiagnosticMessage {
                check_id: self.id.clone(),
                status: HealthStatus::Failed,
                message: "fail".to_string(),
            }]
        }
    }

    fn sv() -> SchemaVersion {
        SchemaVersion::from_string("0.1.0")
    }

    #[tokio::test]
    async fn all_passing_checks_produce_success() {
        let doctor = KernelDoctor::new(vec![
            Box::new(PassingCheck { id: HealthCheckId::new("a") }),
            Box::new(PassingCheck { id: HealthCheckId::new("b") }),
        ])
        .unwrap();

        let report = doctor.report(sv()).await;
        assert_eq!(report.aggregate_status, HealthStatus::Passed);
        assert_eq!(report.checks.len(), 2);
    }

    #[tokio::test]
    async fn at_least_one_failed_check_produces_failure() {
        let doctor = KernelDoctor::new(vec![
            Box::new(PassingCheck { id: HealthCheckId::new("a") }),
            Box::new(FailingCheck { id: HealthCheckId::new("b") }),
        ])
        .unwrap();

        let report = doctor.report(sv()).await;
        assert_eq!(report.aggregate_status, HealthStatus::Failed);
    }

    #[tokio::test]
    async fn duplicate_check_ids_rejected() {
        let result = KernelDoctor::new(vec![
            Box::new(PassingCheck { id: HealthCheckId::new("same") }),
            Box::new(PassingCheck { id: HealthCheckId::new("same") }),
        ]);

        assert!(result.is_err());
    }
}
