//! The `POST /v1/systemone` wire, and nothing else.
//!
//! Every Laya-shaped type in this crate lives here. Above this module the
//! vocabulary is [`QuestionPack`](agent_ports::decision::QuestionPack) and
//! [`DecisionResult`](agent_ports::decision::DecisionResult); a change in the
//! sidecar's JSON reaches exactly one file.
//!
//! The shapes below are the server's, verified against `laya 0.3.20`
//! (`laya/serve.py`, `laya/agent.py`) and against a live `laya-serve`:
//!
//! ```text
//! request   { state, questions: { <id>: { type, instructions, criteria } }, model? }
//! response  { model, answers: { <id>: <answer> }, usage, routing }
//! ```
//!
//! Two details are worth naming because they are easy to get wrong:
//!
//! - `criteria` is **an object keyed by option id** for `choice`, and **an array
//!   of level labels** for `score`. The two shapes share a field name and share
//!   nothing else, which is why the value here is untyped: [`crate::mapping`]
//!   builds it, and this module's job is only to put it on the wire unchanged.
//! - `score` is the **expected level index** in `0..levels-1`, not a normalized
//!   score. A five-level `score` of `0.9647` is "near level 1", and the
//!   conversion to an ordinal in `0..=1` is the mapping's business.
//!
//! Only `answers` is read back. `model`, `usage`, `routing`, the probabilities,
//! and the `action` block are the sidecar's own bookkeeping and are deliberately
//! not deserialized — `deny_unknown_fields` would break on the next minor
//! release of the server, and the fields this adapter does not read are fields
//! it would then be keeping for no reason.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One request body.
#[derive(Debug, Serialize)]
pub(crate) struct SystemOneRequest<'a> {
    /// The restricted state the decision is made over. Sent as the
    /// `facts` object alone: the session id, turn id, and log revision are this
    /// project's accounting, and a side model has no use for them.
    pub state: &'a serde_json::Value,
    pub questions: BTreeMap<String, WireQuestion>,
    /// Which checkpoint to use. Absent means the server routes on its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
}

/// One question, in the sidecar's shape.
#[derive(Debug, Serialize)]
pub(crate) struct WireQuestion {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub instructions: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria: Option<serde_json::Value>,
}

/// One response body.
///
/// Only `answers` is read. The `model` the server reports is not kept: which
/// checkpoint answered is the *provider's* pin, asked for once with no question
/// in hand, and a per-call value has nowhere honest to go in a trace that
/// attributes a backend by revision. `serde` ignores what is not named here.
#[derive(Debug, Deserialize)]
pub(crate) struct SystemOneResponse {
    #[serde(default)]
    pub answers: BTreeMap<String, WireAnswer>,
}

/// One answer, tagged by the question shape it answers.
///
/// A `type` this build does not know is a deserialization error, which the
/// provider reports as a protocol failure: the server was reached and returned
/// something this adapter cannot read, and guessing at it would be worse than
/// saying so.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum WireAnswer {
    Choice {
        choice: String,
        /// The calibrated confidence, present on every question type. This is
        /// the number the abstention threshold reads; `confidence` means
        /// different things for different types and `action.act_probability` is
        /// known to be unusable upstream.
        #[serde(default)]
        answer_confidence: Option<f64>,
    },
    Score {
        /// The expected level index in `0..levels-1`.
        score: f64,
        #[serde(default)]
        answer_confidence: Option<f64>,
    },
    Noul {
        /// `P(true)`.
        noul: f64,
        #[serde(default)]
        answer_confidence: Option<f64>,
    },
}

impl WireAnswer {
    /// The calibrated confidence, when the server stated one.
    pub fn answer_confidence(&self) -> Option<f64> {
        match self {
            Self::Choice { answer_confidence, .. }
            | Self::Score { answer_confidence, .. }
            | Self::Noul { answer_confidence, .. } => *answer_confidence,
        }
    }

    /// The question type this answer is shaped for.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
            Self::Noul { .. } => "noul",
        }
    }
}
