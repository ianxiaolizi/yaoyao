//! `why`: read a turn's reasoning chain back from the log.
//!
//! This is a reader, not a run: it opens the session's event log and answers
//! "why did this step happen" from the nodes the loop recorded. It deliberately
//! does not assemble a model, a tool surface or a sandbox — asking why a turn
//! ran must work even when the configuration that ran it has since broken.
//!
//! The output is a tree walked **backwards**: the newest end of the derivation
//! first, then the node that produced it, and so on until the turn's own input.
//! Each line names the rule that fired, so a reader who disagrees with a step
//! knows exactly which table entry to argue with.

use std::collections::{BTreeMap, HashSet};

use agent_kernel::reason::Chain;
use agent_ports::event_store::EventStore;
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::reason::{ReasonNode, rule};

use crate::{CliOutcome, CliOutput, json_output};

/// How deep a rendered derivation may go before it is cut off.
///
/// A cycle cannot be written by this build, but a log can be edited; a reader
/// that hangs on a hand-made loop would be worse than one that says where it
/// stopped.
const MAX_DEPTH: usize = 64;

/// Execute the `why` command: render the reasoning chain of one turn.
pub async fn run_why(
    store: &dyn EventStore,
    session_id: &SessionId,
    turn: Option<&str>,
    json: bool,
) -> Result<CliOutcome, String> {
    let events = store
        .read_stream(session_id, EventSeq::ZERO, usize::MAX)
        .await
        .map_err(|error| format!("cannot read the session log: {error}"))?;
    let chain = Chain::parse(&events)
        .map_err(|error| format!("the reasoning chain is unreadable: {error}"))?;
    if chain.is_empty() {
        return Ok(CliOutcome::did_not_run(CliOutput::Text(String::from(
            "no reasoning chain recorded in this session: it was written by an older build, \
             or no turn has run since the chain was introduced",
        ))));
    }
    let turn_id = match turn {
        Some(turn) => turn.to_string(),
        None => chain.latest_turn().ok_or_else(|| String::from("no turn to explain"))?.to_string(),
    };
    let nodes = chain.turn_nodes(&turn_id);
    if nodes.is_empty() {
        return Ok(CliOutcome::did_not_run(CliOutput::Text(format!(
            "no reasoning chain recorded for turn '{turn_id}'"
        ))));
    }
    let check = chain.check(&turn_id);
    let event_index: BTreeMap<String, (String, u64)> = events
        .iter()
        .map(|stored| {
            (
                stored.event.event_id.as_str().to_string(),
                (stored.event.event_type.as_str().to_string(), stored.seq.as_u64()),
            )
        })
        .collect();
    if json {
        let value = serde_json::json!({
            "session": session_id.as_str(),
            "turn": turn_id,
            "nodes": nodes,
            "check": check.as_ref().map_or_else(|error| error.to_string(), |_| String::from("ok")),
        });
        return Ok(CliOutcome::ok(CliOutput::Json(json_output(&value)?)));
    }
    Ok(CliOutcome::ok(CliOutput::Text(render_turn(&turn_id, &chain, &nodes, &event_index))))
}

/// The human-readable half: a reverse tree from the turn's ends.
fn render_turn(
    turn: &str,
    chain: &Chain,
    nodes: &[&ReasonNode],
    event_index: &BTreeMap<String, (String, u64)>,
) -> String {
    let by_id: BTreeMap<&str, &ReasonNode> =
        nodes.iter().map(|node| (node.id.as_str(), *node)).collect();

    let mut out = format!("turn {turn} has {} reason node(s)\n", nodes.len());
    match chain.check(turn) {
        Ok(()) => out.push_str("chain check: every node names a rule and every edge points back\n"),
        Err(error) => out.push_str(&format!("chain check: {error}\n")),
    }

    let mut seen = HashSet::new();
    for root in chain.roots(turn).iter().rev() {
        render_node(root, &by_id, event_index, 0, &mut seen, &mut out);
    }
    out
}

/// Render one node and, below it, the nodes it was derived from.
fn render_node(
    node: &ReasonNode,
    by_id: &BTreeMap<&str, &ReasonNode>,
    event_index: &BTreeMap<String, (String, u64)>,
    depth: usize,
    seen: &mut HashSet<String>,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    let step = if node.step == 0 { String::new() } else { format!(" (step {})", node.step) };
    let rule_text = rule(&node.rule).map_or("rule unknown in this build", |rule| rule.text);

    if depth >= MAX_DEPTH {
        out.push_str(&format!("{indent}... deeper than {MAX_DEPTH} levels, stopped here\n"));
        return;
    }
    if !seen.insert(node.id.clone()) {
        out.push_str(&format!(
            "{indent}[{}] {} (cited above)\n",
            node.kind.as_str(),
            one_line(&node.claim)
        ));
        return;
    }

    out.push_str(&format!("{indent}[{}] {}{step}\n", node.kind.as_str(), one_line(&node.claim)));
    out.push_str(&format!("{indent}  because {}: {rule_text}\n", node.rule));
    for basis in &node.basis {
        if let Some(id) = &basis.node {
            match by_id.get(id.as_str()) {
                Some(parent) => render_node(parent, by_id, event_index, depth + 1, seen, out),
                None => out.push_str(&format!("{indent}  <- node {id} (not in this turn)\n")),
            }
        } else if let Some(event) = &basis.event {
            match event_index.get(event) {
                Some((kind, seq)) => {
                    out.push_str(&format!("{indent}  <- event {kind} #{seq}\n"));
                }
                None => {
                    out.push_str(&format!("{indent}  <- event {event} (not in this session)\n"))
                }
            }
        } else if let Some(fact) = &basis.fact {
            out.push_str(&format!("{indent}  <- fact: {}\n", one_line(fact)));
        }
    }
}

/// A claim on one line, with runs of whitespace collapsed.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::events::{Actor, EventDraft, EventType};
    use agent_protocol::ids::{EventId, SchemaVersion};
    use agent_protocol::reason::{Basis, ReasonKind, ReasonSource, Recorder};
    use chrono::Utc;

    fn stored(
        seq: u64,
        event_type: &str,
        payload: serde_json::Value,
    ) -> agent_protocol::events::StoredEvent {
        agent_protocol::events::StoredEvent {
            seq: EventSeq::new(seq),
            recorded_at: Utc::now(),
            event: EventDraft {
                event_id: EventId::new(),
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

    fn fixture() -> (Chain, Vec<agent_protocol::events::StoredEvent>) {
        let mut recorder = Recorder::new("t1");
        let situation = recorder.node(
            ReasonKind::Situation,
            0,
            "situation: user turn; 1 write tool",
            vec![Basis::fact("the input: 改 report.txt")],
            "situation/derive",
            ReasonSource::Kernel,
        );
        let reading = recorder.node(
            ReasonKind::Reading,
            0,
            "kernel reading: action request",
            vec![Basis::node(situation.id.clone())],
            "reading/action-cue",
            ReasonSource::Kernel,
        );
        let stance = recorder.node(
            ReasonKind::Stance,
            0,
            "stance: act (guarded)",
            vec![Basis::node(situation.id.clone()), Basis::node(reading.id.clone())],
            "stance/act-guarded",
            ReasonSource::Kernel,
        );
        let action = recorder.node(
            ReasonKind::Action,
            1,
            "fs.write report.txt",
            vec![Basis::node(stance.id.clone())],
            "action/dispatch",
            ReasonSource::Model,
        );
        let observation = recorder.node(
            ReasonKind::Observation,
            1,
            "fs.write failed: disk full",
            vec![Basis::node(action.id.clone())],
            "observation/result",
            ReasonSource::World,
        );
        let switched = recorder.node(
            ReasonKind::Stance,
            1,
            "stance switched: act -> repair (`fs.write` failed)",
            vec![Basis::node(observation.id.clone()), Basis::node(stance.id.clone())],
            "switch/failure-repair",
            ReasonSource::Kernel,
        );
        let verdict = recorder.node(
            ReasonKind::Verdict,
            2,
            "answer: stopped after the failure",
            vec![Basis::node(switched.id.clone()), Basis::node(observation.id.clone())],
            "verdict/after-failure",
            ReasonSource::Model,
        );
        let events: Vec<_> = [situation, reading, stance, action, observation, switched, verdict]
            .iter()
            .enumerate()
            .map(|(index, node)| {
                stored(
                    index as u64,
                    EventType::REASON_RECORDED,
                    serde_json::json!({ "node": node }),
                )
            })
            .collect();
        (Chain::parse(&events).expect("readable"), events)
    }

    #[test]
    fn the_reverse_tree_shows_the_rule_behind_every_step() {
        let (chain, events) = fixture();
        let nodes = chain.turn_nodes("t1");
        let index: BTreeMap<String, (String, u64)> = events
            .iter()
            .map(|stored| {
                (
                    stored.event.event_id.as_str().to_string(),
                    (stored.event.event_type.as_str().to_string(), stored.seq.as_u64()),
                )
            })
            .collect();
        let text = render_turn("t1", &chain, &nodes, &index);
        assert!(text.contains("chain check: every node names a rule"));
        assert!(text.contains("[verdict] answer: stopped after the failure (step 2)"));
        assert!(text.contains("because verdict/after-failure:"));
        assert!(text.contains("[stance] stance switched: act -> repair"));
        assert!(text.contains("because switch/failure-repair:"));
        assert!(text.contains("[observation] fs.write failed"));
        assert!(text.contains("because action/dispatch:"));
        assert!(text.contains("<- fact: the input: 改 report.txt"));
        // The root is the verdict, and the input is the deepest line.
        let verdict = text.find("[verdict]").expect("a verdict");
        let input = text.find("the input: 改 report.txt").expect("the input");
        assert!(verdict < input);
    }

    #[test]
    fn a_cited_twice_node_is_named_rather_than_repeated() {
        let (chain, events) = fixture();
        let nodes = chain.turn_nodes("t1");
        let index: BTreeMap<String, (String, u64)> = events
            .iter()
            .map(|stored| {
                (
                    stored.event.event_id.as_str().to_string(),
                    (stored.event.event_type.as_str().to_string(), stored.seq.as_u64()),
                )
            })
            .collect();
        let text = render_turn("t1", &chain, &nodes, &index);
        assert!(text.contains("(cited above)"), "{text}");
    }

    #[test]
    fn one_line_collapses_the_claims_whitespace() {
        assert_eq!(one_line("a\n b\tc"), "a b c");
    }
}
