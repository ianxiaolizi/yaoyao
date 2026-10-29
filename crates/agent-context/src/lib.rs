//! agent-context: the model's history, derived from the durable session log.
//!
//! This crate is the one place that may depend on both the projection engine
//! and the loop-facing port. The kernel, the CLI, and the loop itself cannot
//! depend on `agent-projection` (that would invert the dependency direction),
//! so the engine that implements [`ContextEngine`] is assembled at the app
//! layer and injected.
//!
//! Checkpoints are refreshed here and never trusted as a source of truth. A
//! derivation folds through `Projection::build`, which already prefers a
//! checkpoint whose key, schema, state version and grounding the log supports
//! and falls back to folding the log otherwise; after a derivation that has
//! advanced far enough past the last one, the engine writes the new state back
//! as a checkpoint. That is the whole of it: the write is a cache refresh, it
//! fails silently (the fold that just happened *is* the answer — a failed write
//! costs the next derivation its shortcut), and `doctor`'s `check_checkpoint`
//! is where a wrong checkpoint would be seen. Only the model-context projection
//! is checkpointed; the session projection folds a log whose cost does not grow
//! with the transcript.

#![deny(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_ports::context_engine::{ContextEngine, ContextEngineError, DerivedContext};
use agent_ports::event_store::EventStore;
use agent_projection::{ModelContextProjection, ModelContextState, Projected, Projection};
use agent_protocol::ids::{EventSeq, SessionId};

pub mod dedupe;
pub mod inspect;
pub mod trim;

/// How many events a derivation has to advance past the last checkpoint before
/// it writes a new one.
///
/// The number trades two costs against each other: a refresh re-serializes the
/// whole folded state, and the events since the last refresh are what a restart
/// has to fold. At 256, a process that drives a session for a hundred events
/// never writes, and one that drives it for ten thousand writes about forty
/// times — each of them during a derivation that was already going to fold less
/// than a full log afterwards.
pub const DEFAULT_CHECKPOINT_EVENTS: u64 = 256;

/// Derives model context by folding the session's durable log.
pub struct ProjectionContextEngine {
    projection: Projection<ModelContextProjection>,
    policy: Option<trim::ContextPolicy>,
    checkpoint_every: Option<u64>,
    /// The seq each session's checkpoint was last refreshed at, for this
    /// process. Per session because one engine serves every session the process
    /// drives; in memory because a fresh process has no way to know what the
    /// file already holds without reading the whole state back, and the worst
    /// case of forgetting is one extra refresh.
    refreshed: Mutex<HashMap<SessionId, EventSeq>>,
}

impl ProjectionContextEngine {
    /// Build an engine over a shared event store.
    ///
    /// `checkpoint_root` is where this projection's checkpoints live: a
    /// disposable cache the engine refreshes and `build` prefers when the log
    /// still supports it.
    pub fn new(store: Arc<dyn EventStore>, checkpoint_root: impl Into<PathBuf>) -> Self {
        Self {
            projection: Projection::new(ModelContextProjection::new(), store, checkpoint_root),
            policy: None,
            checkpoint_every: Some(DEFAULT_CHECKPOINT_EVENTS),
            refreshed: Mutex::new(HashMap::new()),
        }
    }

    /// Refresh the checkpoint every `events` events instead of
    /// [`DEFAULT_CHECKPOINT_EVENTS`].
    ///
    /// `0` refreshes on every derivation that folds anything, for a caller that
    /// wants the cache as warm as it can be and will pay for the writes.
    pub fn with_checkpoint_interval(mut self, events: u64) -> Self {
        self.checkpoint_every = Some(events);
        self
    }

    /// Derive without writing checkpoints, so every derivation folds the log.
    ///
    /// For a caller that wants the fold itself — a benchmark's baseline, a test
    /// asserting what the log alone produces — not for the product, which wants
    /// the shortcut.
    pub fn without_checkpoints(mut self) -> Self {
        self.checkpoint_every = None;
        self
    }

    /// Trim the settled history to fit `policy`.
    ///
    /// Not set by default: an engine with no policy sends what the log holds,
    /// which is the behaviour every existing session already has. The window is
    /// an explicit configuration, never a default that silently changes what a
    /// model sees.
    pub fn with_context_policy(mut self, policy: trim::ContextPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Write the state this derivation folded back as a checkpoint, if enough
    /// has happened since the last one.
    ///
    /// Deliberately infallible. Every derivation has just produced the answer by
    /// folding, so a checkpoint that cannot be written costs the *next*
    /// derivation its shortcut and nothing else; making `derive` fail here would
    /// turn a cache into a dependency. The marker is released on failure so the
    /// next derivation tries again rather than waiting out another interval.
    async fn refresh_checkpoint(
        &self,
        session_id: &SessionId,
        projected: &Projected<ModelContextState>,
    ) {
        let Some(interval) = self.checkpoint_every else {
            return;
        };
        // Nothing folded means an empty state under a seq the log may not even
        // hold yet: there is nothing to save.
        let Some(seq) = projected.as_of_seq else {
            return;
        };
        if !self.claim_refresh(session_id, seq, interval) {
            return;
        }
        if self.projection.checkpoint(session_id, projected).await.is_err() {
            self.release_refresh(session_id);
        }
    }

    /// Claim this derivation's right to refresh the checkpoint, or say it is
    /// not due yet.
    ///
    /// Claiming before writing keeps the two derivations that a busy session can
    /// have in flight from both serializing the same state. A poisoned lock is
    /// recovered rather than propagated: the map is a scheduling hint, and
    /// losing it only makes the next derivation write sooner.
    fn claim_refresh(&self, session_id: &SessionId, seq: EventSeq, interval: u64) -> bool {
        let mut refreshed = self.refreshed.lock().unwrap_or_else(|poison| poison.into_inner());
        let due = match refreshed.get(session_id) {
            Some(previous) => seq.as_u64().saturating_sub(previous.as_u64()) >= interval,
            // The first refresh of a session waits until the fold it would save
            // is at least one interval long: a checkpoint of thirty events pays
            // a file write to save a fold that took no time.
            None => seq.as_u64().saturating_add(1) >= interval,
        };
        if due {
            refreshed.insert(session_id.clone(), seq);
        }
        due
    }

    fn release_refresh(&self, session_id: &SessionId) {
        let mut refreshed = self.refreshed.lock().unwrap_or_else(|poison| poison.into_inner());
        refreshed.remove(session_id);
    }
}

#[async_trait::async_trait]
impl ContextEngine for ProjectionContextEngine {
    async fn derive(&self, session_id: &SessionId) -> Result<DerivedContext, ContextEngineError> {
        let projected = self
            .projection
            .build(session_id)
            .await
            .map_err(|e| ContextEngineError::Derivation(e.to_string()))?;
        self.refresh_checkpoint(session_id, &projected).await;
        let view = self.projection.view(&projected.state);
        // The provider's own count for the latest reported request. It is what
        // the trigger is measured on when present: an estimate of the derived
        // messages alone is roughly half a real request (2026-10-19), so it
        // would let the window fill further than the configuration says.
        let observed_input_tokens = projected
            .state
            .last_usage()
            .and_then(|usage| usage.input_tokens)
            .and_then(|tokens| u32::try_from(tokens).ok());
        // The proposal is made over the whole view, before trimming: once the
        // window starts dropping, the dropped messages are still in the log
        // and can still be summarized.
        let compaction = self
            .policy
            .as_ref()
            .and_then(|policy| trim::compaction_candidate(&view, policy, observed_input_tokens));
        // A repeated identical read is collapsed only when a window is in
        // force: with no window the engine sends what the log holds, and that
        // promise is worth more than the tokens. The proposal above still sees
        // the full text, so a summary is written from the original words.
        let view = if self.policy.is_some() { dedupe::repeated_results(view) } else { view };
        let trimmed = trim::apply(view, self.policy);
        Ok(DerivedContext {
            messages: trimmed.messages,
            // No folded event yet: the context is empty and its revision is
            // the start of the log. The revision names where the history came
            // from, not where the kept slice begins: two derivations of the
            // same prefix must report the same revision even when the window
            // dropped different amounts.
            revision: projected.as_of_seq.unwrap_or(EventSeq::ZERO),
            usage: trimmed.usage,
            observed_input_tokens,
            compaction,
        })
    }
}
