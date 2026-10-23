//! Two writers over one log, sharing no state but the file and its lock.
//!
//! The failure this file was written for is the multi-process one: every
//! `agent` command is its own process opening the same session, so a reader
//! must see what another writer appended and the two must not settle on the
//! same sequence. **What is exercised here is two `JsonlEventStore` instances
//! in one process**, each with its own lease table. They share exactly what
//! separate processes would share — the log file and the OS advisory lock on
//! `events.lock` — which is the mechanism under test, but no child process is
//! started (ST-9). The name is kept as the record of the failure class rather
//! than as a claim about how it is driven.
//!
//! This file used to be `tmp_cross_process_probe.rs`, and it used to print the
//! opposite of the above: a reader that never saw the other's writes, and a
//! writer that reused a sequence already taken, leaving a log that no longer
//! replays. It asserted nothing, so it passed while saying so, and the gap it
//! described sat in the tree unfixed. These are those findings as assertions.

use std::time::Duration;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{EventId, EventSeq, SchemaVersion, SessionId};
use agent_store_jsonl::JsonlEventStore;
use chrono::Utc;

/// How long an explicit `lease()` here waits for a lease it will never get.
///
/// Only the explicit calls need it: `append` carries its own short wait, which
/// these tests leave alone. The tests hold the lease for the whole of the
/// competing call, so the refusal is certain whatever the wait; the wait only
/// decides how long the test spends proving it.
const TEST_LEASE_WAIT: Duration = Duration::from_millis(50);

fn draft(session_id: &SessionId, event_type: &str) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::from_string("0.1.0"),
        session_id: session_id.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor: Actor::User,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::json!({}),
        metadata: serde_json::json!({}),
        ignorable: false,
    }
}

/// Both writers every test needs: one that got there first, one that opened
/// afterwards knowing nothing of what the first had done.
fn two_writers(root: &std::path::Path) -> (JsonlEventStore, JsonlEventStore) {
    let open = || JsonlEventStore::new(root.to_path_buf()).with_lease_wait(TEST_LEASE_WAIT);
    (open(), open())
}

#[tokio::test]
async fn a_writer_sees_what_the_other_one_appended() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("see-each-other");
    let (a, b) = two_writers(dir.path());

    a.append(&sid, EventSeq::ZERO, vec![draft(&sid, "turn/started")]).await.unwrap();

    // B opened before A wrote, so this is a replay of what is on disk.
    assert_eq!(b.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().len(), 1);

    // Now the other direction, which is the one that used to fail: A has been
    // open the whole time and must still pick up what B writes.
    b.append(&sid, EventSeq::new(1), vec![draft(&sid, "inbox/appended")]).await.unwrap();
    let seen_by_a = a.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    assert_eq!(seen_by_a.len(), 2, "a long-lived reader must pick up another writer's append");
    assert_eq!(seen_by_a[1].seq, EventSeq::new(1));
}

#[tokio::test]
async fn a_writer_that_lost_the_race_conflicts_instead_of_reusing_a_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("no-reuse");
    let (a, b) = two_writers(dir.path());

    a.append(&sid, EventSeq::ZERO, vec![draft(&sid, "turn/started")]).await.unwrap();
    b.append(&sid, EventSeq::new(1), vec![draft(&sid, "inbox/appended")]).await.unwrap();

    // A still believes seq 1 is free. It is not, and A must be told so rather
    // than write a second line claiming seq 1 — which is what it used to do, and
    // which left a log that would not replay.
    let stale = a.append(&sid, EventSeq::new(1), vec![draft(&sid, "turn/completed")]).await;
    match stale {
        Err(EventStoreError::SequenceConflict { expected, actual }) => {
            assert_eq!(expected, EventSeq::new(1));
            assert_eq!(actual, EventSeq::new(2), "the conflict reports the log's real next seq");
        }
        other => panic!("expected a sequence conflict, got {other:?}"),
    }

    // The log is still the log: contiguous, and it replays.
    let reopened = JsonlEventStore::new(dir.path().to_path_buf());
    let events = reopened.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
    assert_eq!(events.len(), 2);
    for (idx, event) in events.iter().enumerate() {
        assert_eq!(event.seq.as_u64(), idx as u64, "seq must be contiguous and hole-free");
    }

    // And having been told, A can re-derive and write for real.
    a.append(&sid, EventSeq::new(2), vec![draft(&sid, "turn/completed")]).await.unwrap();
    assert_eq!(reopened.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_lease_keeps_the_other_writer_out_until_it_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("exclusive");
    let (a, b) = two_writers(dir.path());

    a.append(&sid, EventSeq::ZERO, vec![draft(&sid, "turn/started")]).await.unwrap();

    // A claims the session for a critical section wider than a single write —
    // the shape of resuming a tool call, where the effect happens before the
    // append that records it.
    let lease = a.lease(&sid).await.unwrap();
    assert_eq!(lease.session_id(), &sid);

    let blocked = b.append(&sid, EventSeq::new(1), vec![draft(&sid, "inbox/appended")]).await;
    assert!(
        matches!(blocked, Err(EventStoreError::ConcurrentWriter(_))),
        "a leased session must refuse another writer, got {blocked:?}"
    );

    // The refusal wrote nothing.
    assert_eq!(a.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().len(), 1);

    // Dropping the lease is the release; the log takes the write again.
    drop(lease);
    b.append(&sid, EventSeq::new(1), vec![draft(&sid, "inbox/appended")]).await.unwrap();
    assert_eq!(a.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_wait_long_enough_outlasts_a_holder_that_lets_go() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("brief-hold");
    let (a, b) = two_writers(dir.path());
    // B is rebuilt with a wait that outlasts the hold below. The two ways a
    // contended lease can go are refusal and acquisition, and the tests so far
    // only ever showed the first; this is the other one, and it is the whole
    // reason the wait is a wait rather than an immediate no.
    let b = b.with_lease_wait(Duration::from_secs(5));

    let lease = a.lease(&sid).await.unwrap();

    // The release comes from another OS thread on purpose: `acquire` waits by
    // blocking its thread, so a task on this runtime would never get to run
    // while B is inside the wait.
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        drop(lease);
    });

    let taken = b.lease(&sid).await.expect("a wait that outlasts the holder takes the lease");
    holder.join().unwrap();

    // And the lease B took is a real one: a third writer is shut out until B
    // lets go, so this is not an acquire that quietly did nothing.
    let c = JsonlEventStore::new(dir.path().to_path_buf()).with_lease_wait(TEST_LEASE_WAIT);
    assert!(
        matches!(c.lease(&sid).await, Err(EventStoreError::ConcurrentWriter(_))),
        "the lease B took has to exclude the next writer"
    );
    drop(taken);
}

#[tokio::test]
async fn an_explicit_lease_waits_past_the_point_where_an_append_gives_up() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("two-waits");
    let holder_store = JsonlEventStore::new(dir.path().to_path_buf());
    let appender = JsonlEventStore::new(dir.path().to_path_buf());
    // No `with_lease_wait`: waiting as long as it takes is the default for an
    // explicit lease, and that default is the thing under test.
    let waiter = JsonlEventStore::new(dir.path().to_path_buf());

    let lease = holder_store.lease(&sid).await.unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        drop(lease);
    });

    // An append gives up long before this holder is done. That is not impatience
    // for its own sake: the append carries a sequence it read before the wait, so
    // an append that waited the hold out would only fail with a sequence
    // conflict instead, further from the truth.
    let refused = appender.append(&sid, EventSeq::ZERO, vec![draft(&sid, "turn/started")]).await;
    assert!(
        matches!(refused, Err(EventStoreError::ConcurrentWriter(_))),
        "an append must give up rather than wait out a long hold, got {refused:?}"
    );

    // The same holder at the same moment, and an explicit lease does the
    // opposite: it waits, and takes the lease when the holder finally lets go.
    // The two callers were waiting for different things, so they wait for
    // different lengths — which is the whole reason the wait is not one number.
    let taken = waiter.lease(&sid).await.expect("the explicit lease waits the hold out");
    holder.join().unwrap();
    drop(taken);
}

#[tokio::test]
async fn a_caller_can_append_while_holding_its_own_lease() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::from_string("reentrant");
    let (a, _b) = two_writers(dir.path());

    // The critical section a resume needs: read, decide, act, then append, all
    // with the lease held. If `append` tried to take the lock a second time this
    // would deadlock rather than fail.
    let lease = a.lease(&sid).await.unwrap();
    assert!(a.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().is_empty());

    a.append(&sid, EventSeq::ZERO, vec![draft(&sid, "turn/started")]).await.unwrap();
    a.append(&sid, EventSeq::new(1), vec![draft(&sid, "turn/resumed")]).await.unwrap();
    assert_eq!(a.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().len(), 2);
    drop(lease);

    // Releasing a nested level must not release the whole lease: while the outer
    // one is held, another writer is still shut out.
    let outer = a.lease(&sid).await.unwrap();
    let inner = a.lease(&sid).await.unwrap();
    drop(inner);
    let other = JsonlEventStore::new(dir.path().to_path_buf()).with_lease_wait(TEST_LEASE_WAIT);
    assert!(
        matches!(
            other.append(&sid, EventSeq::new(2), vec![draft(&sid, "inbox/appended")]).await,
            Err(EventStoreError::ConcurrentWriter(_))
        ),
        "a nested release must not drop the outer lease"
    );
    drop(outer);
}
