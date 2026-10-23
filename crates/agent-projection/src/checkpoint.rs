//! Projection checkpoints: a disposable cache of folded state.

use agent_protocol::ids::{EventSeq, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A cached projection state, valid up to `as_of_seq`.
///
/// Checkpoints are a cache, not a source of truth: they can always be dropped
/// and rebuilt by folding the event log from seq 0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectionCheckpoint<State> {
    pub session_id: SessionId,
    pub projection_key: String,
    /// `None` when the projection has not folded any event yet.
    pub as_of_seq: Option<EventSeq>,
    pub projection_schema_version: String,
    pub state_version: u64,
    pub created_at: DateTime<Utc>,
    pub state: State,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionState;

    #[test]
    fn checkpoint_round_trips_through_serde() {
        let cp = ProjectionCheckpoint {
            session_id: SessionId::from_string("s"),
            projection_key: "session".to_string(),
            as_of_seq: Some(EventSeq::new(3)),
            projection_schema_version: "session/0.1.0".to_string(),
            state_version: 1,
            created_at: Utc::now(),
            state: SessionState {
                started: true,
                turns_started: 2,
                turns_terminal: 1,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&cp).unwrap();
        let decoded: ProjectionCheckpoint<SessionState> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.session_id, cp.session_id);
        assert_eq!(decoded.as_of_seq, Some(EventSeq::new(3)));
        assert_eq!(decoded.state, cp.state);
    }
}
