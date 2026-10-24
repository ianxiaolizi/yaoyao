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
use agent_protocol::events::{EventType, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::reason::{ReasonKind, ReasonNode, rule};

use crate::{CliOutcome, CliOutput, json_output};

/// How deep a rendered derivation may go before it is cut off.
///
/// A cycle cannot be written by this build, but a log can be edited; a reader
/// that hangs on a hand-made loop would be worse than one that says where it
/// stopped.
pub const DEFAULT_DEPTH: usize = 64;

/// Execute the `why` command: render the reasoning chain of one turn.
///
/// `depth` bounds how many levels of derivation are shown; `None` uses
/// [`DEFAULT_DEPTH`]. It is a reader's knob, not a semantic one: the same nodes
/// are read and checked either way.
pub async fn run_why(
    store: &dyn EventStore,
    session_id: &SessionId,
    turn: Option<&str>,
    depth: Option<usize>,
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
    let end = TurnEnd::of(&events, &turn_id);
    let verdict = verdict_node(&nodes);
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
            "verdict": verdict.map(|node| serde_json::json!({
                "rule": node.rule,
                "claim": node.claim,
                "step": node.step,
            })),
            "stop": end.as_ref().map(TurnEnd::as_json),
            "check": check.as_ref().map_or_else(|error| error.to_string(), |_| String::from("ok")),
        });
        return Ok(CliOutcome::ok(CliOutput::Json(json_output(&value)?)));
    }
    Ok(CliOutcome::ok(CliOutput::Text(render_turn(
        &turn_id,
        &chain,
        &nodes,
        &event_index,
        end.as_ref(),
        depth.unwrap_or(DEFAULT_DEPTH),
    ))))
}

/// How a turn ended, when the log says so.
///
/// Most terminal events do not carry the reason in their payload: the event
/// type *is* the reason. Only a budget stop needed a field, because
/// `turn/completed {}` could not be told from a turn that ended with nothing to
/// say. This reads both places so a reader does not have to know which.
struct TurnEnd {
    event: String,
    stop: Option<String>,
}

impl TurnEnd {
    fn of(events: &[StoredEvent], turn: &str) -> Option<Self> {
        const TERMINAL: [&str; 4] = [
            EventType::TURN_COMPLETED,
            EventType::TURN_YIELDED,
            EventType::TURN_FAILED,
            EventType::TURN_CANCELLED,
        ];
        events.iter().rev().find_map(|stored| {
            let event = &stored.event;
            let event_type = event.event_type.as_str();
            let same_turn = event.turn_id.as_ref().is_some_and(|id| id.as_str() == turn);
            if !same_turn || !TERMINAL.contains(&event_type) {
                return None;
            }
            Some(Self {
                event: event_type.to_string(),
                stop: event
                    .payload
                    .get("stop_reason")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            })
        })
    }

    /// The stop reason as one machine word.
    fn reason(&self) -> String {
        match (self.event.as_str(), self.stop.as_deref()) {
            (EventType::TURN_COMPLETED, Some(reason)) => reason.to_string(),
            (EventType::TURN_COMPLETED, None) => String::from("final_answer"),
            (EventType::TURN_YIELDED, _) => String::from("needs_user_input"),
            (EventType::TURN_FAILED, _) => String::from("fatal_error"),
            (EventType::TURN_CANCELLED, _) => String::from("user_cancelled"),
            (other, _) => other.to_string(),
        }
    }

    fn as_json(&self) -> serde_json::Value {
        serde_json::json!({ "event": self.event, "reason": self.reason() })
    }
}

/// The latest verdict the turn wrote, if it wrote one.
///
/// A turn cut by the step budget, held for approval, failed or cancelled never
/// reached an answer; the summary has to say that rather than leave the reader
/// wondering why the tree has no end.
fn verdict_node<'a>(nodes: &[&'a ReasonNode]) -> Option<&'a ReasonNode> {
    nodes.iter().rev().find(|node| node.kind == ReasonKind::Verdict).copied()
}

/// The human-readable half: a reverse tree from the turn's ends.
fn render_turn(
    turn: &str,
    chain: &Chain,
    nodes: &[&ReasonNode],
    event_index: &BTreeMap<String, (String, u64)>,
    end: Option<&TurnEnd>,
    max_depth: usize,
) -> String {
    let by_id: BTreeMap<&str, &ReasonNode> =
        nodes.iter().map(|node| (node.id.as_str(), *node)).collect();

    let mut out = format!("turn {turn}: {} reason node(s)\n", nodes.len());
    match end {
        Some(end) => out.push_str(&format!("turn end: {} ({})\n", end.reason(), end.event)),
        None => out.push_str(
            "turn end: not recorded — the turn is still running, or the log stops before it ended\n",
        ),
    }
    match verdict_node(nodes) {
        Some(node) => {
            out.push_str(&format!("verdict: {} — {}\n", node.rule, one_line(&node.claim)))
        }
        None => out
            .push_str("verdict: none — no answer node was written; see how the turn ended above\n"),
    }
    match chain.check(turn) {
        Ok(()) => out.push_str("chain check: every node names a rule and every edge points back\n"),
        Err(error) => out.push_str(&format!("chain check: {error}\n")),
    }

    let mut seen = HashSet::new();
    for root in chain.roots(turn).iter().rev() {
        render_node(root, &by_id, event_index, 0, &mut seen, &mut out, max_depth);
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
    max_depth: usize,
) {
    let indent = "  ".repeat(depth);
    let step = if node.step == 0 { String::new() } else { format!(" (step {})", node.step) };
    let rule_text = rule(&node.rule).map_or("rule unknown in this build", |rule| rule.text);

    if depth > max_depth {
        out.push_str(&format!("{indent}... deeper than {max_depth} levels, stopped here\n"));
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
                Some(parent) => {
                    render_node(parent, by_id, event_index, depth + 1, seen, out, max_depth)
                }
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
        let text = render_turn("t1", &chain, &nodes, &index, None, DEFAULT_DEPTH);
        assert!(text.contains("chain check: every node names a rule"));
        assert!(
            text.contains("verdict: verdict/after-failure — answer: stopped after the failure")
        );
        assert!(text.contains("turn end: not recorded"), "{text}");
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
        let text = render_turn("t1", &chain, &nodes, &index, None, DEFAULT_DEPTH);
        assert!(text.contains("(cited above)"), "{text}");
    }

    /// A turn that never reached an answer says so, and says what stopped it:
    /// "no tree end" and "the budget ran out" are different facts.
    #[test]
    fn a_turn_without_a_verdict_names_the_reason_it_stopped() {
        let (_, mut events) = fixture();
        // Drop the verdict: the turn stopped before writing one.
        events.retain(|stored| {
            let node = &stored.event.payload["node"];
            node["kind"] != serde_json::json!("verdict")
        });
        let yielded = stored(events.len() as u64, EventType::TURN_YIELDED, serde_json::json!({}));
        let mut yielded = yielded;
        yielded.event.turn_id = Some(agent_protocol::ids::TurnId::from_string("t1"));
        events.push(yielded);

        let chain = Chain::parse(&events).expect("readable");
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
        let text = render_turn("t1", &chain, &nodes, &index, None, DEFAULT_DEPTH);
        assert!(text.contains("verdict: none"), "{text}");
        // The text renderer was handed no end, but the node count still holds.
        assert!(text.contains("turn t1: 6 reason node(s)"), "{text}");

        let end = TurnEnd::of(&events, "t1").expect("the yield is the turn's end");
        let text = render_turn("t1", &chain, &nodes, &index, Some(&end), DEFAULT_DEPTH);
        assert!(text.contains("turn end: needs_user_input (turn/yielded)"), "{text}");
    }

    /// `--depth` cuts the derivation, not the summary.
    #[test]
    fn a_depth_limit_stops_the_tree_but_keeps_the_verdict_line() {
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
        let text = render_turn("t1", &chain, &nodes, &index, None, 0);
        assert!(text.contains("[verdict] answer: stopped after the failure"), "{text}");
        assert!(text.contains("... deeper than 0 levels, stopped here"), "{text}");
        assert!(!text.contains("[observation] fs.write failed"), "{text}");
    }

    #[test]
    fn one_line_collapses_the_claims_whitespace() {
        assert_eq!(one_line("a\n b\tc"), "a b c");
    }
}
