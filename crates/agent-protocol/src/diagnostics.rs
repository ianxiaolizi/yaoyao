//! Health check and diagnostic types.

use crate::ids::SchemaVersion;
use serde::{Deserialize, Serialize};

/// Unique identifier for a health check.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HealthCheckId(pub String);

impl HealthCheckId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl std::fmt::Display for HealthCheckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Status of a single health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Passed,
    Failed,
    Skipped,
}

/// A diagnostic message from a health check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticMessage {
    pub check_id: HealthCheckId,
    pub status: HealthStatus,
    pub message: String,
}

/// Aggregated health report from all checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub schema_version: SchemaVersion,
    pub checks: Vec<DiagnosticMessage>,
    pub aggregate_status: HealthStatus,
}

impl HealthReport {
    /// Determine aggregate status: any Failed → Failed, else Passed.
    pub fn aggregate(checks: &[DiagnosticMessage]) -> HealthStatus {
        if checks.iter().any(|c| c.status == HealthStatus::Failed) {
            HealthStatus::Failed
        } else {
            HealthStatus::Passed
        }
    }
}
