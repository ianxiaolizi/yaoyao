//! agent-projection: derive read-only state from the durable event log.
//!
//! Projections are pure folds over `StoredEvent`s. The engine rebuilds state
//! from the `EventStore`, caching progress in checkpoints that are always
//! disposable and rebuildable.

#![deny(unsafe_code)]

pub mod checkpoint;
pub mod definition;
pub mod engine;
pub mod model_context;
pub mod session;

pub use checkpoint::ProjectionCheckpoint;
pub use definition::{ApplyError, ProjectionDefinition};
pub use engine::{CheckpointVerdict, Projected, Projection, ProjectionError};
pub use model_context::{ModelContextProjection, ModelContextState};
pub use session::{SessionProjection, SessionState, SessionView};
