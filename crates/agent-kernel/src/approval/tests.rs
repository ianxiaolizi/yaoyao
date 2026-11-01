//! Tests for `approval`.
//!
//! Split out of the module when the approval work pushed that file past
//! its size ceiling; they are tests, and the module they test is unchanged.

use super::*;
use agent_protocol::permission::ActionSpec;
use agent_testkit::FakeEventStore;

fn service() -> (ApprovalService, Arc<FakeEventStore>, SessionId) {
    let store = Arc::new(FakeEventStore::new());
    let service = ApprovalService::new(store.clone());
    let sid = SessionId::from_string("approval-session");
    (service, store, sid)
}

async fn event_types(store: &FakeEventStore, sid: &SessionId) -> Vec<String> {
    store
        .read_stream(sid, EventSeq::ZERO, usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event.event_type.to_string())
        .collect()
}

/// Append a bare lifecycle event, of the shape the Scheduler writes.
///
/// The approval log and the turn log are one stream, which is the whole
/// reason a scope can be resolved against a turn: an event that says a turn
/// ended lands in the same order as the grant it ends.
async fn append(service: &ApprovalService, sid: &SessionId, event_type: &str) {
    let next = service.next_seq(sid).await.unwrap();
    let draft = service.draft(sid, event_type, serde_json::json!({}), Actor::Scheduler);
    service.store.append(sid, next, vec![draft]).await.unwrap();
}

/// Append an event carrying `payload`, for the events a tool run writes.
async fn append_payload(
    service: &ApprovalService,
    sid: &SessionId,
    event_type: &str,
    payload: serde_json::Value,
) {
    let next = service.next_seq(sid).await.unwrap();
    let draft = service.draft(sid, event_type, payload, Actor::Scheduler);
    service.store.append(sid, next, vec![draft]).await.unwrap();
}

/// One tool call's facts, the shape `agent-kernel::action` builds.
fn facts(risk: RiskLevel, path: &str, task_revision: u64) -> ActionFacts {
    facts_with_arguments(risk, path, task_revision, serde_json::json!({ "path": path }))
}

/// The same, with the arguments spelled out — the part of a call a target
/// does not name.
fn facts_with_arguments(
    risk: RiskLevel,
    path: &str,
    task_revision: u64,
    arguments: serde_json::Value,
) -> ActionFacts {
    ActionFacts::from_spec(ActionSpec {
        task_id: "task-1".to_string(),
        task_revision,
        tool_name: "fs.write".to_string(),
        capabilities: vec![Capability::new(Capability::FS_WRITE)],
        risk,
        reversibility: Reversibility::for_risk(risk),
        targets: vec![ActionTarget::WorkspacePath {
            path: path.to_string(),
            writable_root: Some("C:/work".to_string()),
        }],
        arguments,
        summary: format!("write {path}"),
    })
}

/// Ask a question `age` ago, the shape `request` writes.
///
/// `request` stamps `Utc::now()`, and an expiry that takes a day to observe
/// is a test that never runs, so the two tests about the clock write the
/// event themselves.
async fn request_at(
    service: &ApprovalService,
    sid: &SessionId,
    capability: Capability,
    age: Duration,
) -> ApprovalId {
    let req = ApprovalRequest {
        approval_id: ApprovalId::new(),
        session_id: sid.clone(),
        capability,
        tool_name: "shell.run".to_string(),
        args_summary: "ls".to_string(),
        status: ApprovalStatus::Pending,
        created_at: Utc::now() - age,
        action_digest: None,
        risk: None,
        task_id: None,
        task_revision: None,
        reversibility: None,
        targets: Vec::new(),
    };
    let next = service.next_seq(sid).await.unwrap();
    let draft = service.draft(
        sid,
        EventType::APPROVAL_REQUESTED,
        serde_json::to_value(&req).unwrap(),
        Actor::Scheduler,
    );
    service.store.append(sid, next, vec![draft]).await.unwrap();
    req.approval_id
}

/// Grant an approval `age` ago, bypassing `grant`'s pending check and the
/// session-scope gate — both are about *making* a decision, and this is
/// about one that was already made.
async fn grant_at(
    service: &ApprovalService,
    sid: &SessionId,
    approval_id: &ApprovalId,
    scope: ApprovalScope,
    age: Duration,
) {
    let reply = ApprovalReply {
        approval_id: approval_id.clone(),
        decision: ApprovalDecision::Grant,
        scope,
        actor: "user".to_string(),
        created_at: Utc::now() - age,
        action_digest: None,
        risk: None,
        authority: None,
    };
    let next = service.next_seq(sid).await.unwrap();
    let draft = service.draft(
        sid,
        EventType::APPROVAL_GRANTED,
        serde_json::to_value(&reply).unwrap(),
        Actor::User,
    );
    service.store.append(sid, next, vec![draft]).await.unwrap();
}

#[tokio::test]
async fn request_creates_a_pending_approval() {
    let (service, _, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls -la")
        .await
        .unwrap();
    assert_eq!(req.status, ApprovalStatus::Pending);

    let pending = service.pending(&sid).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].approval_id, req.approval_id);
    assert_eq!(pending[0].capability, Capability::new(Capability::PROCESS_SPAWN));
}

#[tokio::test]
async fn grant_sets_status_and_yields_a_lease() {
    let (service, _, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "src/main.rs")
        .await
        .unwrap();

    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &ApprovalAuthority::test())
        .await
        .unwrap();

    assert_eq!(service.status(&sid, &req.approval_id).await.unwrap(), ApprovalStatus::Granted);
    assert!(service.pending(&sid).await.unwrap().is_empty());

    let lease = service.lease_for(&sid, &req.approval_id).await.unwrap().expect("a lease");
    assert_eq!(lease.capability, Capability::new(Capability::FS_WRITE));
    assert_eq!(lease.scope, ApprovalScope::AllowOnce);
}

#[tokio::test]
async fn deny_sets_status_and_yields_no_lease() {
    let (service, _, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "rm -rf /")
        .await
        .unwrap();

    service.deny(&sid, &req.approval_id, "user").await.unwrap();

    assert_eq!(service.status(&sid, &req.approval_id).await.unwrap(), ApprovalStatus::Denied);
    assert!(service.lease_for(&sid, &req.approval_id).await.unwrap().is_none());
}

#[tokio::test]
async fn granting_a_non_pending_approval_fails() {
    let (service, _, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &ApprovalAuthority::test())
        .await
        .unwrap();

    assert!(matches!(
        service
            .grant(
                &sid,
                &req.approval_id,
                ApprovalScope::AllowOnce,
                "user",
                &ApprovalAuthority::test()
            )
            .await,
        Err(ApprovalError::NotPending(_))
    ));
}

#[tokio::test]
async fn session_scope_is_rejected_by_default() {
    let (service, _, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    assert!(matches!(
        service
            .grant(
                &sid,
                &req.approval_id,
                ApprovalScope::AllowForSession,
                "user",
                &ApprovalAuthority::test()
            )
            .await,
        Err(ApprovalError::SessionScopeNotAllowed)
    ));
}

#[tokio::test]
async fn session_scope_allowed_when_enabled() {
    let store = Arc::new(FakeEventStore::new());
    let service = ApprovalService::new(store).with_session_scope(true);
    let sid = SessionId::from_string("approval-session-scope");
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    let reply = service
        .grant(
            &sid,
            &req.approval_id,
            ApprovalScope::AllowForSession,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();
    assert_eq!(reply.scope, ApprovalScope::AllowForSession);
    let lease = service.lease_for(&sid, &req.approval_id).await.unwrap().unwrap();
    assert_eq!(lease.scope, ApprovalScope::AllowForSession);
}

#[tokio::test]
async fn only_a_scoped_grant_leaves_a_standing_lease() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("standing-lease-session");

    let once = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();
    service
        .grant(
            &sid,
            &once.approval_id,
            ApprovalScope::AllowOnce,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();

    // `once` authorised the one call `resume` dispatches itself. Read as a
    // standing permission it would authorise a second call, which is the
    // opposite of what the person answered.
    assert!(service.leases_in_effect(&sid).await.unwrap().is_empty());

    let session = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    service
        .grant(
            &sid,
            &session.approval_id,
            ApprovalScope::AllowForSession,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();

    let leases = service.leases_in_effect(&sid).await.unwrap();
    assert_eq!(leases.len(), 1, "{leases:?}");
    assert_eq!(leases[0].capability, Capability::new(Capability::FS_WRITE));
    assert_eq!(leases[0].scope, ApprovalScope::AllowForSession);
}

#[tokio::test]
async fn a_turn_grant_stops_being_in_effect_when_the_turn_ends() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("turn-grant-session");
    let req = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();
    service
        .grant(
            &sid,
            &req.approval_id,
            ApprovalScope::AllowForTurn,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();

    // Nothing has run and no turn has ended: the grant stands.
    assert_eq!(service.leases_in_effect(&sid).await.unwrap().len(), 1);

    // `turn/yielded` is where the session already was — the held turn — and
    // it ends nothing: `resume` carries that turn forward into a *new* one,
    // and the new one is what this grant is for. Ending the grant here
    // would make `approve ... turn` mean "almost nothing".
    append(&service, &sid, EventType::TURN_YIELDED).await;
    assert_eq!(service.leases_in_effect(&sid).await.unwrap().len(), 1);

    append(&service, &sid, EventType::TURN_COMPLETED).await;
    assert!(service.leases_in_effect(&sid).await.unwrap().is_empty());
    assert_eq!(
        service.status(&sid, &req.approval_id).await.unwrap(),
        ApprovalStatus::Expired,
        "a grant that outlived its turn is no longer in effect, and `Expired` is the word for that"
    );
}

#[tokio::test]
async fn a_question_nobody_answers_expires_on_the_clock() {
    let (service, store, sid) = service();
    let old = request_at(
        &service,
        &sid,
        Capability::new(Capability::PROCESS_SPAWN),
        PENDING_TTL + Duration::minutes(1),
    )
    .await;

    // Nobody wrote an expiry event, and the request has already stopped
    // being one: the fold derives it from the log and the clock, so every
    // reader agrees without having to sweep first.
    assert_eq!(service.status(&sid, &old).await.unwrap(), ApprovalStatus::Expired);
    assert!(
        service.pending(&sid).await.unwrap().is_empty(),
        "a question past its TTL is not still waiting to be answered"
    );

    // Looking at the session left the reason behind, once.
    let written = store
        .read_stream(&sid, EventSeq::ZERO, usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event.event_type.as_str() == EventType::APPROVAL_EXPIRED)
        .collect::<Vec<_>>();
    assert_eq!(written.len(), 1, "{written:?}");
    assert_eq!(
        written[0].event.payload.get("cause").and_then(|v| v.as_str()),
        Some("pending_ttl"),
        "the log says which clock ran out"
    );

    // And a second look adds nothing: the event just read back is what tells
    // the next sweep there is nothing left to say.
    assert!(service.pending(&sid).await.unwrap().is_empty());
    assert_eq!(
        event_types(&store, &sid).await.len(),
        2,
        "a sweep that already ran wrote nothing new"
    );
}

#[tokio::test]
async fn a_session_grant_outlives_nothing_but_its_ttl() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("session-grant-ttl");

    let fresh = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    service
        .grant(
            &sid,
            &fresh.approval_id,
            ApprovalScope::AllowForSession,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();

    // In effect, and it says until when — a session has no end written down
    // anywhere, so the TTL is the only end a session grant can have.
    let lease = service.lease_for(&sid, &fresh.approval_id).await.unwrap().expect("a lease");
    let expires_at = lease.expires_at.expect("a session grant ends on the clock");
    let drift = (expires_at - (Utc::now() + SESSION_GRANT_TTL)).num_seconds().abs();
    assert!(drift < 60, "expiry should be a session TTL out, was {expires_at}");
    assert_eq!(service.leases_in_effect(&sid).await.unwrap().len(), 1);

    // One made more than a TTL ago is not in effect, and says so.
    let old_sid = SessionId::from_string("session-grant-old");
    let old = service
        .request(&old_sid, Capability::new(Capability::NETWORK_REQUEST), "net.get", "u")
        .await
        .unwrap();
    grant_at(
        &service,
        &old_sid,
        &old.approval_id,
        ApprovalScope::AllowForSession,
        SESSION_GRANT_TTL + Duration::minutes(1),
    )
    .await;

    assert!(
        service.leases_in_effect(&old_sid).await.unwrap().is_empty(),
        "a session grant a day old authorises nothing"
    );
    assert_eq!(service.status(&old_sid, &old.approval_id).await.unwrap(), ApprovalStatus::Expired);
}

#[tokio::test]
async fn cancelling_takes_back_a_question_without_answering_it() {
    let (service, store, sid) = service();
    let first = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();
    let second = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    let cancelled = service.cancel_pending(&sid, "user_interrupt").await.unwrap();
    assert_eq!(cancelled.len(), 2);

    // Not an answer: nothing was granted and nothing was denied. The call
    // did not run, which `resume` is what turns into words.
    for id in [&first.approval_id, &second.approval_id] {
        assert_eq!(service.status(&sid, id).await.unwrap(), ApprovalStatus::Cancelled);
    }
    assert!(service.leases_in_effect(&sid).await.unwrap().is_empty());
    assert!(service.pending(&sid).await.unwrap().is_empty());

    let written = store
        .read_stream(&sid, EventSeq::ZERO, usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event.event_type.as_str() == EventType::APPROVAL_CANCELLED)
        .collect::<Vec<_>>();
    assert_eq!(written.len(), 2);
    assert_eq!(
        written[0].event.payload.get("cause").and_then(|v| v.as_str()),
        Some("user_interrupt")
    );
}

#[tokio::test]
async fn lifecycle_events_are_durable_and_ordered() {
    let (service, store, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();
    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &ApprovalAuthority::test())
        .await
        .unwrap();

    assert_eq!(
        event_types(&store, &sid).await,
        vec!["approval/requested".to_string(), "approval/granted".to_string()]
    );
}

#[tokio::test]
async fn a_fresh_service_rebuilds_state_from_the_log() {
    let store = Arc::new(FakeEventStore::new());
    let sid = SessionId::from_string("approval-session-rebuild");
    let service = ApprovalService::new(store.clone());
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &ApprovalAuthority::test())
        .await
        .unwrap();

    let rebuilt = ApprovalService::new(store);
    assert_eq!(rebuilt.status(&sid, &req.approval_id).await.unwrap(), ApprovalStatus::Granted);
}

fn session_scoped(store: Arc<FakeEventStore>) -> ApprovalService {
    ApprovalService::new(store).with_session_scope(true)
}

#[tokio::test]
async fn grant_always_settles_every_pending_request_of_the_same_kind() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("always-session");
    let first = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "a")
        .await
        .unwrap();
    let second = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "b")
        .await
        .unwrap();
    // Same capability, different tool: not the same kind of request.
    let other =
        service.request(&sid, Capability::new(Capability::FS_WRITE), "fs.edit", "c").await.unwrap();

    let replies = service
        .grant_always(&sid, &first.approval_id, "user", &ApprovalAuthority::test())
        .await
        .unwrap();

    let settled: Vec<&str> = replies.iter().map(|r| r.approval_id.as_str()).collect();
    assert_eq!(settled.len(), 2, "{settled:?}");
    assert!(settled.contains(&first.approval_id.as_str()));
    assert!(settled.contains(&second.approval_id.as_str()));
    for reply in &replies {
        assert_eq!(reply.scope, ApprovalScope::AllowForSession);
    }
    assert_eq!(service.status(&sid, &other.approval_id).await.unwrap(), ApprovalStatus::Pending);
}

#[tokio::test]
async fn grant_always_needs_the_session_scope_to_be_permitted() {
    let (service, _, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    assert!(matches!(
        service.grant_always(&sid, &req.approval_id, "user", &ApprovalAuthority::test()).await,
        Err(ApprovalError::SessionScopeNotAllowed)
    ));
    // The refusal settled nothing: the request is still a question.
    assert_eq!(service.status(&sid, &req.approval_id).await.unwrap(), ApprovalStatus::Pending);
}

#[tokio::test]
async fn grant_always_refuses_a_request_that_is_no_longer_pending() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("always-settled");
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &ApprovalAuthority::test())
        .await
        .unwrap();
    let other = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "y")
        .await
        .unwrap();

    assert!(matches!(
        service.grant_always(&sid, &req.approval_id, "user", &ApprovalAuthority::test()).await,
        Err(ApprovalError::NotPending(_))
    ));
    // A cascade that could not name its subject settles nothing else.
    assert_eq!(service.status(&sid, &other.approval_id).await.unwrap(), ApprovalStatus::Pending);
}

#[tokio::test]
async fn deny_cascade_denies_every_pending_request() {
    let (service, _, sid) = service();
    let first = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();
    let second = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    let third = service
        .request(&sid, Capability::new(Capability::NETWORK_REQUEST), "net.get", "u")
        .await
        .unwrap();

    let replies = service.deny_cascade(&sid, &first.approval_id, "user").await.unwrap();

    assert_eq!(replies.len(), 3);
    assert!(replies.iter().all(|r| r.decision == ApprovalDecision::Deny));
    assert!(service.pending(&sid).await.unwrap().is_empty());
    for id in [&second.approval_id, &third.approval_id] {
        assert_eq!(service.status(&sid, id).await.unwrap(), ApprovalStatus::Denied);
    }
}

#[tokio::test]
async fn deny_cascade_leaves_an_already_settled_request_alone() {
    let (service, _, sid) = service();
    let settled = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    service
        .grant(
            &sid,
            &settled.approval_id,
            ApprovalScope::AllowOnce,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();
    let pending = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .unwrap();

    assert!(matches!(
        service.deny_cascade(&sid, &settled.approval_id, "user").await,
        Err(ApprovalError::NotPending(_))
    ));
    // The earlier grant stands; the still-open question is untouched.
    assert_eq!(service.status(&sid, &settled.approval_id).await.unwrap(), ApprovalStatus::Granted);
    assert_eq!(service.status(&sid, &pending.approval_id).await.unwrap(), ApprovalStatus::Pending);
}

/// The approval writers read the log and then append to it, so each must
/// hold the session lease across both. A store that refuses an unleased
/// append turns that coupling into a failure: if any of them computes a
/// sequence without the lease, its append comes back `ConcurrentWriter`
/// instead of landing. (YA-72 / CONTRACT §4 C4.5)
#[tokio::test]
async fn every_approval_writer_holds_the_lease_across_its_read_and_append() {
    let store = Arc::new(FakeEventStore::new().requiring_lease());
    let service = session_scoped(store);
    let sid = SessionId::from_string("lease-held");

    let asked = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .expect("request holds the lease across its read and append");
    service
        .grant(
            &sid,
            &asked.approval_id,
            ApprovalScope::AllowOnce,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .expect("grant holds the lease across its read and append");

    let asked_again = service
        .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
        .await
        .expect("request still holds the lease with a decision already logged");
    service
        .deny(&sid, &asked_again.approval_id, "user")
        .await
        .expect("deny holds the lease across its read and append");
}

/// The question a request asks has to name the action, and the answer has to
/// name the same one — that is what makes a lease about an action rather
/// than about a capability. (CONTRACT §7 #178)
#[tokio::test]
async fn an_l2_lease_matches_one_complete_action_fingerprint() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("l2-lease");
    let call = facts(RiskLevel::L2, "a.txt", 1);

    let req = service.request_for_action(&sid, &call, "fs.write", "write a.txt").await.unwrap();
    // The request carries the fingerprint, so whoever answers is answering
    // about this call and not about "fs.write" in general.
    assert_eq!(req.action_digest.as_deref(), Some(call.digest.as_str()));
    assert_eq!(req.risk, Some(RiskLevel::L2));
    assert_eq!(req.targets, call.targets);
    assert_eq!(req.reversibility, Some(Reversibility::Compensatable));

    service
        .grant(
            &sid,
            &req.approval_id,
            ApprovalScope::AllowForTurn,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();

    let leases = service.leases_in_effect(&sid).await.unwrap();
    assert_eq!(leases.len(), 1);
    assert!(leases[0].matches_action(&call));
    assert_eq!(leases[0].action_digest.as_deref(), Some(call.digest.as_str()));
    assert_eq!(leases[0].task_revision, Some(1));
}

#[tokio::test]
async fn changing_a_path_or_argument_reasks() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("lease-drift");
    let approved = facts(RiskLevel::L2, "a.txt", 1);
    let req = service.request_for_action(&sid, &approved, "fs.write", "write a.txt").await.unwrap();
    service
        .grant(
            &sid,
            &req.approval_id,
            ApprovalScope::AllowForTurn,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();
    let lease = service.leases_in_effect(&sid).await.unwrap().remove(0);

    // The same call twice is the same action.
    assert!(lease.matches_action(&facts(RiskLevel::L2, "a.txt", 1)));

    // A different path is a different action: this is the drift the whole
    // slice is about.
    let elsewhere = facts(RiskLevel::L2, "b.txt", 1);
    assert_ne!(elsewhere.digest, approved.digest);
    assert!(!lease.matches_action(&elsewhere));

    // And so is the same path with different arguments — the target is
    // what a person reads, the arguments are what runs.
    let other_args = facts_with_arguments(
        RiskLevel::L2,
        "a.txt",
        1,
        serde_json::json!({ "path": "a.txt", "append": true }),
    );
    assert_ne!(other_args.digest, approved.digest);
    assert!(!lease.matches_action(&other_args));
}

#[tokio::test]
async fn changing_the_task_revision_reasks() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("lease-revision");
    let approved = facts(RiskLevel::L2, "a.txt", 1);
    let req = service.request_for_action(&sid, &approved, "fs.write", "write a.txt").await.unwrap();
    service
        .grant(
            &sid,
            &req.approval_id,
            ApprovalScope::AllowForTurn,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();
    let lease = service.leases_in_effect(&sid).await.unwrap().remove(0);

    // The work moved on. An answer given about revision 1 says nothing
    // about what revision 2 is doing, even with the same path.
    let next_revision = facts(RiskLevel::L2, "a.txt", 2);
    assert_ne!(next_revision.digest, approved.digest);
    assert!(!lease.matches_action(&next_revision));
    assert_eq!(lease.task_revision, Some(1));
}

#[tokio::test]
async fn old_capability_only_leases_do_not_authorize_l2_or_l3() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("legacy-lease");
    // A request with no action facts is what a pre-B4 log holds.
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "a.txt")
        .await
        .unwrap();
    assert!(req.action_digest.is_none());
    service
        .grant(
            &sid,
            &req.approval_id,
            ApprovalScope::AllowForTurn,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();
    let lease = service.leases_in_effect(&sid).await.unwrap().remove(0);
    assert!(lease.action_digest.is_none());

    // A capability-wide grant still spares the person every read, which is
    // what it was given for.
    assert!(lease.matches_action(&facts(RiskLevel::L1, "a.txt", 1)));
    // It does not stretch to an action that was never named.
    assert!(!lease.matches_action(&facts(RiskLevel::L2, "a.txt", 1)));
    assert!(!lease.matches_action(&facts(RiskLevel::L3, "a.txt", 1)));
}

#[tokio::test]
async fn an_l3_request_refuses_session_scope() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("l3-scope");
    let call = facts(RiskLevel::L3, "secrets.txt", 1);
    let req =
        service.request_for_action(&sid, &call, "fs.write", "write secrets.txt").await.unwrap();

    for scope in [ApprovalScope::AllowForSession, ApprovalScope::AllowForTurn] {
        assert!(matches!(
            service.grant(&sid, &req.approval_id, scope, "user", &ApprovalAuthority::test()).await,
            Err(ApprovalError::HighRiskNeedsPerActionConfirmation { .. })
        ));
    }
    // "Always" is a wider answer still, and is refused before it settles
    // anything else in the session.
    assert!(matches!(
        service.grant_always(&sid, &req.approval_id, "user", &ApprovalAuthority::test()).await,
        Err(ApprovalError::HighRiskNeedsPerActionConfirmation { .. })
    ));

    // The refusals settled nothing: the question is still open.
    assert_eq!(service.status(&sid, &req.approval_id).await.unwrap(), ApprovalStatus::Pending);
    assert!(service.leases_in_effect(&sid).await.unwrap().is_empty());

    // One action at a time is exactly what it does accept.
    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &ApprovalAuthority::test())
        .await
        .unwrap();
    assert!(service.covers_action(&sid, &req.approval_id, &call).await.unwrap());
}

#[tokio::test]
async fn an_l3_identical_retry_is_the_only_reuse() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("l3-retry");
    let call = facts(RiskLevel::L3, "secrets.txt", 1);
    let req =
        service.request_for_action(&sid, &call, "fs.write", "write secrets.txt").await.unwrap();
    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &ApprovalAuthority::test())
        .await
        .unwrap();

    assert!(service.covers_action(&sid, &req.approval_id, &call).await.unwrap());

    // A failure did not do the thing the person confirmed, so trying again
    // is the call they answered.
    append_payload(
        &service,
        &sid,
        EventType::TOOL_FAILED,
        serde_json::json!({ "policy": { "action_digest": call.digest } }),
    )
    .await;
    assert!(service.covers_action(&sid, &req.approval_id, &call).await.unwrap());

    // Once it has run, the confirmation is spent: the same call a second
    // time is a new decision, not a replay of this one.
    append_payload(
        &service,
        &sid,
        EventType::TOOL_COMPLETED,
        serde_json::json!({ "policy": { "action_digest": call.digest } }),
    )
    .await;
    assert!(!service.covers_action(&sid, &req.approval_id, &call).await.unwrap());
}

#[tokio::test]
async fn a_revoked_lease_stops_matching_at_once() {
    let store = Arc::new(FakeEventStore::new());
    let service = session_scoped(store);
    let sid = SessionId::from_string("lease-revoke");
    let call = facts(RiskLevel::L2, "a.txt", 1);
    let req = service.request_for_action(&sid, &call, "fs.write", "write a.txt").await.unwrap();
    service
        .grant(
            &sid,
            &req.approval_id,
            ApprovalScope::AllowForSession,
            "user",
            &ApprovalAuthority::test(),
        )
        .await
        .unwrap();
    assert_eq!(service.leases_in_effect(&sid).await.unwrap().len(), 1);

    service.revoke(&sid, &req.approval_id, "user", &ApprovalAuthority::test()).await.unwrap();

    // Nothing stands, and neither reader sees a lease: the fold is the only
    // answer, so a revocation cannot be pending anywhere.
    assert!(service.leases_in_effect(&sid).await.unwrap().is_empty());
    assert!(service.lease_for(&sid, &req.approval_id).await.unwrap().is_none());
    assert_eq!(service.status(&sid, &req.approval_id).await.unwrap(), ApprovalStatus::Cancelled);
    assert!(!service.covers_action(&sid, &req.approval_id, &call).await.unwrap());
}

#[tokio::test]
async fn a_revocation_needs_a_grant_to_take_back() {
    let (service, _, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();

    assert!(matches!(
        service.revoke(&sid, &req.approval_id, "user", &ApprovalAuthority::test()).await,
        Err(ApprovalError::NotGranted(_))
    ));
    assert_eq!(service.status(&sid, &req.approval_id).await.unwrap(), ApprovalStatus::Pending);
}

/// The answer records where it came from — never the token that proved it,
/// only the kind of entry point.
#[tokio::test]
async fn a_grant_records_the_authority_that_answered() {
    let (service, store, sid) = service();
    let req = service
        .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
        .await
        .unwrap();
    let authority = ApprovalAuthority::local_http("token-value").unwrap();
    service
        .grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user", &authority)
        .await
        .unwrap();

    let granted = store
        .read_stream(&sid, EventSeq::ZERO, usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.event.event_type.as_str() == EventType::APPROVAL_GRANTED)
        .expect("the grant is in the log");
    assert_eq!(granted.event.payload.get("authority").and_then(|v| v.as_str()), Some("local_http"));
    assert!(!granted.event.payload.to_string().contains("token-value"));
}

#[test]
fn a_non_interactive_process_cannot_claim_a_person_answered() {
    // The entry point earns the authority; a process with no terminal has
    // no way to produce one, which is what stops a tool child from running
    // `agent approve` on its own behalf.
    assert!(matches!(
        ApprovalAuthority::interactive_tty(false),
        Err(ApprovalAuthorityError::NotInteractive)
    ));
    assert!(ApprovalAuthority::interactive_tty(true).is_ok());
    assert!(matches!(ApprovalAuthority::local_http(""), Err(ApprovalAuthorityError::MissingToken)));
}
