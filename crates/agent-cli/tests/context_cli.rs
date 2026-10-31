//! `context inspect` rendering: the numbers come from the builder, the words
//! are decided here.

use agent_cli::CliOutput;
use agent_cli::context::run_context_inspect;
use agent_ports::context_engine::{
    ContextCategoryKind, ContextCategoryUsage, ContextUsageReport, LargestMessage,
};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId, TurnId};
use agent_protocol::model::MessageRole;
use chrono::{TimeZone, Utc};

fn stored(seq: u64, event_type: &str, payload: serde_json::Value) -> StoredEvent {
    let at = Utc.with_ymd_and_hms(2026, 10, 18, 12, 0, 0).unwrap();
    StoredEvent {
        seq: EventSeq::new(seq),
        recorded_at: at,
        event: EventDraft {
            event_id: EventId::from_string(format!("evt-{seq}")),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::from_string("0.1.0"),
            session_id: SessionId::from_string("default"),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor: Actor::Agent,
            created_at: at,
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: serde_json::json!({}),
            ignorable: false,
        },
    }
}

fn prepared(seq: u64, attempt: &str, notice: Option<serde_json::Value>) -> StoredEvent {
    let mut payload = serde_json::json!({
        "attempt_id": attempt,
        "context_revision": 7,
        "message_count": 3,
        "context_message_count": 5,
        "dropped_message_count": 2,
        "layout_version": 2,
        "estimated_context_tokens": 500,
        "estimated_messages_tokens": 420,
        "estimated_tool_tokens": 60,
        "estimated_request_tokens": 480,
        "input_working_set_tokens": 800,
        "tool_manifest_id": "manifest-1",
        "tool_count": 4,
        "messages": [
            { "message_uid": "msg/system/run-1", "role": "system", "hash": "aaaaaaaaaaaaaaaa" },
            { "message_uid": "msg/task/turn-1", "role": "system", "hash": "bbbbbbbbbbbbbbbb" },
            { "message_uid": "msg/user/turn-1", "role": "user", "hash": "cccccccccccccccc" },
        ],
        "messages_hash": "dddddddddddddddd",
        "required_capabilities": ["streaming"],
    });
    if let Some(notice) = notice {
        payload["context_notice"] = notice;
    }
    stored(seq, EventType::MODEL_REQUEST_PREPARED, payload)
}

/// The full tool definitions, as the loop records them once per turn.
fn frozen(seq: u64, manifest: &str) -> StoredEvent {
    stored(
        seq,
        EventType::MODEL_TOOLS_FROZEN,
        serde_json::json!({
            "manifest_id": manifest,
            "created_at": "2026-10-18T12:00:00Z",
            "entries_hash": "eeeeeeeeeeeeeeee",
            "tool_count": 2,
            "entries": [
                {
                    "tool_id": "fs.glob",
                    "registration_version": "v1",
                    "source": "builtin",
                    "description": "Find files by pattern.",
                    "input_schema": { "type": "object" },
                    "schema_hash": "ffffffffffffffff",
                },
                {
                    "tool_id": "fs.read",
                    "registration_version": "v1",
                    "source": "builtin",
                    "description": "Read a file.",
                    "input_schema": { "type": "object" },
                    "schema_hash": "1111111111111111",
                },
            ],
        }),
    )
}

fn completed(seq: u64, attempt: &str) -> StoredEvent {
    stored(
        seq,
        EventType::MODEL_RESPONSE_COMPLETED,
        serde_json::json!({
            "attempt_id": attempt,
            "response": {
                "usage": {
                    "input_tokens": 512, "output_tokens": 12, "total_tokens": 524,
                    "cache_hit_tokens": 448, "cache_miss_tokens": 64, "reasoning_tokens": 3,
                },
            },
        }),
    )
}

fn report() -> ContextUsageReport {
    let categories = ContextCategoryKind::ALL
        .iter()
        .map(|kind| ContextCategoryUsage {
            kind: *kind,
            messages: usize::from(*kind == ContextCategoryKind::Framing) * 3,
            blocks: 0,
            estimated_tokens: 0,
        })
        .collect();
    ContextUsageReport {
        message_count: 3,
        dropped_messages: 2,
        estimated_tokens: 500,
        context_window_tokens: Some(1000),
        input_working_set_tokens: Some(800),
        completion_cap_tokens: Some(200),
        compaction_trigger_tokens: Some(640),
        retain_recent_tokens: Some(400),
        budget_sources: std::collections::BTreeMap::from([(
            String::from("input_working_set_tokens"),
            String::from("preset:balanced"),
        )]),
        budget_tokens: Some(800),
        remaining_tokens: Some(300),
        percent_used: Some(62),
        categories,
        largest_messages: vec![LargestMessage {
            message_uid: String::from("msg/user/1"),
            role: MessageRole::User,
            kinds: vec![ContextCategoryKind::UserText],
            estimated_tokens: 300,
            preview: String::from("a long user message"),
        }],
    }
}

fn session() -> SessionId {
    SessionId::from_string("default")
}

fn text_of(outcome: agent_cli::CliOutcome) -> String {
    match outcome.output {
        CliOutput::Text(text) => text,
        CliOutput::Json(doc) => panic!("expected text, got {doc}"),
    }
}

fn json_of(outcome: agent_cli::CliOutcome) -> serde_json::Value {
    match outcome.output {
        CliOutput::Json(doc) => serde_json::from_str(doc.as_str()).expect("one JSON object"),
        CliOutput::Text(text) => panic!("expected JSON, got {text}"),
    }
}

#[test]
fn the_text_shows_the_budget_every_kind_and_the_largest_message() {
    let events = vec![
        prepared(4, "attempt-1", Some(serde_json::json!({ "kind": "trimmed" }))),
        completed(5, "attempt-1"),
    ];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(9), &events, false).unwrap();
    assert_eq!(outcome.verdict, agent_cli::Verdict::Answered);
    let text = text_of(outcome);

    assert!(text.contains("messages: 3 in context; 2 dropped by the window"), "{text}");
    assert!(
        text.contains("budget: 800-token input working set; about 500 used (62%), 300 left"),
        "{text}"
    );
    assert!(
        text.contains(
            "context window 1000, completion cap 200, compaction trigger 640, \
             retained recent 400"
        ),
        "every resolved number is shown: {text}"
    );
    assert!(
        text.contains("sources input_working_set_tokens=preset:balanced"),
        "the source of each field is shown: {text}"
    );
    for kind in ContextCategoryKind::ALL {
        assert!(text.contains(kind.as_str()), "missing kind {}: {text}", kind.as_str());
    }
    assert!(text.contains("a long user message"), "{text}");
    // The last request is read back, including the notice the log records.
    assert!(text.contains("last request: seq 4"), "{text}");
    assert!(text.contains("context revision: 7"), "{text}");
    assert!(text.contains("tools: 4 tool(s), manifest manifest-1"), "{text}");
    assert!(text.contains("required capabilities: streaming"), "{text}");
    assert!(text.contains("notice: trimmed"), "{text}");
    assert!(
        text.contains(
            "provider usage for that attempt: input 512, output 12, total 524; \
             cache hit 448 / miss 64 (87% hit)"
        ),
        "{text}"
    );
    assert!(text.contains("reasoning tokens: 3"), "{text}");
    assert!(
        text.contains(
            "estimated request: 480 tok (messages 420, tools 60); \
             provider input 512; difference 6%"
        ),
        "{text}"
    );
}

#[test]
fn a_context_without_a_window_says_so_instead_of_inventing_one() {
    let mut report = report();
    report.context_window_tokens = None;
    report.input_working_set_tokens = None;
    report.completion_cap_tokens = None;
    report.compaction_trigger_tokens = None;
    report.retain_recent_tokens = None;
    report.budget_sources.clear();
    report.budget_tokens = None;
    report.remaining_tokens = None;
    report.percent_used = None;

    let outcome = run_context_inspect(&report, &session(), EventSeq::ZERO, &[], false).unwrap();
    let text = text_of(outcome);

    assert!(
        text.contains("budget: no resolved context budget; history is sent untrimmed"),
        "{text}"
    );
    assert!(
        text.contains("last request: none (no model request in this log)"),
        "an empty log is an answer, not a failure: {text}"
    );
}

#[test]
fn the_json_answer_carries_the_report_and_the_last_request() {
    let events = vec![
        prepared(4, "attempt-1", Some(serde_json::json!({ "kind": "near-limit" }))),
        completed(5, "attempt-1"),
    ];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(9), &events, true).unwrap();
    let value = json_of(outcome);

    assert_eq!(value["session"], "default");
    assert_eq!(value["context_revision"], 9);
    assert_eq!(value["context"]["message_count"], 3);
    assert_eq!(
        value["context"]["categories"].as_array().expect("every kind").len(),
        ContextCategoryKind::ALL.len()
    );
    assert_eq!(value["context"]["largest_messages"][0]["message_uid"], "msg/user/1");
    assert_eq!(value["last_request"]["seq"], 4);
    assert_eq!(value["last_request"]["attempt_id"], "attempt-1");
    assert_eq!(value["last_request"]["tool_count"], 4);
    assert_eq!(value["last_request"]["context_notice"]["kind"], "near-limit");
    assert_eq!(value["last_request"]["layout_version"], 2);
    assert_eq!(value["last_request"]["estimated_request_tokens"], 480);
    assert_eq!(value["last_request"]["provider_usage"]["input_tokens"], 512);
    assert_eq!(value["last_request"]["provider_usage"]["total_tokens"], 524);
    assert_eq!(value["last_request"]["provider_usage"]["cache_hit_tokens"], 448);
    assert_eq!(value["last_request"]["provider_usage"]["cache_miss_tokens"], 64);
    assert_eq!(value["last_request"]["provider_usage"]["reasoning_tokens"], 3);
}

#[test]
fn the_last_compaction_is_read_back_in_text_and_json() {
    let mut compacted = stored(
        3,
        EventType::CONTEXT_COMPACTED,
        serde_json::json!({
            "attempt_id": "attempt-c",
            "summary_message_uid": "msg/summary/attempt-c",
            "summary": "the work so far",
            "summarized": [
                { "message_uid": "msg/user/1", "role": "user", "hash": "aaaa" },
                { "message_uid": "msg/user/2", "role": "user", "hash": "bbbb" },
            ],
            "summarized_hash": "cccc",
            "context_revision": 7,
            "message_count": 2,
            "removed_messages": 2,
            "estimated_tokens": 900,
            "retained_tokens": 100,
            "truncated": false,
            "ledger": {
                "message_uid": "msg/ledger/attempt-c",
                "view_version": "ledger/1",
                "text": "1. fs.read {\"path\":\"a\"} -> ok: text",
                "lines": 1,
                "elided": 0,
                "files": ["a"],
            },
            "usertext": {
                "message_uid": "msg/usertext/attempt-c",
                "view_version": "usertext/1",
                "text": "Recorded user messages: never edit tests/spec.rs",
                "messages": 1,
                "carried": false,
                "elided": 0,
                "considered": [
                    { "message_uid": "msg/user/1", "score": 8, "kept": true },
                ],
            },
            "failures": {
                "message_uid": "msg/failures/attempt-c",
                "view_version": "failures/1",
                "text": "Recorded failures: expected Ok(111), got Ok(74)",
                "entries": 1,
                "carried": false,
                "elided": 0,
            },
        }),
    );
    compacted.event.turn_id = Some(TurnId::new());
    let mut request = prepared(4, "attempt-1", None);
    request.event.payload["context_ledger"] = serde_json::json!({
        "message_uid": "msg/ledger/attempt-c",
        "view_version": "ledger/1",
        "hash": "eeeeeeeeeeeeeeee",
        "lines": 1,
        "elided": 0,
    });
    request.event.payload["context_usertext"] = serde_json::json!({
        "message_uid": "msg/usertext/attempt-c",
        "view_version": "usertext/1",
        "hash": "ffffffffffffffff",
        "messages": 1,
        "elided": 0,
    });
    request.event.payload["context_failures"] = serde_json::json!({
        "message_uid": "msg/failures/attempt-c",
        "view_version": "failures/1",
        "hash": "9999999999999999",
        "entries": 1,
        "elided": 0,
    });
    let events = vec![
        stored(2, EventType::CONTEXT_COMPACTION_STARTED, serde_json::json!({})),
        compacted,
        request,
        completed(5, "attempt-1"),
    ];

    let text = text_of(
        run_context_inspect(&report(), &session(), EventSeq::new(9), &events, false).unwrap(),
    );
    assert!(
        text.contains(
            "last compaction: seq 3, summarized 2 message(s) into msg/summary/attempt-c; \
             removed 2; truncated no; ledger 1 line(s), 0 elided; user record 1 message(s), 0 \
             elided; failures 1 recorded, 0 elided"
        ),
        "{text}"
    );
    // The summary text itself stays in the event, not on the report line.
    assert!(!text.contains("the work so far"), "{text}");

    let value = json_of(
        run_context_inspect(&report(), &session(), EventSeq::new(9), &events, true).unwrap(),
    );
    assert_eq!(value["last_compaction"]["seq"], 3);
    assert_eq!(value["last_compaction"]["outcome"], "compacted");
    assert_eq!(value["last_compaction"]["message_count"], 2);
    assert_eq!(value["last_compaction"]["removed_messages"], 2);
    assert_eq!(value["last_compaction"]["summary_message_uid"], "msg/summary/attempt-c");
    assert_eq!(value["last_compaction"]["ledger"]["view_version"], "ledger/1");
    assert_eq!(value["last_compaction"]["usertext"]["view_version"], "usertext/1");
    assert_eq!(value["last_compaction"]["usertext"]["considered"][0]["score"], 8);
    assert_eq!(value["last_compaction"]["failures"]["view_version"], "failures/1");
    assert_eq!(value["last_compaction"]["failures"]["entries"], 1);
    assert_eq!(value["last_request"]["context_ledger"]["message_uid"], "msg/ledger/attempt-c");
    assert_eq!(value["last_request"]["context_usertext"]["message_uid"], "msg/usertext/attempt-c");
    assert_eq!(value["last_request"]["context_failures"]["message_uid"], "msg/failures/attempt-c");
}

#[test]
fn a_failed_compaction_is_reported_and_a_lone_start_is_not_an_outcome() {
    let events = vec![
        stored(2, EventType::CONTEXT_COMPACTION_STARTED, serde_json::json!({})),
        stored(
            3,
            EventType::CONTEXT_COMPACTION_FAILED,
            serde_json::json!({
                "attempt_id": "attempt-c",
                "error_kind": "http",
                "message": "model provider answered 503 busy",
            }),
        ),
        prepared(4, "attempt-1", None),
    ];

    let value = json_of(
        run_context_inspect(&report(), &session(), EventSeq::new(9), &events, true).unwrap(),
    );
    assert_eq!(value["last_compaction"]["outcome"], "failed");
    assert_eq!(value["last_compaction"]["error_kind"], "http");

    let text = text_of(
        run_context_inspect(&report(), &session(), EventSeq::new(9), &events, false).unwrap(),
    );
    assert!(
        text.contains("last compaction: seq 3, failed (http): model provider answered 503 busy"),
        "{text}"
    );

    // A start whose outcome never reached the log is not a compaction.
    let cut_short = vec![stored(2, EventType::CONTEXT_COMPACTION_STARTED, serde_json::json!({}))];
    let value = json_of(
        run_context_inspect(&report(), &session(), EventSeq::ZERO, &cut_short, true).unwrap(),
    );
    assert!(value["last_compaction"].is_null(), "{value}");
}

#[test]
fn the_last_request_reports_the_per_turn_layers_it_carried() {
    let mut event = prepared(4, "attempt-1", None);
    event.event.payload["context_layers"] = serde_json::json!({
        "state": {
            "message_uid": "msg/state/turn-1",
            "text": "State at 2026-10-18T14:05:00Z.\nQueued input(s): 1.\nPrevious turn: none.",
        },
        "task": {
            "message_uid": "msg/task/turn-1",
            "text": "Task for this turn: \"fix the parser\"\nInput: user turn. This run allows at most 16 model steps.",
        },
    });
    let events = vec![event];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &events, false).unwrap();
    let text = text_of(outcome);

    // The text output is one line per layer, collapsed; the JSON below carries
    // the recorded text whole.
    assert!(
        text.contains(
            "state layer: State at 2026-10-18T14:05:00Z. Queued input(s): 1. Previous turn: none."
        ),
        "{text}"
    );
    assert!(
        text.contains("task layer: Task for this turn: \"fix the parser\" Input: user turn."),
        "{text}"
    );

    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &events, true).unwrap();
    let value = json_of(outcome);
    assert_eq!(value["last_request"]["context_layers"]["state"]["message_uid"], "msg/state/turn-1");
    assert_eq!(
        value["last_request"]["context_layers"]["task"]["text"],
        "Task for this turn: \"fix the parser\"\nInput: user turn. This run allows at most 16 model steps."
    );
}

#[test]
fn the_last_request_reports_the_capability_layer_it_carried() {
    let mut event = prepared(4, "attempt-1", None);
    event.event.payload["context_capability"] = serde_json::json!({
        "message_uid": "msg/capability/run-1",
        "text": "Capabilities in this run:\n\
                 - sandbox: local (commands run on the host, confined to the workspace)\n\
                 - network for sandboxed commands: not bounded by this backend (a command \
                   reaches the network as your user does)\n\
                 - 2 tools are available; full arguments are in the tool definitions:\n\
                 \u{20}\u{20}fs.glob — Find files by pattern",
    });
    event.event.payload["context_layers"] = serde_json::json!({
        "state": { "message_uid": "msg/state/turn-1", "text": "State at 2026-10-18T14:05:00Z." },
    });
    let events = vec![event];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &events, false).unwrap();
    let text = text_of(outcome);

    assert!(
        text.contains("capability layer: Capabilities in this run: - sandbox: local"),
        "{text}"
    );
    // It belongs to the run, so it reads before the turn's own layers.
    let capability = text.find("capability layer:").expect("a capability line");
    let state = text.find("state layer:").expect("a state line");
    assert!(capability < state, "{text}");

    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &events, true).unwrap();
    let value = json_of(outcome);
    assert_eq!(value["last_request"]["context_capability"]["message_uid"], "msg/capability/run-1");
    assert!(
        value["last_request"]["context_capability"]["text"]
            .as_str()
            .expect("the full text")
            .contains("- network for sandboxed commands: not bounded by this backend"),
        "{value}"
    );
}

#[test]
fn the_last_request_names_the_exact_messages_and_the_definitions_it_was_sent_with() {
    let events = vec![frozen(3, "manifest-1"), prepared(4, "attempt-1", None)];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &events, false).unwrap();
    let text = text_of(outcome);

    assert!(
        text.contains("tool definitions: 2 frozen at seq 3 (entries hash eeeeeeeeeeeeeeee)"),
        "{text}"
    );
    assert!(text.contains("message refs: 3 sent, hash dddddddddddddddd"), "{text}");

    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &events, true).unwrap();
    let value = json_of(outcome);
    let recorded = &value["last_request"];
    assert_eq!(recorded["messages"].as_array().expect("the ordered refs").len(), 3);
    assert_eq!(recorded["messages"][2]["message_uid"], "msg/user/turn-1");
    assert_eq!(recorded["messages_hash"], "dddddddddddddddd");
    assert_eq!(recorded["tool_definitions"]["seq"], 3);
    assert_eq!(recorded["tool_definitions"]["manifest_id"], "manifest-1");
    assert_eq!(recorded["tool_definitions"]["entries"][0]["description"], "Find files by pattern.");
    assert!(
        recorded["tool_definitions"]["entries"][0]["input_schema"].is_object(),
        "the schema itself is the record, not a hash of it: {recorded}"
    );
}

#[test]
fn a_freeze_from_another_turn_is_not_attached_to_this_request() {
    let mut freezing = frozen(3, "manifest-old");
    freezing.event.turn_id = Some(TurnId::from_string("turn-old"));
    let mut request = prepared(4, "attempt-1", None);
    request.event.turn_id = Some(TurnId::from_string("turn-new"));

    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &[freezing, request], true)
            .unwrap();
    let value = json_of(outcome);

    assert!(
        value["last_request"]["tool_definitions"].is_null(),
        "an older turn's definitions are not this request's: {value}"
    );
}

#[test]
fn a_request_before_any_freeze_has_no_definitions() {
    let events = vec![prepared(1, "attempt-1", None)];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(1), &events, true).unwrap();
    let value = json_of(outcome);

    assert!(value["last_request"]["tool_definitions"].is_null(), "{value}");
    assert!(value["last_request"]["messages"][0]["hash"].is_string(), "{value}");
}

#[test]
fn a_request_without_layers_shows_none() {
    let events = vec![prepared(1, "attempt-1", None)];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(1), &events, false).unwrap();
    let text = text_of(outcome);

    assert!(!text.contains("state layer:"), "{text}");
    assert!(!text.contains("task layer:"), "{text}");
    assert!(!text.contains("capability layer:"), "{text}");
}

#[test]
fn a_response_is_attached_by_attempt_not_by_position() {
    // A retry: the older attempt answered, the newer one did not. The newer
    // request must not inherit the older attempt's usage.
    let events = vec![
        prepared(1, "attempt-1", None),
        completed(2, "attempt-1"),
        prepared(3, "attempt-2", None),
        completed(4, "attempt-1"),
    ];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(4), &events, true).unwrap();
    let value = json_of(outcome);

    assert_eq!(value["last_request"]["attempt_id"], "attempt-2");
    assert_eq!(
        value["last_request"]["provider_usage"],
        serde_json::Value::Null,
        "a different attempt's usage is not this request's usage"
    );
}

#[test]
fn a_provider_that_reports_no_counts_is_treated_as_unreported_not_as_zero() {
    let events = vec![
        prepared(1, "attempt-1", None),
        stored(
            2,
            EventType::MODEL_RESPONSE_COMPLETED,
            serde_json::json!({
                "attempt_id": "attempt-1",
                "response": { "usage": {
                    "input_tokens": null, "output_tokens": null, "total_tokens": null,
                }},
            }),
        ),
    ];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(2), &events, false).unwrap();
    let text = text_of(outcome);

    assert!(!text.contains("provider usage"), "{text}");
}

#[test]
fn a_request_that_never_got_a_response_still_renders() {
    let events = vec![prepared(1, "attempt-1", None)];
    let outcome =
        run_context_inspect(&report(), &session(), EventSeq::new(1), &events, false).unwrap();
    let text = text_of(outcome);

    assert!(text.contains("last request: seq 1"), "{text}");
    assert!(!text.contains("provider usage"), "{text}");
    assert!(!text.contains("notice:"), "no notice was placed: {text}");
}
