//! EventStore trait — the single source of truth for durable events.

use agent_protocol::events::{EventDraft, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};

/// Whether a session id is safe to use as a single directory-name segment.
///
/// Session ids become directory names wherever a store or a projection keeps one
/// directory per session, and the port is the only crate both of those already
/// depend on — so this is the one answer to the question, and a store, a
/// projection and a test double cannot drift into different acceptance rules.
///
/// `SessionId::new()` produces UUIDs, so this only rejects adversarial
/// `from_string` input that would otherwise escape the root.
///
/// `.` and `..` are the two that do. Both pass the character test below — `.`
/// is in the allowed set because a session called `2026-10-11.run` is an
/// ordinary thing to want — and neither is a child of the root: they *are* the
/// root and its parent. So this rejects the two spellings, not the character,
/// and every other name is a directory under the root however odd it looks.
///
/// Windows is the exception to "however odd it looks", because it renames a
/// segment in two ways the character test cannot see. A trailing `.` is
/// dropped, so `foo.` and `foo` are one directory and two session ids would
/// share a log; and the reserved device names (`CON`, `PRN`, `AUX`, `NUL`,
/// `COM1`–`COM9`, `LPT1`–`LPT9`, with or without an extension) route to a
/// device, so `NUL` "creates" nothing and the log under it then fails to open.
/// Both are refused there. The device list is matched on the name before the
/// first `.` — `NUL.txt` is still the device, while `foo.NUL` is an ordinary
/// file (YA-71).
pub fn is_safe_dir_name(id: &str) -> bool {
    if matches!(id, "." | "..") {
        return false;
    }
    let well_formed = !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if !well_formed {
        return false;
    }
    #[cfg(windows)]
    {
        let base = id.split('.').next().unwrap_or(id).to_ascii_uppercase();
        let reserved_device = matches!(
            base.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
        if id.ends_with('.') || reserved_device {
            return false;
        }
    }
    true
}

/// Receipt returned after a successful append.
#[derive(Debug, Clone)]
pub struct AppendReceipt {
    pub session_id: SessionId,
    /// Stored events actually appended (idempotent duplicates are excluded).
    pub appended: Vec<StoredEvent>,
    /// Number of drafts skipped as idempotent duplicates.
    pub duplicates: usize,
}

/// Receipt returned after a successful flush.
#[derive(Debug, Clone)]
pub struct FlushReceipt {
    pub session_id: SessionId,
    /// Highest sequence number that is now durable.
    pub durable_seq: EventSeq,
}

/// A subscription to stored events from `after_seq` onward.
///
/// `next` waits for the event after the last one it yielded. A subscription that
/// has caught up returns `Pending`, not `None`: the log may grow again, and that
/// is the whole point of watching it. `None` means no further event will *ever*
/// arrive from this subscription — the store that made it is gone — so a loop
/// over `next` reads `None` as an end and `Pending` as a pause.
///
/// Which writers a subscription can see is the implementation's to state, since
/// it is not derivable from the signature. In-process notification alone cannot
/// see another process append; an implementation that must cover both has to
/// re-read the log as well.
///
/// An error is not an end. A log that cannot be read right now — a torn line, a
/// store still holding the file, a disk that said no — is reported as `Err` and
/// the subscription stays usable, because "I could not read this" and "there is
/// nothing more" are different facts and only one of them is a reason to stop
/// watching.
#[async_trait::async_trait]
pub trait EventSubscription: Send {
    async fn next(&mut self) -> Result<Option<StoredEvent>, EventStoreError>;
}

/// An exclusive claim on one session's log, held across a caller's whole
/// read → decide → act → append sequence.
///
/// `append` already takes the session lock for the length of a write, which is
/// enough to keep the log from being corrupted. A lease is for callers that must
/// not merely *write* once but *act* once: resuming an approved tool call runs a
/// side effect **before** anything is appended, so exclusion has to span the
/// action and not just the write. With only a write lock, two processes both
/// read the same unanswered call, both run it, and only then does one of them
/// lose the append.
///
/// Dropping the lease releases it. Within one store instance leases are
/// re-entrant: a caller holding one may call `append` without deadlocking, so a
/// critical section can be written as flat code rather than as a callback.
#[async_trait::async_trait]
pub trait SessionLease: Send + Sync {
    fn session_id(&self) -> &SessionId;
}

/// Port for appending and reading durable events.
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    /// Append drafts as the next events in the session.
    ///
    /// `expected_next_seq` must equal the session's current next sequence,
    /// otherwise `SequenceConflict` is returned. Duplicate `event_id`s are
    /// idempotent: they are skipped and reported in the receipt, never applied twice.
    ///
    /// The sequence is compared against the log itself, not against a
    /// process-local snapshot, so `SequenceConflict` means *someone else wrote*
    /// — including another process. A caller that loses the race has to re-read
    /// and decide again; the store will not do that on its behalf, because a
    /// batch is usually built from what the caller read, and re-deriving only
    /// the sequence would apply a decision made against state that has moved.
    async fn append(
        &self,
        session_id: &SessionId,
        expected_next_seq: EventSeq,
        drafts: Vec<EventDraft>,
    ) -> Result<AppendReceipt, EventStoreError>;

    /// Acquire the session's write lease.
    ///
    /// Waits for a holder in another process to let go, because the lease exists
    /// to be held across a side effect — a resumed tool call runs before
    /// anything is appended — and the wait is the caller letting that run finish
    /// rather than racing it. An implementation may wait without bound, which is
    /// safe because a lock is released when its holder exits, or report
    /// `ConcurrentWriter` after a wait it documents.
    async fn lease(&self, session_id: &SessionId)
    -> Result<Box<dyn SessionLease>, EventStoreError>;

    /// Make all previously acknowledged appends durable. Until flush returns,
    /// an acknowledged append may be lost on crash.
    async fn flush(&self, session_id: &SessionId) -> Result<FlushReceipt, EventStoreError>;

    /// Read events starting at `from_seq`, up to `limit`.
    ///
    /// A read observes appends made by any process before the call returns, so a
    /// long-lived reader sees another process's writes without reopening. What a
    /// read must *not* do is invent order: only complete records are returned,
    /// and a line still being written is left for a later read rather than
    /// reported torn or repaired by the reader.
    async fn read_stream(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, EventStoreError>;

    /// Remove the session's log for good.
    ///
    /// This is the one operation in the port that destroys facts, and it exists
    /// because "hide it" already has an answer: a person who deletes a session
    /// expects the transcript to be gone from the disk, not hidden behind a
    /// tombstone that a backup, an editor or `read_stream` would still find.
    ///
    /// What it must do, in order: take the session's lease, so no write
    /// interleaves; drop whatever this instance cached for the session, so its
    /// file handles are closed before the directory goes and a later session
    /// that reuses the id cannot read the old log out of a cache; then remove
    /// the log. A session that is not there is not an error — the caller asked
    /// for it to be gone, and it is.
    ///
    /// A turn running in *another* process is not visible to this call beyond
    /// the lease it takes for each write. Callers that can see their own turns
    /// must refuse to delete under one; the port cannot know about them.
    ///
    /// The default refusal exists so a double that only gates or refuses writes
    /// does not have to spell out a destruction it would never perform. A store
    /// that can hold events is expected to answer this one.
    async fn delete_session(&self, session_id: &SessionId) -> Result<(), EventStoreError> {
        Err(EventStoreError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "this store cannot delete session {}: it is a test double",
                session_id.as_str()
            ),
        )))
    }

    /// Subscribe to events from `from_seq` onward, `from_seq` included.
    ///
    /// The bound is inclusive because it is the same kind of number
    /// `read_stream` takes and means the same thing there: a caller holding a
    /// `from` can hand it to either without arithmetic, and there is no way to
    /// express "the whole log" to an exclusive bound — `EventSeq::ZERO` has no
    /// predecessor, so an exclusive reading of it silently drops the first
    /// event of every session.
    async fn subscribe(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
    ) -> Result<Box<dyn EventSubscription>, EventStoreError>;
}

/// Errors from an EventStore implementation.
#[derive(Debug, thiserror::Error)]
pub enum EventStoreError {
    #[error("sequence conflict: expected {expected}, actual {actual}")]
    SequenceConflict { expected: EventSeq, actual: EventSeq },
    /// Another holder kept the session leased for longer than the caller was
    /// willing to wait. Distinct from `SequenceConflict`: nothing was read or
    /// written, and the caller may simply be unlucky.
    #[error("session {0} is leased by another writer")]
    ConcurrentWriter(SessionId),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// The log contains a record whose schema version this build cannot read
    /// (YA-21). Refused rather than parsed best-effort: a newer record may use
    /// a field's old spelling for a new meaning, and a reader that guesses will
    /// produce a confident answer built on the wrong one.
    #[error("event schema version: {0}")]
    Version(#[from] agent_protocol::versioning::VersionError),
}

impl EventStoreError {
    /// The refusal for a session id [`is_safe_dir_name`] turns down.
    ///
    /// Shared so that every implementation refuses an unusable id in the same
    /// words: a caller cannot tell a store from a test double by the error, and
    /// a test asserting the refusal is asserting the port's behavior.
    pub fn unsafe_session_id() -> Self {
        Self::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id is not a safe directory name",
        ))
    }
}
