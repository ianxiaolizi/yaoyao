//! The projection engine: fold + checkpoint + recovery.

use std::path::PathBuf;
use std::sync::Arc;

use agent_ports::event_store::{EventStore, is_safe_dir_name};
use agent_protocol::events::StoredEvent;
use agent_protocol::ids::{EventSeq, SessionId};
use chrono::Utc;

use crate::checkpoint::ProjectionCheckpoint;
use crate::definition::{ApplyError, ProjectionDefinition};

/// What comparing a stored checkpoint against the log found.
///
/// A checkpoint is a cache, so three of these are ordinary cache misses rather
/// than corruption: absent, unusable (not this projection's, or unreadable), and
/// ungrounded (its `as_of_seq` is not in the log). Only [`Diverges`] is a
/// defect: the file claims a state the log does not support, and
/// [`Projection::build`] would silently serve it.
///
/// [`Diverges`]: CheckpointVerdict::Diverges
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointVerdict {
    /// No checkpoint file exists for this projection and session.
    Absent,
    /// A file exists but is not a checkpoint this projection wrote — another
    /// key, another schema version, or not JSON this build can read. `build`
    /// ignores it.
    Unusable,
    /// The checkpoint names a sequence the log does not hold. `build` treats it
    /// as a miss and rebuilds from the log.
    Ungrounded { claimed: EventSeq },
    /// The checkpoint is grounded and its state equals folding the log up to
    /// `as_of_seq`.
    Matches { as_of_seq: Option<EventSeq> },
    /// The checkpoint is grounded and its state is **wrong**: folding the log
    /// itself up to `as_of_seq` produces a different state.
    Diverges { as_of_seq: Option<EventSeq> },
}

/// A projection state and the sequence it is valid up to.
#[derive(Debug, Clone)]
pub struct Projected<State> {
    pub state: State,
    /// `None` when the projection has not folded any event yet.
    pub as_of_seq: Option<EventSeq>,
}

/// Rebuilds a projection from an `EventStore`, caching progress in checkpoints.
pub struct Projection<D: ProjectionDefinition> {
    definition: D,
    store: Arc<dyn EventStore>,
    checkpoint_root: PathBuf,
}

impl<D: ProjectionDefinition> Projection<D> {
    pub fn new(
        definition: D,
        store: Arc<dyn EventStore>,
        checkpoint_root: impl Into<PathBuf>,
    ) -> Self {
        Self { definition, store, checkpoint_root: checkpoint_root.into() }
    }

    /// Derive the client-facing value from folded state.
    pub fn view(&self, state: &D::State) -> D::View {
        self.definition.view(state)
    }

    /// Rebuild using a checkpoint when one is valid, else from seq 0.
    ///
    /// "Valid" is two things, and the second one needs the log: the metadata
    /// must name this projection (`load_checkpoint`), **and** the log must still
    /// hold the event the checkpoint says it folded up to. A checkpoint is a
    /// shortcut through the derivation, never a second source of truth for it,
    /// so a shortcut the log cannot back is not taken.
    pub async fn build(
        &self,
        session_id: &SessionId,
    ) -> Result<Projected<D::State>, ProjectionError> {
        let Some(cp) = self.load_checkpoint(session_id) else {
            return self.build_full(session_id).await;
        };
        if !self.checkpoint_is_grounded(session_id, &cp).await? {
            return self.build_full(session_id).await;
        }
        self.build_from(session_id, Some(cp)).await
    }

    /// Rebuild from seq 0, ignoring any checkpoint.
    pub async fn build_full(
        &self,
        session_id: &SessionId,
    ) -> Result<Projected<D::State>, ProjectionError> {
        self.build_from(session_id, None).await
    }

    /// Compare the stored checkpoint against the log it claims to have folded.
    ///
    /// This is the consumer `CONTRACT.md` C1.6 requires: a checkpoint whose
    /// metadata is valid and whose `as_of_seq` the log holds can still carry a
    /// wrong state, and until something folds the log itself and compares, that
    /// wrong state is served as the derivation. The comparison folds the log
    /// **up to the checkpoint's own seq** — folding further would report every
    /// checkpoint that is merely behind the tail as divergent.
    ///
    /// The log is the truth. On [`CheckpointVerdict::Diverges`] the remedy is to
    /// drop the checkpoint; this method reports, it does not repair, because a
    /// diagnostic that rewrites the thing it is inspecting cannot be run twice.
    pub async fn check_checkpoint(
        &self,
        session_id: &SessionId,
    ) -> Result<CheckpointVerdict, ProjectionError> {
        let checkpoint = match self.read_checkpoint_file(session_id)? {
            CheckpointFile::Absent => return Ok(CheckpointVerdict::Absent),
            CheckpointFile::Unusable => return Ok(CheckpointVerdict::Unusable),
            CheckpointFile::Ours(checkpoint) => checkpoint,
        };
        let as_of_seq = checkpoint.as_of_seq;
        if let Some(claimed) = as_of_seq {
            let grounded = self.checkpoint_is_grounded(session_id, &checkpoint).await?;
            if !grounded {
                return Ok(CheckpointVerdict::Ungrounded { claimed });
            }
        }
        let folded = self.fold_through(session_id, as_of_seq).await?;
        if folded == checkpoint.state {
            Ok(CheckpointVerdict::Matches { as_of_seq })
        } else {
            Ok(CheckpointVerdict::Diverges { as_of_seq })
        }
    }

    /// Persist a checkpoint for the given projected state.
    ///
    /// Written to a staging name and renamed into place, so a concurrent reader
    /// sees either the previous checkpoint or this one and never a torn file.
    /// A torn file would be `Unusable`, which costs a full fold and nothing
    /// else, but a write that a reader can observe half-done turns a background
    /// refresh into a visible stall for whoever reads next. The staging name
    /// carries the writer's pid for the same reason: two processes refreshing
    /// one session must not interleave their bytes into one file. A process that
    /// dies mid-write leaves its staging file behind; nothing reads it, and the
    /// next successful refresh replaces the checkpoint proper.
    pub async fn checkpoint(
        &self,
        session_id: &SessionId,
        projected: &Projected<D::State>,
    ) -> Result<(), ProjectionError> {
        let dir = self.checkpoints_dir(session_id)?;
        std::fs::create_dir_all(&dir)?;
        let cp = ProjectionCheckpoint {
            session_id: session_id.clone(),
            projection_key: self.definition.key().to_string(),
            as_of_seq: projected.as_of_seq,
            projection_schema_version: self.definition.state_schema().to_string(),
            state_version: self.definition.state_version(),
            created_at: Utc::now(),
            state: projected.state.clone(),
        };
        let bytes = serde_json::to_vec(&cp)?;
        let path = self.checkpoint_path(session_id)?;
        let staging = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&staging, bytes)?;
        std::fs::rename(&staging, &path)?;
        Ok(())
    }

    async fn build_from(
        &self,
        session_id: &SessionId,
        checkpoint: Option<ProjectionCheckpoint<D::State>>,
    ) -> Result<Projected<D::State>, ProjectionError> {
        let (mut state, mut last_seq, from_seq) = match &checkpoint {
            Some(cp) => {
                let from = cp.as_of_seq.map_or(EventSeq::ZERO, EventSeq::next);
                (cp.state.clone(), cp.as_of_seq, from)
            }
            None => (self.definition.init(), None, EventSeq::ZERO),
        };

        let events = self.store.read_stream(session_id, from_seq, usize::MAX).await?;
        for event in &events {
            state = self.apply_event(state, event)?;
            last_seq = Some(event.seq);
        }

        Ok(Projected { state, as_of_seq: last_seq })
    }

    /// Fold the log from zero through `through` (inclusive), or through the end
    /// when `None`, ignoring checkpoints.
    ///
    /// The inclusive bound matters: a checkpoint's `as_of_seq` is the seq of the
    /// last event it folded, so a comparison that stopped at the previous event
    /// would disagree with every checkpoint by exactly one event.
    async fn fold_through(
        &self,
        session_id: &SessionId,
        through: Option<EventSeq>,
    ) -> Result<D::State, ProjectionError> {
        let mut state = self.definition.init();
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        for event in &events {
            if through.is_some_and(|through| event.seq > through) {
                break;
            }
            state = self.apply_event(state, event)?;
        }
        Ok(state)
    }

    /// Apply one event, or explain why the fold cannot continue.
    fn apply_event(
        &self,
        state: D::State,
        event: &StoredEvent,
    ) -> Result<D::State, ProjectionError> {
        match self.definition.apply(&state, event) {
            Ok(next) => Ok(next),
            // Unknown but ignorable: the event stays in the log, the fold skips
            // it.
            Err(ApplyError::UnknownEventType(_)) if event.event.ignorable => Ok(state),
            Err(ApplyError::UnknownEventType(_)) => Err(ProjectionError::UnknownRequiredEvent {
                event_type: event.event.event_type.clone(),
                seq: event.seq,
            }),
            // A known event with an unreadable payload is not the `ignorable`
            // case: that flag says a type may be skipped when the fold does not
            // know it, and here the fold knows it and cannot read it. Skipping
            // would shorten the derived view without saying so.
            Err(ApplyError::MalformedPayload { event_type, seq, reason }) => {
                Err(ProjectionError::MalformedEvent { event_type, seq, reason })
            }
        }
    }

    fn checkpoints_dir(&self, session_id: &SessionId) -> Result<PathBuf, ProjectionError> {
        if !is_safe_dir_name(session_id.as_str()) {
            return Err(ProjectionError::InvalidSessionId(session_id.clone()));
        }
        Ok(self.checkpoint_root.join(session_id.as_str()).join("checkpoints"))
    }

    fn checkpoint_path(&self, session_id: &SessionId) -> Result<PathBuf, ProjectionError> {
        Ok(self.checkpoints_dir(session_id)?.join(format!("{}.json", self.definition.key())))
    }

    /// Load a checkpoint, or `None` when it is absent, unreadable, corrupt, or
    /// stale (schema/version/session/key mismatch). All of those mean "rebuild
    /// from the source of truth" — a checkpoint is never trusted over the log.
    ///
    /// Metadata only: everything checked here says the checkpoint was written by
    /// *this* projection. Whether the log still supports it is
    /// [`Self::checkpoint_is_grounded`]'s question, and whether the state itself
    /// is right is [`Self::check_checkpoint`]'s — a comparison deliberately kept
    /// out of `build`, because a cache lookup that folds the log anyway would
    /// buy nothing.
    fn load_checkpoint(&self, session_id: &SessionId) -> Option<ProjectionCheckpoint<D::State>> {
        match self.read_checkpoint_file(session_id).ok()? {
            CheckpointFile::Ours(checkpoint) => Some(checkpoint),
            CheckpointFile::Absent | CheckpointFile::Unusable => None,
        }
    }

    /// Distinguish "no checkpoint" and "not a checkpoint of ours" from a
    /// checkpoint this projection can actually reason about.
    ///
    /// An unreadable file is `Unusable` rather than an error: `build` would
    /// rebuild from the log regardless, and a diagnostic that fails on the file
    /// it is inspecting cannot report on it. An unsafe session id is an error,
    /// because no file path can be constructed for it at all.
    fn read_checkpoint_file(
        &self,
        session_id: &SessionId,
    ) -> Result<CheckpointFile<D::State>, ProjectionError> {
        let path = self.checkpoint_path(session_id)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CheckpointFile::Absent);
            }
            Err(_) => return Ok(CheckpointFile::Unusable),
        };
        let Ok(checkpoint) = serde_json::from_slice::<ProjectionCheckpoint<D::State>>(&bytes)
        else {
            return Ok(CheckpointFile::Unusable);
        };
        if self.validate_checkpoint(session_id, &checkpoint) {
            Ok(CheckpointFile::Ours(checkpoint))
        } else {
            Ok(CheckpointFile::Unusable)
        }
    }

    fn validate_checkpoint(
        &self,
        session_id: &SessionId,
        cp: &ProjectionCheckpoint<D::State>,
    ) -> bool {
        cp.session_id == *session_id
            && cp.projection_key.as_str() == self.definition.key()
            && cp.projection_schema_version.as_str() == self.definition.state_schema()
            && cp.state_version == self.definition.state_version()
    }

    /// Whether the log actually holds the event a checkpoint says it folded up
    /// to.
    ///
    /// A checkpoint without this is a state with nothing under it: `build_from`
    /// starts reading *after* `as_of_seq`, so a checkpoint claiming a seq past
    /// the end of the log makes that read come back empty, the fold loop never
    /// run, and the checkpoint's state be returned as the answer — along with
    /// its `as_of_seq`, which callers take as the derivation's revision. Nothing
    /// compares the two, so both the transcript and the revision would be wrong
    /// and neither would say so.
    ///
    /// A cache naming a seq the log does not hold is a miss, not a corruption:
    /// the caller rebuilds from zero. `None` needs no grounding — there is no
    /// claim about the log in it, and `build_from` folds the whole log onto it.
    async fn checkpoint_is_grounded(
        &self,
        session_id: &SessionId,
        cp: &ProjectionCheckpoint<D::State>,
    ) -> Result<bool, ProjectionError> {
        let Some(seq) = cp.as_of_seq else {
            return Ok(true);
        };
        let at_seq = self.store.read_stream(session_id, seq, 1).await?;
        Ok(at_seq.first().is_some_and(|event| event.seq == seq))
    }
}

/// What a checkpoint path held.
enum CheckpointFile<State> {
    /// Nothing at the path.
    Absent,
    /// Something is there, but it is not a checkpoint this projection wrote.
    Unusable,
    /// A checkpoint this projection can reason about.
    Ours(ProjectionCheckpoint<State>),
}

/// Errors from projection build or checkpoint persistence.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("event store error: {0}")]
    Store(#[from] agent_ports::event_store::EventStoreError),
    #[error("unknown required event {event_type} at seq {seq}")]
    UnknownRequiredEvent { event_type: agent_protocol::events::EventType, seq: EventSeq },
    /// A known event the fold cannot read. Distinct from `UnknownRequiredEvent`
    /// because the remedy differs: an unknown type means the log holds something
    /// this build does not know about, while this means the log holds something
    /// this build knows and cannot parse — the record itself is damaged.
    #[error("malformed payload for {event_type} at seq {seq}: {reason}")]
    MalformedEvent { event_type: agent_protocol::events::EventType, seq: EventSeq, reason: String },
    #[error("checkpoint IO error: {0}")]
    CheckpointIo(#[from] std::io::Error),
    #[error("checkpoint serialization error: {0}")]
    CheckpointSerialization(#[from] serde_json::Error),
    #[error("invalid session id: {0}")]
    InvalidSessionId(SessionId),
}
