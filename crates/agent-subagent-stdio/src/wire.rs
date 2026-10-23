//! The stdio wire: one line in, several lines out.
//!
//! A child process that speaks this protocol reads exactly one line of JSON on
//! stdin — its request — and writes JSON lines on stdout. Everything else about
//! the child is its own business: which language it is written in, whether it
//! talks to a model, whether it has a loop. This module is the whole of the
//! agreement, and it is separate from [`crate::provider`] so that the agreement
//! can be tested without starting a process for every case.
//!
//! ## The two halves are not symmetric, on purpose
//!
//! The parent's side is closed: it writes one message, [`RequestMessage`], and
//! closes stdin. There is no second line, so a child never waits for one.
//!
//! The child's side is open where it can be and strict where it must be:
//!
//!   * **Unknown fields are tolerated.** A child that also reports a `duration`
//!     or a `trace_id` is not wrong, and a parent that refused it could never
//!     learn anything the wire did not already anticipate. Tolerating extra
//!     fields is what lets the child's side grow without a flag day.
//!   * **Unknown message types and unknown statuses are refused.** A line whose
//!     `type` this module does not define means the two sides disagree about
//!     what they are speaking — the exact situation in which continuing to read
//!     produces a confident answer built on a misunderstanding. The child is put
//!     down and the delegation ends `Failed` with the line quoted.
//!
//! The asymmetry is the same rule in both places: extra *data* is harmless, a
//! disagreement about *meaning* is not.

use agent_ports::subagent::SubagentOutcome;
use agent_protocol::model::ModelRoute;
use agent_protocol::versioning::{self, Version, VersionError, formats};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What the parent writes as its one line of stdin.
///
/// It carries exactly the fields whose features this backend advertises: a
/// request asking for something else is refused in [`crate::provider`] before a
/// process exists, so there is no shape here for a message that will not be
/// sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "request")]
pub struct RequestMessage {
    /// The protocol version this request is written in (YA-20).
    ///
    /// An old child tolerates the field and ignores it, which is the point:
    /// the version is addressed to a child that *does* know it, and its absence
    /// on a child's reply means a child that predates the field — read as
    /// current, because the shapes it writes are the shapes this build parses.
    #[serde(default = "agent_protocol::versioning::current_subagent_wire")]
    pub protocol_version: Version,
    /// What the child is for, in the parent's words.
    pub objective: String,
    /// References to context the child may want to resolve for itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
    /// Tools the child may use, if the parent restricted them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_allowlist: Vec<String>,
    /// The model route the child should run on, if the parent pinned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_route: Option<ModelRoute>,
    /// The shape the child's answer must have, if the parent required one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// How long the child has, in milliseconds. The effective deadline is the
    /// smaller of this and the backend's configured ceiling; what is written
    /// here is the effective one, so the child is told the deadline it is
    /// actually held to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl RequestMessage {
    /// The request as it goes on the wire, with the deadline the backend will
    /// actually enforce.
    ///
    /// `effective_timeout_ms` is passed in rather than read from the request so
    /// that the child cannot be told a deadline the parent is not holding it to:
    /// the ceiling is the backend's, and it is the backend that has to apply it.
    pub fn of(request: &agent_ports::subagent::SubagentRequest, effective_timeout_ms: u64) -> Self {
        Self {
            protocol_version: formats::SUBAGENT_WIRE,
            objective: request.objective.clone(),
            context_refs: request.context_refs.clone(),
            tool_allowlist: request.tool_allowlist.clone(),
            model_route: request.model_route.clone(),
            output_schema: request.output_schema.clone(),
            timeout_ms: Some(effective_timeout_ms),
        }
    }

    /// The one line to write, newline included.
    pub fn line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// What the child wrote, once it is understood.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildMessage {
    /// The child is still working. Accepted and discarded: the port has no
    /// progress channel, so there is nowhere to put it — but a child that says
    /// it is alive is not misbehaving, and killing it for being talkative would
    /// be worse than ignoring it.
    Progress { message: String },
    /// The child's answer, and the end of the delegation.
    Result(SubagentOutcome),
}

/// Why a line could not be understood.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("the line is not JSON: {message}")]
    NotJson { message: String },
    #[error("the line is not a message this wire defines: {message}")]
    Unknown { message: String },
    #[error("the child reported '{status}', which does not fit what it sent: {problem}")]
    Inconsistent { status: String, problem: String },
    /// The line names a wire version this build does not read (YA-20). The
    /// error says what the child claimed, which is the difference between a
    /// version mismatch and "I did not understand that message".
    #[error(transparent)]
    Version(#[from] VersionError),
}

/// Read one line from the child.
///
/// A line that is not a message this wire defines is an `Err`, and there are
/// three ways to be one: not JSON at all, a type this wire does not know, and a
/// `result` whose fields do not fit the status it claimed. There is no third
/// answer, so "no more output" is EOF, which is the caller's to interpret.
pub fn parse(line: &str) -> Result<ChildMessage, WireError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| WireError::NotJson { message: error.to_string() })?;

    // The version is checked before the type: a line from a newer wire may use
    // a type this build has never heard of, and "version 2" is a more useful
    // answer than "unknown type".
    check_wire_version(&value)?;

    let kind = value.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
    match kind.as_str() {
        "progress" => {
            let message = value.get("message").and_then(Value::as_str).ok_or_else(|| {
                WireError::Inconsistent {
                    status: "progress".to_string(),
                    problem: "no 'message' field".to_string(),
                }
            })?;
            Ok(ChildMessage::Progress { message: message.to_string() })
        }
        "result" => result_from(&value).map(ChildMessage::Result),
        other => Err(WireError::Unknown { message: format!("unknown message type '{other}'") }),
    }
}

/// Refuse a line whose `protocol_version` this build cannot read.
///
/// Absent is current. An external child written before the field existed has no
/// way to name a version, and the messages it writes are the ones this build
/// already understood — rejecting them would make the field a flag day rather
/// than a boundary. A child that *does* name a version is held to it.
fn check_wire_version(value: &Value) -> Result<(), WireError> {
    let found = match value.get("protocol_version") {
        None => formats::SUBAGENT_WIRE,
        Some(Value::String(text)) => Version::parse(text)?,
        Some(other) => {
            return Err(VersionError::Malformed {
                found: other.to_string(),
                detail: "`protocol_version` must be a string such as \"1\"".to_string(),
            }
            .into());
        }
    };
    versioning::check(&found, &formats::SUBAGENT_WIRE)?;
    Ok(())
}

/// Turn a `result` line into the outcome it says it is.
///
/// The variants are read one at a time rather than through a derived enum so
/// that a status whose required field is missing fails with a sentence naming
/// both — a child that says `completed` and gives no summary is not a child that
/// completed, and the message should say so rather than deserialize to an empty
/// string.
///
/// The status is matched as a string, and there is deliberately no enum behind
/// it: the three values below are the whole vocabulary a child may use, and they
/// are not [`agent_ports::subagent::SubagentStatus`]. A child cannot report that
/// it was cancelled or that it ran out of time — those are the parent's and the
/// backend's story, and the backend is what stops a child, so a child claiming
/// `cancelled` would be describing something only the other side can know it
/// did. An enum here said the wire was that vocabulary while nothing routed
/// through it; the `other` arm is where the refusal actually happens (YA-47).
fn result_from(value: &Value) -> Result<SubagentOutcome, WireError> {
    let status = value.get("status").and_then(Value::as_str).unwrap_or_default().to_string();
    let text = |field: &str| value.get(field).and_then(Value::as_str).map(str::to_string);
    let missing = |field: &str| WireError::Inconsistent {
        status: status.clone(),
        problem: format!("no '{field}' field"),
    };

    match status.as_str() {
        "completed" => Ok(SubagentOutcome::Completed {
            summary: text("summary").ok_or_else(|| missing("summary"))?,
            result: value.get("result").cloned(),
        }),
        "partial" => Ok(SubagentOutcome::Partial {
            summary: text("summary").ok_or_else(|| missing("summary"))?,
        }),
        "failed" => Ok(SubagentOutcome::Failed {
            message: text("message").ok_or_else(|| missing("message"))?,
            side_effects: value
                .get("side_effects")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
        }),
        "" => Err(WireError::Inconsistent {
            status: "none".to_string(),
            problem: "no 'status' field".to_string(),
        }),
        other => Err(WireError::Unknown {
            message: format!(
                "unknown result status '{other}' (a child may report completed, partial or failed)"
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::subagent::SubagentRequest;
    use agent_protocol::model::ModelTier;

    #[test]
    fn a_request_goes_out_as_one_line_with_the_deadline_it_will_be_held_to() {
        let request = SubagentRequest {
            context_refs: vec!["artifact://notes".to_string()],
            tool_allowlist: vec!["fs.read".to_string()],
            model_route: Some(ModelRoute::new(ModelTier::Fast, "openai", "small")),
            output_schema: Some(serde_json::json!({ "type": "object" })),
            ..SubagentRequest::new("summarize the notes")
        };

        let message = RequestMessage::of(&request, 2_500);
        assert_eq!(message.timeout_ms, Some(2_500), "the child is told the effective deadline");

        let line = message.line().expect("a request serializes");
        assert!(line.ends_with('\n'), "a request is one line, newline included");
        assert_eq!(line.matches('\n').count(), 1, "{line}");

        let sent: Value = serde_json::from_str(line.trim_end()).expect("it is JSON");
        assert_eq!(sent["type"], "request");
        assert_eq!(sent["objective"], "summarize the notes");
        assert_eq!(sent["timeout_ms"], 2_500);
        assert_eq!(sent["tool_allowlist"][0], "fs.read");
        assert_eq!(sent["model_route"]["tier"], "fast");
        assert_eq!(sent["protocol_version"], "1.0.0", "the child is told the wire's version");

        // Round-trips into the same message, so the fixture and the parent are
        // reading one description of the wire rather than two.
        let decoded: RequestMessage = serde_json::from_str(line.trim_end()).expect("readable");
        assert_eq!(decoded, message);
    }

    #[test]
    fn a_child_from_a_future_wire_is_refused_with_its_version_in_the_message() {
        let line =
            r#"{"type":"result","status":"completed","summary":"done","protocol_version":"99"}"#;
        let error = parse(line).expect_err("a future wire is not this wire");
        assert!(matches!(error, WireError::Version(VersionError::Future { .. })), "{error}");
        assert!(
            error.to_string().contains("99"),
            "the error names what the child claimed: {error}"
        );

        // A version this build has no migration from is named too — it is not
        // silently read as "old enough".
        let line = r#"{"type":"progress","message":"working","protocol_version":"0"}"#;
        let error = parse(line).expect_err("an unmigratable wire is refused");
        assert!(matches!(error, WireError::Version(VersionError::NoMigration { .. })), "{error}");
    }

    #[test]
    fn a_child_that_predates_the_version_field_is_still_read() {
        // The compatibility decision is explicit: no version means the shape
        // this build already parses. Every fixture written before YA-20 is
        // this case.
        let old = r#"{"type":"result","status":"completed","summary":"done"}"#;
        assert_eq!(
            parse(old).expect("a pre-versioning child is readable"),
            ChildMessage::Result(SubagentOutcome::Completed {
                summary: "done".to_string(),
                result: None,
            })
        );
    }

    #[test]
    fn a_request_that_asked_for_nothing_does_not_send_empty_boxes() {
        // A line full of empty arrays and nulls would be noise that a child has
        // to skip; the wire says what was asked for and nothing else.
        let line =
            RequestMessage::of(&SubagentRequest::new("look"), 1_000).line().expect("serializes");
        let sent: Value = serde_json::from_str(line.trim_end()).expect("JSON");

        assert!(sent.get("context_refs").is_none(), "{sent}");
        assert!(sent.get("tool_allowlist").is_none(), "{sent}");
        assert!(sent.get("model_route").is_none(), "{sent}");
        assert!(sent.get("output_schema").is_none(), "{sent}");
    }

    #[test]
    fn a_completed_child_answers_with_its_summary_and_whatever_it_returned() {
        let message = parse(r#"{"type":"result","status":"completed","summary":"three notes","result":{"count":3}}"#)
            .expect("a well-formed result");

        assert_eq!(
            message,
            ChildMessage::Result(SubagentOutcome::Completed {
                summary: "three notes".to_string(),
                result: Some(serde_json::json!({ "count": 3 })),
            })
        );
    }

    #[test]
    fn a_failed_child_carries_what_it_says_it_changed() {
        // A failed child may still have changed the world, which is why the
        // design asks for the side effects and why they survive the wire.
        let message = parse(
            r#"{"type":"result","status":"failed","message":"no such file","side_effects":["created /tmp/x"]}"#,
        )
        .expect("a well-formed failure");

        assert_eq!(
            message,
            ChildMessage::Result(SubagentOutcome::Failed {
                message: "no such file".to_string(),
                side_effects: vec!["created /tmp/x".to_string()],
            })
        );
    }

    #[test]
    fn a_partial_result_is_taken_only_from_the_childs_own_words() {
        let message = parse(r#"{"type":"result","status":"partial","summary":"two of three"}"#)
            .expect("a well-formed partial");
        assert_eq!(
            message,
            ChildMessage::Result(SubagentOutcome::Partial { summary: "two of three".to_string() })
        );
    }

    #[test]
    fn progress_is_accepted_and_discarded() {
        // Reading it and dropping it is the honest behaviour while the port has
        // no progress channel; killing a child for saying it is alive would not
        // be.
        assert_eq!(
            parse(r#"{"type":"progress","message":"reading notes"}"#).expect("well-formed"),
            ChildMessage::Progress { message: "reading notes".to_string() }
        );
    }

    #[test]
    fn extra_fields_a_child_adds_are_tolerated() {
        // What lets the child's side grow without a flag day.
        let message = parse(
            r#"{"type":"result","status":"completed","summary":"done","trace_id":"abc","tokens":42}"#,
        )
        .expect("extra fields are not an error");
        assert!(matches!(message, ChildMessage::Result(SubagentOutcome::Completed { .. })));

        let request: RequestMessage =
            serde_json::from_str(r#"{"type":"request","objective":"look","deadline_hint":"soon"}"#)
                .expect("a child reading a request must tolerate a parent that adds a field too");
        assert_eq!(request.objective, "look");
    }

    #[test]
    fn a_message_type_the_wire_does_not_define_is_refused() {
        // The other half: extra data is harmless, a disagreement about meaning
        // is not. `final` is a plausible name for a child to invent, and it is
        // exactly the invention this refusal exists to catch.
        let refused = parse(r#"{"type":"final","status":"completed","summary":"done"}"#)
            .expect_err("an unknown type is refused");
        assert!(matches!(refused, WireError::Unknown { .. }), "{refused}");
        assert!(refused.to_string().contains("final"), "{refused}");
    }

    #[test]
    fn a_status_a_child_cannot_report_is_refused() {
        // A child describing its own cancellation would be describing something
        // only the parent can know it did.
        for status in ["cancelled", "timed_out", "cancel_uncertain", "running"] {
            let line = format!(r#"{{"type":"result","status":"{status}","summary":"x"}}"#);
            let refused = parse(&line).err().unwrap_or_else(|| panic!("{status} was accepted"));
            assert!(matches!(refused, WireError::Unknown { .. }), "{status}: {refused}");
            assert!(refused.to_string().contains(status), "{status}: {refused}");
        }
    }

    #[test]
    fn a_result_whose_field_does_not_fit_its_status_is_refused_by_name() {
        // Silence is not the same as an empty answer: a child that says it
        // completed and gives no summary would otherwise become a completed
        // outcome with nothing in it, which reads exactly like a child that
        // answered with nothing to say.
        let refused = parse(r#"{"type":"result","status":"completed"}"#).expect_err("refused");
        assert!(matches!(refused, WireError::Inconsistent { .. }), "{refused}");
        assert!(refused.to_string().contains("summary"), "{refused}");

        let refused = parse(r#"{"type":"result","status":"failed"}"#).expect_err("refused");
        assert!(refused.to_string().contains("message"), "{refused}");

        let refused = parse(r#"{"type":"result","summary":"no status at all"}"#)
            .expect_err("a result with no status is refused");
        assert!(refused.to_string().contains("status"), "{refused}");
    }

    #[test]
    fn a_line_that_is_not_json_is_refused_as_such() {
        // The two failures are kept apart because they send a reader to
        // different places: one to the child's output formatting, the other to
        // its protocol.
        let refused = parse("not json at all").expect_err("refused");
        assert!(matches!(refused, WireError::NotJson { .. }), "{refused}");

        let empty = parse("").expect_err("refused");
        assert!(matches!(empty, WireError::NotJson { .. }), "{empty}");
    }
}
