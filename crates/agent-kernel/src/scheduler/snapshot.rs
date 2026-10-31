//! Reading the configuration a session's runs execute under.
//!
//! The profile snapshot is what every run records as `profile/resolved`; a
//! caller that has to answer "what budget did assembly derive, and where did
//! each number come from?" should not have to open a turn to find out.

use agent_protocol::profile::ResolvedProfileSnapshot;

use super::Scheduler;

impl Scheduler {
    /// The configuration this session's runs execute under, when the app layer
    /// resolved one. `None` is the harness state the field itself documents.
    pub fn profile_snapshot(&self) -> Option<&ResolvedProfileSnapshot> {
        self.profile.as_ref()
    }
}

/// Test-only helpers. Out of line because `#[cfg(test)]` must be the last
/// thing a production file says (the boundary guards in agent-testkit assert
/// exactly that), and this module's production half is only the accessor above.
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) use tests::snapshot_with_budget;
