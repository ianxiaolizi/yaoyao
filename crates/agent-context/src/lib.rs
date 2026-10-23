//! agent-context: the model's history, derived from the durable session log.
//!
//! This crate is the one place that may depend on both the projection engine
//! and the loop-facing port. The kernel, the CLI, and the loop itself cannot
//! depend on `agent-projection` (that would invert the dependency direction),
//! so the engine that implements [`ContextEngine`] is assembled at the app
//! layer and injected.
//!
//! Nothing here writes checkpoints. The derived transcript grows with the
//! session, so a checkpoint would buy real time on long sessions — but a
//! checkpoint that deserializes cleanly while its `state_version` no longer
//! matches the fold is a silent way to serve a stale conversation, and the
//! session lengths this supports today do not pay for that risk. `Projection`
//! reads a checkpoint when one exists, so turning them on later is a write, not
//! a redesign.

#![deny(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use agent_ports::context_engine::{ContextEngine, ContextEngineError, DerivedContext};
use agent_ports::event_store::EventStore;
use agent_projection::{ModelContextProjection, Projection};
use agent_protocol::ids::{EventSeq, SessionId};

pub mod dedupe;
pub mod inspect;
pub mod trim;

/// Derives model context by folding the session's durable log.
pub struct ProjectionContextEngine {
    projection: Projection<ModelContextProjection>,
    policy: Option<trim::ContextPolicy>,
}

impl ProjectionContextEngine {
    /// Build an engine over a shared event store.
    ///
    /// `checkpoint_root` is where this projection's checkpoints would live; it
    /// is part of the engine's contract even though nothing writes one today.
    pub fn new(store: Arc<dyn EventStore>, checkpoint_root: impl Into<PathBuf>) -> Self {
        Self {
            projection: Projection::new(ModelContextProjection::new(), store, checkpoint_root),
            policy: None,
        }
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
}

#[async_trait::async_trait]
impl ContextEngine for ProjectionContextEngine {
    async fn derive(&self, session_id: &SessionId) -> Result<DerivedContext, ContextEngineError> {
        let projected = self
            .projection
            .build(session_id)
            .await
            .map_err(|e| ContextEngineError::Derivation(e.to_string()))?;
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
