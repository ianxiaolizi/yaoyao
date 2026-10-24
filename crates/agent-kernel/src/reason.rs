//! Fold for the session's reasoning chain.
//!
//! The kernel cannot depend on agent-projection, so the chain is read straight
//! from the event stream here, the way the plan and the judgment are. Each
//! `reason/recorded` event carries one complete node; the fold only orders them
//! and checks the edges, it does not reconstruct them. That keeps a node
//! readable exactly as it was written, which is what "trace it back" means.
//!
//! It deliberately does not decide whether a node is *right*. The rules live in
//! `agent_protocol::reason::RULES`; this module checks that every edge points
//! at a node that was already recorded and that every named rule still exists
//! in this build. An unknown rule is reported rather than dropped: a log written
//! by an older build is still evidence, and hiding the line that cannot be
//! explained would make the chain look complete when it is not.

use std::collections::BTreeSet;

use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::reason::{ReasonNode, rule};

/// The reasoning chain of a session, in log order.
#[derive(Debug, Clone, Default)]
pub struct Chain {
    nodes: Vec<ReasonNode>,
}

impl Chain {
    /// Read every reason node out of the session's events.
    ///
    /// A malformed payload is an error rather than a skipped line: the node was
    /// written by this build, and a `why` that silently drops the step it could
    /// not read would answer a narrower question than the one that was asked.
    pub fn parse(events: &[StoredEvent]) -> Result<Self, serde_json::Error> {
        let mut nodes = Vec::new();
        for stored in events {
            if stored.event.event_type.as_str() != EventType::REASON_RECORDED {
                continue;
            }
            let node = serde_json::from_value(
                stored.event.payload.get("node").cloned().unwrap_or_default(),
            )?;
            nodes.push(node);
        }
        Ok(Self { nodes })
    }

    /// Every node, in log order.
    pub fn nodes(&self) -> &[ReasonNode] {
        &self.nodes
    }

    /// Whether any node was recorded at all.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The turn of the most recently recorded node.
    pub fn latest_turn(&self) -> Option<&str> {
        self.nodes.last().map(|node| node.turn_id.as_str())
    }

    /// The nodes of one turn, in log order.
    pub fn turn_nodes(&self, turn_id: &str) -> Vec<&ReasonNode> {
        self.nodes.iter().filter(|node| node.turn_id == turn_id).collect()
    }

    /// The nodes of one turn that no later node of the same turn cites.
    ///
    /// These are the ends of the derivation — the answer, and any call that was
    /// never observed. `why` renders a tree from each.
    pub fn roots(&self, turn_id: &str) -> Vec<&ReasonNode> {
        let turn = self.turn_nodes(turn_id);
        let cited: BTreeSet<&str> = turn
            .iter()
            .flat_map(|node| node.basis.iter())
            .filter_map(|basis| basis.node.as_deref())
            .collect();
        turn.into_iter().filter(|node| !cited.contains(node.id.as_str())).collect()
    }

    /// Check the graph: ids unique, every edge points backwards, rules known.
    ///
    /// The rules check is a build-compatibility check, not a correctness one: a
    /// rule that has been removed still explains an old node, but this build
    /// cannot print its text, so the gap is reported.
    pub fn check(&self, turn_id: &str) -> Result<(), ChainError> {
        let turn = self.turn_nodes(turn_id);
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut problems = Vec::new();
        for node in &turn {
            if !seen.insert(node.id.as_str()) {
                problems.push(format!("{} appears twice", node.id));
            }
            if node.basis.is_empty() {
                problems.push(format!("{} has no basis", node.id));
            }
            if rule(&node.rule).is_none() {
                problems.push(format!("{} names unknown rule `{}`", node.id, node.rule));
            }
            for basis in &node.basis {
                let Some(edge) = basis.node.as_deref() else { continue };
                if !seen.contains(edge) {
                    problems
                        .push(format!("{} cites {edge}, which is not an earlier node", node.id));
                }
            }
        }
        if problems.is_empty() { Ok(()) } else { Err(ChainError::Broken(problems.join("; "))) }
    }
}

/// A chain whose edges do not form a derivation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("{0}")]
    Broken(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft};
    use agent_protocol::ids::{EventSeq, SchemaVersion, SessionId};
    use agent_protocol::reason::{Basis, ReasonKind, ReasonSource, Recorder};
    use chrono::Utc;

    fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
        StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: agent_protocol::ids::EventId::new(),
                event_type: EventType::new(event_type),
                schema_version: SchemaVersion::from_string("0.1.0"),
                session_id: SessionId::from_string("s"),
                run_id: None,
                turn_id: None,
                step_id: None,
                actor: Actor::Agent,
                created_at: Utc::now(),
                causation_id: None,
                correlation_id: None,
                payload,
                metadata: serde_json::json!({}),
                ignorable: false,
            },
        }
    }

    fn reason_event(seq: u64, node: &ReasonNode) -> StoredEvent {
        stored(seq, EventType::REASON_RECORDED, serde_json::json!({ "node": node }))
    }

    fn chain() -> (Chain, ReasonNode, ReasonNode, ReasonNode) {
        let mut recorder = Recorder::new("t1");
        let situation = recorder.node(
            ReasonKind::Situation,
            0,
            "situation: user turn",
            vec![Basis::fact("the turn opened")],
            "situation/derive",
            ReasonSource::Kernel,
        );
        let stance = recorder.node(
            ReasonKind::Stance,
            0,
            "stance: act",
            vec![Basis::node(situation.id.clone())],
            "stance/act-request",
            ReasonSource::Kernel,
        );
        let action = recorder.node(
            ReasonKind::Action,
            1,
            "fs.read report.txt",
            vec![Basis::node(stance.id.clone())],
            "action/dispatch",
            ReasonSource::Model,
        );
        let event = reason_event(1, &situation);
        let mut events = vec![event];
        events.push(reason_event(2, &stance));
        events.push(reason_event(3, &action));
        let chain = Chain::parse(&events).expect("readable");
        (chain, situation, stance, action)
    }

    #[test]
    fn nodes_come_back_in_log_order_and_keep_their_bases() {
        let (chain, _, stance, action) = chain();
        assert_eq!(chain.nodes().len(), 3);
        assert_eq!(chain.latest_turn(), Some("t1"));
        assert_eq!(action.basis[0].node.as_deref(), Some(stance.id.as_str()));
    }

    #[test]
    fn the_answer_is_the_only_root_when_every_node_has_a_parent() {
        let (chain, _, _, action) = chain();
        let roots = chain.roots("t1");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, action.id);
        assert!(chain.check("t1").is_ok());
    }

    #[test]
    fn a_broken_edge_is_reported_rather_than_hidden() {
        let (_, situation, _, _) = chain();
        let mut recorder = Recorder::new("t1");
        let orphan = recorder.node(
            ReasonKind::Verdict,
            1,
            "answer",
            vec![Basis::node("r/t1/99")],
            "verdict/answer",
            ReasonSource::Model,
        );
        let events = vec![reason_event(1, &situation), reason_event(2, &orphan)];
        let chain = Chain::parse(&events).expect("readable");
        let error = chain.check("t1").expect_err("the edge is broken");
        assert!(error.to_string().contains("not an earlier node"), "{error}");
    }

    #[test]
    fn a_payload_that_will_not_read_back_is_an_error() {
        let broken =
            stored(1, EventType::REASON_RECORDED, serde_json::json!({ "node": { "id": 3 } }));
        assert!(Chain::parse(&[broken]).is_err());
    }

    #[test]
    fn unrelated_events_are_skipped() {
        let events = vec![
            stored(1, EventType::TURN_STARTED, serde_json::json!({})),
            stored(2, EventType::TOOL_STARTED, serde_json::json!({})),
        ];
        assert!(Chain::parse(&events).expect("readable").is_empty());
    }
}
