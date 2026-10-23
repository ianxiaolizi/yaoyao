//! agent-decision-laya: the local fast-decision sidecar as a backend.
//!
//! [Laya](https://pypi.org/project/laya/) is a small non-autoregressive model
//! that answers typed decisions in one forward pass. It is a *backend*, not a
//! participant: it goes behind [`FastDecisionProvider`] and is attached to the
//! broker with one `with_backend` call, so nothing above this crate — the loop,
//! the router, the kernel — learns that it exists.
//!
//! Three things Laya does not do, which this adapter therefore does:
//!
//! - **It does not abstain.** Every question comes back with an answer. The
//!   adapter synthesizes an abstention from `answer_confidence`: one answer
//!   below the threshold abstains the **whole pack**, so "the model is not sure
//!   about one question" is recorded as
//!   [`DecisionBackendOutcome::Abstained`] rather than served as a decision
//!   that silently omitted that question.
//! - **It does not bound its own time.** A `laya-serve` worker runs inference
//!   synchronously, so a slow request also stalls `/health`. The deadline stays
//!   where the broker put it; the client here carries only a socket-level bound
//!   so a wedged connection cannot leak forever.
//! - **It has no notion of a pack.** `pack_id`/`pack_version` are this project's
//!   accounting. The adapter stamps the pack it was handed onto the result, so
//!   a trace still names the question set that was asked.
//!
//! What it must not do is decide *reachability policy* the way a person would
//! like it to be. `laya-serve` binds `0.0.0.0` by default; this adapter refuses
//! any endpoint that is not loopback, at construction, because a decision
//! sidecar is a local artifact and a remote one would be a side channel for the
//! conversation.
//!
//! See [`FastDecisionProvider`] for the port and [`DecisionBroker`] for the
//! selection and comparison this plugs into.
//!
//! [`DecisionBackendOutcome::Abstained`]: agent_ports::decision::DecisionBackendOutcome::Abstained
//! [`DecisionBroker`]: agent_ports::decision

#![deny(unsafe_code)]

/// The Laya release this adapter's wire shape was verified against.
///
/// The request/response vocabulary in `wire` is not versioned on the wire,
/// so nothing here can detect a server upgrade: a `laya 0.4` that changed the
/// shape would be answered by the fixture (which shares this assumption) and
/// by the unit tests alike, and only a live call would notice. This constant is
/// the pin that makes that drift visible in review — moving it means
/// re-checking against a live `laya-serve`, not just renaming a number.
pub const LAYA_PINNED_VERSION: &str = "0.3.20";

pub mod endpoint;
pub mod mapping;
pub mod provider;
mod wire;

pub use endpoint::{EndpointError, LayaEndpoint};
pub use mapping::DEFAULT_ORDINAL_LEVELS;
pub use provider::{DEFAULT_ABSTAIN_THRESHOLD, LayaConfig, LayaConfigError, LayaDecisionProvider};
