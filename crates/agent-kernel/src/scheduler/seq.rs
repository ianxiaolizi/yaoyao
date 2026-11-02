//! The next sequence in a session's log.

use agent_protocol::ids::{EventSeq, SessionId};

use super::{Scheduler, SchedulerError};

impl Scheduler {
    pub(crate) async fn next_seq(&self, session: &SessionId) -> Result<EventSeq, SchedulerError> {
        let events = self.store.read_stream(session, EventSeq::ZERO, usize::MAX).await?;
        Ok(events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO))
    }
}
