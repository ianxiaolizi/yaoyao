//! Acceptance tests for the projection engine: fold, checkpoint, and recovery.

use std::sync::Arc;

use agent_ports::event_store::EventStore;
use agent_projection::{
    CheckpointVerdict, Projected, Projection, ProjectionCheckpoint, ProjectionError,
    SessionProjection, SessionState,
};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::*;
use agent_testkit::FakeEventStore;
use chrono::Utc;

fn draft(session_id: &str, event_type: &str, ignorable: bool) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: SessionId::from_string(session_id),
        run_id: Some(RunId::new()),
        turn_id: Some(TurnId::new()),
        step_id: Some(StepId::new()),
        actor: Actor::System,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable,
    }
}

async fn append(store: &FakeEventStore, sid: &SessionId, expected: u64, drafts: Vec<EventDraft>) {
    store.append(sid, EventSeq::new(expected), drafts).await.unwrap();
}

#[tokio::test]
async fn fold_all_rebuilds_from_empty() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    let projected = projection.build_full(&sid).await.unwrap();
    assert_eq!(projected.state, SessionState::default());
    assert_eq!(projected.as_of_seq, None);
}

#[tokio::test]
async fn fold_all_applies_events_in_seq_order() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    // The drafts name their turns. `draft` mints a fresh id per event, which
    // would make the completion end a turn that was never started — a stale
    // terminal, which the fold no longer counts (see `session.rs`). The shape
    // under test stays the same: two turns started, one of them ended.
    let first = TurnId::new();
    let second = TurnId::new();
    let mut drafts = vec![
        draft("s", EventType::SESSION_STARTED, false),
        draft("s", EventType::TURN_STARTED, false),
        draft("s", EventType::TURN_STARTED, false),
        draft("s", EventType::TURN_COMPLETED, false),
    ];
    drafts[1].turn_id = Some(first);
    drafts[2].turn_id = Some(second.clone());
    drafts[3].turn_id = Some(second);
    append(&store, &sid, 0, drafts).await;

    let projected = projection.build_full(&sid).await.unwrap();
    assert_eq!(projected.as_of_seq, Some(EventSeq::new(3)));
    assert!(projected.state.started);
    assert_eq!(projected.state.turns_started, 2);
    assert_eq!(projected.state.turns_terminal, 1);

    let view = projection.view(&projected.state);
    assert!(view.open);
    assert_eq!(view.open_turns, 1);
}

#[tokio::test]
async fn checkpoint_plus_tail_equals_full_replay() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(
        &store,
        &sid,
        0,
        vec![
            draft("s", EventType::SESSION_STARTED, false),
            draft("s", EventType::TURN_STARTED, false),
            draft("s", EventType::TURN_COMPLETED, false),
        ],
    )
    .await;

    let initial = projection.build_full(&sid).await.unwrap();
    assert_eq!(initial.as_of_seq, Some(EventSeq::new(2)));
    projection.checkpoint(&sid, &initial).await.unwrap();

    append(
        &store,
        &sid,
        3,
        vec![draft("s", EventType::TURN_STARTED, false), draft("s", EventType::TURN_FAILED, false)],
    )
    .await;

    let with_checkpoint = projection.build(&sid).await.unwrap();
    let full = projection.build_full(&sid).await.unwrap();
    assert_eq!(with_checkpoint.state, full.state);
    assert_eq!(with_checkpoint.as_of_seq, full.as_of_seq);
    assert_eq!(full.as_of_seq, Some(EventSeq::new(4)));
}

#[tokio::test]
async fn corrupt_checkpoint_rebuilds_from_zero() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;

    let initial = projection.build_full(&sid).await.unwrap();
    projection.checkpoint(&sid, &initial).await.unwrap();

    // Corrupt the checkpoint: garbage bytes.
    let cp_path = dir.path().join("s").join("checkpoints").join("session.json");
    std::fs::write(&cp_path, b"{ not valid json").unwrap();

    let full = projection.build_full(&sid).await.unwrap();
    let rebuilt = projection.build(&sid).await.unwrap();
    assert_eq!(rebuilt.state, full.state);
}

#[tokio::test]
async fn missing_checkpoint_rebuilds_from_zero() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;

    let with_none = projection.build(&sid).await.unwrap();
    let full = projection.build_full(&sid).await.unwrap();
    assert_eq!(with_none.state, full.state);
}

/// The metadata check says the checkpoint was written by *this* projection; it
/// cannot say the log still holds what the checkpoint claims it folded. A
/// checkpoint naming a seq past the end of the log used to be handed back as the
/// answer: `build_from` reads *after* `as_of_seq`, so that read came back empty,
/// the fold loop never ran, and the checkpoint's own `as_of_seq` went out as the
/// derivation's revision — a claim about the log the log does not support.
///
/// The assertion that bites is `as_of_seq`, not `state`: give the checkpoint an
/// honest state and an inflated `as_of_seq` and it looks correct everywhere a
/// spot-check would look, which is why nothing noticed.
#[tokio::test]
async fn a_checkpoint_claiming_a_seq_the_log_does_not_hold_is_not_used() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;
    let honest = projection.build_full(&sid).await.unwrap();
    assert_eq!(honest.as_of_seq, Some(EventSeq::ZERO));

    projection
        .checkpoint(
            &sid,
            &Projected { state: honest.state.clone(), as_of_seq: Some(EventSeq::new(99)) },
        )
        .await
        .unwrap();

    let built = projection.build(&sid).await.unwrap();
    assert_eq!(built.state, honest.state);
    assert_eq!(built.as_of_seq, honest.as_of_seq);
}

/// The other half of the same hole, pinned so it stays visible rather than
/// implied: a checkpoint the log *does* support is used as it stands, and
/// nothing compares its state against what folding the log would produce. A
/// content-wrong checkpoint that passes the metadata check and names a seq the
/// log holds is therefore still served, silently.
///
/// It keeps passing now that the divergence detector exists, because that
/// detector is a separate entry point — `build` stays a cache lookup, or the
/// cache would buy nothing. `check_checkpoint` is the consumer that reports on
/// the state `build` serves; the test below it pins both halves together.
#[tokio::test]
async fn a_grounded_checkpoint_is_used_even_if_its_state_is_wrong() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;
    let honest = projection.build_full(&sid).await.unwrap();

    let wrong = SessionState { turns_terminal: 99, ..honest.state.clone() };
    projection
        .checkpoint(&sid, &Projected { state: wrong.clone(), as_of_seq: honest.as_of_seq })
        .await
        .unwrap();

    let built = projection.build(&sid).await.unwrap();
    assert_ne!(wrong, honest.state, "the fixture has to actually differ to say anything");
    assert_eq!(built.state, wrong);

    // ...and the detector reports exactly that, on the same fixture.
    let verdict = projection.check_checkpoint(&sid).await.unwrap();
    assert_eq!(
        verdict,
        CheckpointVerdict::Diverges { as_of_seq: Some(EventSeq::ZERO) },
        "a checkpoint the log does not support must not pass as a match"
    );
}

/// A checkpoint that is merely *behind* the tail is still correct. The
/// comparison therefore folds only through the checkpoint's own `as_of_seq` —
/// folding the whole log would report every cache as diverging the moment one
/// new event lands, which would make the check useless in practice.
#[tokio::test]
async fn a_checkpoint_behind_the_tail_still_matches() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;
    let at_zero = projection.build_full(&sid).await.unwrap();
    projection.checkpoint(&sid, &at_zero).await.unwrap();

    append(&store, &sid, 1, vec![draft("s", EventType::TURN_STARTED, false)]).await;

    let verdict = projection.check_checkpoint(&sid).await.unwrap();
    assert_eq!(verdict, CheckpointVerdict::Matches { as_of_seq: Some(EventSeq::ZERO) });
}

#[tokio::test]
async fn an_absent_checkpoint_is_a_miss_not_a_defect() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;

    assert_eq!(projection.check_checkpoint(&sid).await.unwrap(), CheckpointVerdict::Absent);
}

#[tokio::test]
async fn an_unreadable_checkpoint_is_unusable() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;
    let cp_dir = dir.path().join("s").join("checkpoints");
    std::fs::create_dir_all(&cp_dir).unwrap();
    std::fs::write(cp_dir.join("session.json"), b"{ not valid json").unwrap();

    assert_eq!(projection.check_checkpoint(&sid).await.unwrap(), CheckpointVerdict::Unusable);
}

/// A file that is a valid checkpoint for a *different* projection is not this
/// projection's cache and cannot be judged as one.
#[tokio::test]
async fn another_projections_checkpoint_is_unusable() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;
    let cp_dir = dir.path().join("s").join("checkpoints");
    std::fs::create_dir_all(&cp_dir).unwrap();
    let foreign = ProjectionCheckpoint {
        session_id: sid.clone(),
        projection_key: "model-context".to_string(),
        as_of_seq: Some(EventSeq::ZERO),
        projection_schema_version: "session/0.1.0".to_string(),
        state_version: 3,
        created_at: Utc::now(),
        state: SessionState::default(),
    };
    std::fs::write(cp_dir.join("session.json"), serde_json::to_vec(&foreign).unwrap()).unwrap();

    assert_eq!(projection.check_checkpoint(&sid).await.unwrap(), CheckpointVerdict::Unusable);
}

#[tokio::test]
async fn a_checkpoint_naming_a_seq_the_log_does_not_hold_is_ungrounded() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;
    let honest = projection.build_full(&sid).await.unwrap();
    projection
        .checkpoint(
            &sid,
            &Projected { state: honest.state.clone(), as_of_seq: Some(EventSeq::new(99)) },
        )
        .await
        .unwrap();

    assert_eq!(
        projection.check_checkpoint(&sid).await.unwrap(),
        CheckpointVerdict::Ungrounded { claimed: EventSeq::new(99) }
    );
}

#[tokio::test]
async fn stale_checkpoint_version_rebuilds() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(&store, &sid, 0, vec![draft("s", EventType::SESSION_STARTED, false)]).await;

    // A checkpoint whose state_version no longer matches the definition.
    let cp_dir = dir.path().join("s").join("checkpoints");
    std::fs::create_dir_all(&cp_dir).unwrap();
    let stale = ProjectionCheckpoint {
        session_id: sid.clone(),
        projection_key: "session".to_string(),
        as_of_seq: Some(EventSeq::new(0)),
        projection_schema_version: "session/0.1.0".to_string(),
        state_version: 999,
        created_at: Utc::now(),
        state: SessionState {
            started: true,
            turns_started: 99,
            turns_terminal: 99,
            ..Default::default()
        },
    };
    std::fs::write(cp_dir.join("session.json"), serde_json::to_vec(&stale).unwrap()).unwrap();

    let full = projection.build_full(&sid).await.unwrap();
    let rebuilt = projection.build(&sid).await.unwrap();
    assert_eq!(rebuilt.state, full.state);
}

#[tokio::test]
async fn unknown_ignorable_event_is_skipped() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(
        &store,
        &sid,
        0,
        vec![draft("s", EventType::SESSION_STARTED, false), draft("s", "plugin/custom", true)],
    )
    .await;

    let projected = projection.build_full(&sid).await.unwrap();
    assert!(projected.state.started);
    assert_eq!(projected.as_of_seq, Some(EventSeq::new(1)));
}

#[tokio::test]
async fn unknown_required_event_fails_closed() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());
    let sid = SessionId::from_string("s");

    append(
        &store,
        &sid,
        0,
        vec![draft("s", EventType::SESSION_STARTED, false), draft("s", "plugin/custom", false)],
    )
    .await;

    let err = projection.build_full(&sid).await.unwrap_err();
    match err {
        ProjectionError::UnknownRequiredEvent { event_type, seq } => {
            assert_eq!(event_type.as_str(), "plugin/custom");
            assert_eq!(seq, EventSeq::new(1));
        }
        other => panic!("expected UnknownRequiredEvent, got {other:?}"),
    }
}

#[tokio::test]
async fn rejects_unsafe_session_id_on_checkpoint() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());

    // `../escape` is the spelling the character class can see: a `/` is not in
    // the allowed set. `..` is the one it cannot — every character of it is an
    // ordinary character — and it is the same escape with fewer keystrokes,
    // which is why this test used to pass while the check was open (YA-08).
    for id in ["../escape", "..", "."] {
        let sid = SessionId::from_string(id);
        let projected = Projected { state: SessionState::default(), as_of_seq: None };
        let err = projection.checkpoint(&sid, &projected).await.unwrap_err();
        match err {
            ProjectionError::InvalidSessionId(s) => assert_eq!(s, sid),
            other => panic!("expected InvalidSessionId for {id:?}, got {other:?}"),
        }
    }

    // And nothing was written where those names would have put it: `checkpoint`
    // creates its directory before writing, so an accepted `..` leaves a
    // `checkpoints` directory beside the root rather than under it.
    assert!(!dir.path().join("checkpoints").exists(), "a checkpoint landed outside the root");
}

#[cfg(windows)]
#[tokio::test]
async fn rejects_a_session_id_windows_would_rename() {
    let store = Arc::new(FakeEventStore::new());
    let dir = tempfile::tempdir().unwrap();
    let projection =
        Projection::new(SessionProjection::new(), store.clone(), dir.path().to_path_buf());

    // A trailing `.` is dropped, so `foo.` and `foo` are one directory and two
    // session ids would share one checkpoint; a reserved device name routes to a
    // device. Neither is the directory the name spells, so both are refused
    // (YA-71). Windows-only: on POSIX these are ordinary names.
    for id in ["NUL", "NUL.txt", "CON", "COM1", "foo."] {
        let sid = SessionId::from_string(id);
        let projected = Projected { state: SessionState::default(), as_of_seq: None };
        let err = projection.checkpoint(&sid, &projected).await.unwrap_err();
        match err {
            ProjectionError::InvalidSessionId(s) => assert_eq!(s, sid),
            other => panic!("expected InvalidSessionId for {id:?}, got {other:?}"),
        }
    }

    // And a dot inside a name is still ordinary, so this still writes.
    let ok = SessionId::from_string("2026-10-11.run");
    let projected = Projected { state: SessionState::default(), as_of_seq: None };
    assert!(projection.checkpoint(&ok, &projected).await.is_ok());
}
