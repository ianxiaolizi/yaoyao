//! agent-decision: the broker, the deterministic baseline, and the families.
//!
//! This crate is where "fast decision" becomes concrete: which backends exist,
//! in what order they are asked, what happens when one fails, and what a
//! decision leaves behind. It implements the ports in `agent-ports` and knows
//! nothing about vendors — a Laya sidecar, a local classifier, and an LLM are
//! all just [`FastDecisionProvider`](agent_ports::decision::FastDecisionProvider)
//! implementations that live elsewhere.
//!
//! What it deliberately does not do:
//!
//! - **Act.** A decision influences nothing here. The broker hands back a trace;
//!   a caller that wants to act on one needs a mode this build refuses to grant
//!   (see [`DecisionBrokerError::ModeNotAvailable`]).
//! - **Persist.** Traces become durable events where they are produced — in the
//!   loop — because the event store is not this crate's to write.
//! - **Own a model.** The router is built around a route the caller chose, so
//!   no process-wide default decides who gets asked.
//!
//! [`replay`] is the other half of producing a trace: it re-derives a recorded
//! decision's conclusion from the record, so what a trace claims can be checked
//! rather than believed.

#![deny(unsafe_code)]

pub mod assembly;
pub mod broker;
pub mod deadline;
pub mod model_route;
pub mod replay;
pub mod router;
pub mod rule;

pub use assembly::shadow_for;
pub use broker::{DEFAULT_TIMEOUT, DecisionBroker, DecisionBrokerError};
pub use deadline::{DEFAULT_ADVISOR_DEADLINE, DeadlineAdvisor};
pub use replay::{ReplayMismatch, Replayed, replay, verify};
pub use router::TableModelRouter;
pub use rule::{RuleAnswer, RuleDecision, RuleDecisionProvider};
