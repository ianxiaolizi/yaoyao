//! ApprovalService — event-sourced, non-blocking approval lifecycle.
//!
//! Approvals are durable facts, not blocking calls. A `request` appends an
//! `approval/requested` event and returns immediately; the caller (a loop) is
//! responsible for yielding the run. A later `grant`/`deny` appends the
//! decision event, and the service folds the log back into current status, so a
//! restarted process sees the same pending approvals as a long-running one.
//!
//! A decision is not the same thing as a grant still being worth something, and
//! that second question is answered here rather than by the policy engine —
//! the engine has no log and no clock, and a second place judging liveness could
//! disagree with this one. [`ApprovalService::leases_in_effect`] is the answer,
//! and the scopes mean:
//!
//! - `allow_once` leaves no standing lease. The call it was about is dispatched
//!   by `resume`, which is why "once" needs no mechanism of its own.
//! - `allow_for_turn` lasts until the session's next turn *ends*. A grant is
//!   always made while the session is held at a yielded turn — nothing else can
//!   be waiting on an answer — so the turn it feeds is the one the `resume`
//!   continuation starts, and `turn/completed` (or failed, cancelled,
//!   interrupted) is where it stops being worth anything. The record then folds
//!   to [`ApprovalStatus::Expired`]: "no longer in effect" is what that word
//!   means, and the enum has no word for "consumed by the turn it was for".
//! - `allow_for_session` lasts until `SESSION_GRANT_TTL` after the grant.
//!
//! A grant is not the only thing that stops being worth something. A *question*
//! nobody answers holds the session at that yielded turn for as long as it is
//! pending — `run_pending` refuses to open a turn while a wait is outstanding —
//! so an unanswered request past `PENDING_TTL` expires too, and `resume`, which
//! already knows how to settle an `Expired` decision as "did not run", carries
//! the turn forward and the session moves again.
//!
//! Both expiries are *derived*: the fold computes them from the events and the
//! clock, so every reader sees them the moment they are true. What the fold
//! cannot do is write, so [`ApprovalService::sweep_expired`] records the ones it
//! found in the log — not to make an approval expire, but so that a log a person
//! reads says why it ended, instead of leaving them to re-derive it from a
//! timestamp.

use std::collections::HashMap;
use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{ApprovalId, EventId, EventSeq, SchemaVersion, SessionId};
use agent_protocol::permissions::{
    ApprovalDecision, ApprovalReply, ApprovalRequest, ApprovalScope, ApprovalStatus, Capability,
    LeaseSpec,
};
use chrono::{DateTime, Duration, Utc};

/// How long a request may stand unanswered before it expires.
///
/// A day is not a tuned number: it is long enough that nobody is surprised by a
/// question they were about to answer, and short enough that a session left
/// overnight is not stuck when its person comes back. Making it a setting is a
/// knob nobody has asked for; the constant is here so that when one is wanted,
/// there is one place to reach into.
pub const PENDING_TTL: Duration = Duration::hours(24);

/// How long a session-scoped grant stays in effect.
///
/// A session has no end that is written down anywhere — nothing in this build
/// says "this session is over" — so a session grant needs an end of some other
/// kind or it is a permission that outlives the work it was given for.
pub const SESSION_GRANT_TTL: Duration = Duration::hours(24);

/// The event-sourced approval lifecycle for a session.
pub struct ApprovalService {
    store: Arc<dyn EventStore>,
    allow_session_scope: bool,
}

/// Errors from ApprovalService operations.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("approval not found: {0}")]
    NotFound(ApprovalId),
    #[error("approval is not pending: {0}")]
    NotPending(ApprovalId),
    #[error("allow_for_session scope is not permitted by configuration")]
    SessionScopeNotAllowed,
    #[error("payload error: {0}")]
    Payload(#[from] serde_json::Error),
}

/// The folded state of one approval.
#[derive(Debug, Clone)]
struct ApprovalRecord {
    approval_id: ApprovalId,
    capability: Capability,
    tool_name: String,
    args_summary: String,
    status: ApprovalStatus,
    granted_scope: Option<ApprovalScope>,
    created_at: DateTime<Utc>,
    granted_at: Option<DateTime<Utc>>,
    /// Whether the log already says why this one ended. Set by the
    /// `approval/expired` arm, and the only thing that makes
    /// [`ApprovalService::sweep_expired`] idempotent: the derived expiry is a
    /// fact about the clock, which does not go away when it is written down.
    expiry_recorded: bool,
}

impl ApprovalRecord {
    /// The lease this record yields, when it is one still worth something.
    ///
    /// `None` covers several things on purpose: never granted, granted with a
    /// scope that leaves nothing standing, and granted for a turn or a session
    /// that has since run out (the fold has already rewritten those to
    /// `Expired`). A caller wants the one answer — "is there a lease here" —
    /// because the reason matters to whoever reads the log, not to whoever is
    /// deciding whether to ask a person a question.
    fn lease(&self) -> Option<LeaseSpec> {
        if self.status != ApprovalStatus::Granted {
            return None;
        }
        let scope = self.granted_scope?;
        // `allow_once` authorised one call, and `resume` dispatches that call
        // itself without asking the engine again. A standing lease here would
        // authorise a second one.
        if scope == ApprovalScope::AllowOnce {
            return None;
        }
        Some(LeaseSpec {
            capability: self.capability.clone(),
            scope,
            expires_at: self.expires_at(),
        })
    }

    /// When this grant stops being in effect, if the clock is what stops it.
    ///
    /// `allow_for_turn` ends at the next turn-terminal event, which the fold
    /// already applies from the log; it has no timestamp to report, because it
    /// is not the clock that ends it. A session grant outlives everything the
    /// log currently says, so the only end it can have is `SESSION_GRANT_TTL`
    /// after it was made.
    fn expires_at(&self) -> Option<DateTime<Utc>> {
        if self.granted_scope != Some(ApprovalScope::AllowForSession) {
            return None;
        }
        self.granted_at.map(|at| at + SESSION_GRANT_TTL)
    }

    /// Why this one ended, for the event that puts the reason in the log.
    ///
    /// Only meaningful for a record the fold has already expired; a granted
    /// scope means the grant ran out, and no scope at all means the question
    /// itself did.
    fn expiry_cause(&self) -> &'static str {
        match self.granted_scope {
            Some(ApprovalScope::AllowForTurn) => "turn_ended",
            Some(_) => "grant_ttl",
            None => "pending_ttl",
        }
    }
}

impl ApprovalService {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self { store, allow_session_scope: false }
    }

    /// Opt into `allow_for_session` grants. Off by default: a session-scoped
    /// grant must be explicitly permitted by configuration, never a silent
    /// widening of a single-use approval.
    pub fn with_session_scope(mut self, allowed: bool) -> Self {
        self.allow_session_scope = allowed;
        self
    }

    /// Raise a non-blocking approval request and return the pending fact.
    pub async fn request(
        &self,
        session_id: &SessionId,
        capability: Capability,
        tool_name: &str,
        args_summary: &str,
    ) -> Result<ApprovalRequest, ApprovalError> {
        let req = ApprovalRequest {
            approval_id: ApprovalId::new(),
            session_id: session_id.clone(),
            capability,
            tool_name: tool_name.to_string(),
            args_summary: args_summary.to_string(),
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
        };
        // Requesting is a read-then-append like every other writer here: the
        // sequence is computed from the log, and the append must land on the
        // log that read saw. Held across both, not around the turn. (YA-72 /
        // CONTRACT §4 C4.5)
        let _lease = self.store.lease(session_id).await?;
        let next = self.next_seq(session_id).await?;
        let draft = self.draft(
            session_id,
            EventType::APPROVAL_REQUESTED,
            serde_json::to_value(&req)?,
            Actor::Scheduler,
        );
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(req)
    }

    /// Record a grant decision, subject to the session-scope policy.
    pub async fn grant(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        scope: ApprovalScope,
        actor: &str,
    ) -> Result<ApprovalReply, ApprovalError> {
        if scope == ApprovalScope::AllowForSession && !self.allow_session_scope {
            return Err(ApprovalError::SessionScopeNotAllowed);
        }
        // The decision is read from the log and then written to it; the append
        // must land on the log that read saw. (YA-72 / CONTRACT §4 C4.5)
        let _lease = self.store.lease(session_id).await?;
        let record = self.record(session_id, approval_id).await?;
        if record.status != ApprovalStatus::Pending {
            return Err(ApprovalError::NotPending(approval_id.clone()));
        }
        let reply = ApprovalReply {
            approval_id: approval_id.clone(),
            decision: ApprovalDecision::Grant,
            scope,
            actor: actor.to_string(),
            created_at: Utc::now(),
        };
        let next = self.next_seq(session_id).await?;
        let draft = self.draft(
            session_id,
            EventType::APPROVAL_GRANTED,
            serde_json::to_value(&reply)?,
            Actor::User,
        );
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(reply)
    }

    /// Record a deny decision.
    pub async fn deny(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        actor: &str,
    ) -> Result<ApprovalReply, ApprovalError> {
        // Same shape as `grant`: read the record, then write the decision on
        // the log that read saw. (YA-72 / CONTRACT §4 C4.5)
        let _lease = self.store.lease(session_id).await?;
        let record = self.record(session_id, approval_id).await?;
        if record.status != ApprovalStatus::Pending {
            return Err(ApprovalError::NotPending(approval_id.clone()));
        }
        let reply = ApprovalReply {
            approval_id: approval_id.clone(),
            decision: ApprovalDecision::Deny,
            scope: ApprovalScope::AllowOnce,
            actor: actor.to_string(),
            created_at: Utc::now(),
        };
        let next = self.next_seq(session_id).await?;
        let draft = self.draft(
            session_id,
            EventType::APPROVAL_DENIED,
            serde_json::to_value(&reply)?,
            Actor::User,
        );
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(reply)
    }

    /// Grant this approval for the whole session, settling every other pending
    /// approval that asks for the same thing.
    ///
    /// "The same thing" is the `(capability, tool_name)` pair. A person who
    /// answers with "always" is answering a *kind* of request: widening that to
    /// every pending request would grant more than they said, and narrowing it
    /// to this one id would leave the identical question waiting behind it,
    /// which is the question they just said not to ask again.
    ///
    /// The cascade runs under the session lease so a `resume` in another process
    /// cannot settle one of these requests in between — the same reason `Resume`
    /// holds the lease across its whole read-decide-act sequence.
    pub async fn grant_always(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        actor: &str,
    ) -> Result<Vec<ApprovalReply>, ApprovalError> {
        if !self.allow_session_scope {
            return Err(ApprovalError::SessionScopeNotAllowed);
        }
        let _lease = self.store.lease(session_id).await?;
        // Read the named record first: it is both the subject of the grant and
        // the shape the cascade matches against, and a named id that is not
        // pending makes the whole command fail rather than silently settle
        // whatever else matched.
        let subject = self.record(session_id, approval_id).await?;
        if subject.status != ApprovalStatus::Pending {
            return Err(ApprovalError::NotPending(approval_id.clone()));
        }
        let targets = self
            .fold(session_id)
            .await?
            .into_iter()
            .filter(|r| {
                r.status == ApprovalStatus::Pending
                    && r.capability == subject.capability
                    && r.tool_name == subject.tool_name
            })
            .map(|r| r.approval_id)
            .collect::<Vec<_>>();
        let mut replies = Vec::with_capacity(targets.len());
        for id in targets {
            replies.push(self.grant(session_id, &id, ApprovalScope::AllowForSession, actor).await?);
        }
        Ok(replies)
    }

    /// Deny this approval, and every other pending approval in the session.
    ///
    /// A denial is a judgement about what the session is doing, not about one
    /// request: leaving the rest pending would keep asking a person who has just
    /// said no. The narrow form — deny only the named id — is the caller's to
    /// ask for, not something the cascade should guess at.
    pub async fn deny_cascade(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        actor: &str,
    ) -> Result<Vec<ApprovalReply>, ApprovalError> {
        let _lease = self.store.lease(session_id).await?;
        let subject = self.record(session_id, approval_id).await?;
        if subject.status != ApprovalStatus::Pending {
            return Err(ApprovalError::NotPending(approval_id.clone()));
        }
        let targets = self
            .fold(session_id)
            .await?
            .into_iter()
            .filter(|r| r.status == ApprovalStatus::Pending)
            .map(|r| r.approval_id)
            .collect::<Vec<_>>();
        let mut replies = Vec::with_capacity(targets.len());
        for id in targets {
            replies.push(self.deny(session_id, &id, actor).await?);
        }
        Ok(replies)
    }

    /// Current status of an approval.
    pub async fn status(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
    ) -> Result<ApprovalStatus, ApprovalError> {
        Ok(self.record(session_id, approval_id).await?.status)
    }

    /// All pending approvals for a session.
    ///
    /// Sweeps first: a reader asking what is waiting to be answered should not
    /// be shown a question that has already stopped being one, and the sweep is
    /// what leaves the reason behind in the log for whoever reads it next.
    pub async fn pending(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ApprovalRequest>, ApprovalError> {
        self.sweep_expired(session_id).await?;
        Ok(self
            .fold(session_id)
            .await?
            .into_iter()
            .filter(|r| r.status == ApprovalStatus::Pending)
            .map(|r| ApprovalRequest {
                approval_id: r.approval_id,
                session_id: session_id.clone(),
                capability: r.capability,
                tool_name: r.tool_name,
                args_summary: r.args_summary,
                status: r.status,
                created_at: r.created_at,
            })
            .collect())
    }

    /// The leases still in effect for this session, for the policy engine to
    /// match against.
    ///
    /// This is the join between an approval and a policy decision: it is the
    /// only thing that turns "the user said yes once" into "this call does not
    /// need to ask". A decision that is not `Granted` yields nothing, and so
    /// does a grant whose scope has run out — see the module doc for what each
    /// scope is worth and for where that is decided.
    pub async fn leases_in_effect(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<LeaseSpec>, ApprovalError> {
        Ok(self.fold(session_id).await?.iter().filter_map(|r| r.lease()).collect())
    }

    /// The lease a granted approval yields, if any.
    pub async fn lease_for(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
    ) -> Result<Option<LeaseSpec>, ApprovalError> {
        let record = self.record(session_id, approval_id).await?;
        if record.status != ApprovalStatus::Granted {
            return Ok(None);
        }
        let expires_at = record.expires_at();
        Ok(Some(LeaseSpec {
            capability: record.capability,
            scope: record.granted_scope.unwrap_or(ApprovalScope::AllowOnce),
            expires_at,
        }))
    }

    /// Write down the expiries the fold has already derived, and return the ones
    /// recorded.
    ///
    /// The expiry itself is not in question — `fold` decided it from the log and
    /// the clock, and every reader sees it whether or not this is ever called.
    /// What the fold cannot do is write, so a session whose request simply timed
    /// out would read as a request that stopped being pending for no stated
    /// reason. This puts the reason in the log. It is idempotent: the record the
    /// next fold builds already carries `expiry_recorded` from the event this
    /// wrote, so a second sweep adds nothing.
    ///
    /// Run under the session lease because it decides on a read and then acts on
    /// it, the same reason `Resume` holds one across its whole sequence.
    pub async fn sweep_expired(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ApprovalId>, ApprovalError> {
        let _lease = self.store.lease(session_id).await?;
        let expired = self
            .fold(session_id)
            .await?
            .into_iter()
            .filter(|r| r.status == ApprovalStatus::Expired && !r.expiry_recorded)
            .map(|r| {
                let cause = r.expiry_cause();
                (r.approval_id, cause)
            })
            .collect::<Vec<_>>();
        let mut swept = Vec::with_capacity(expired.len());
        for (id, cause) in expired {
            self.append_cause(session_id, EventType::APPROVAL_EXPIRED, &id, cause).await?;
            swept.push(id);
        }
        Ok(swept)
    }

    /// Cancel every pending request in the session, recording why.
    ///
    /// A pending request holds the session at a yielded turn — `run_pending`
    /// refuses to open another while one stands — so there has to be a way to
    /// take it back. That is what the user asking the run to stop is: not an
    /// answer to the question, but a decision that the question no longer needs
    /// one. `resume` already settles a `Cancelled` decision as a call that did
    /// not run.
    pub async fn cancel_pending(
        &self,
        session_id: &SessionId,
        cause: &str,
    ) -> Result<Vec<ApprovalId>, ApprovalError> {
        let _lease = self.store.lease(session_id).await?;
        let pending = self
            .fold(session_id)
            .await?
            .into_iter()
            .filter(|r| r.status == ApprovalStatus::Pending)
            .map(|r| r.approval_id)
            .collect::<Vec<_>>();
        let mut cancelled = Vec::with_capacity(pending.len());
        for id in pending {
            self.append_cause(session_id, EventType::APPROVAL_CANCELLED, &id, cause).await?;
            cancelled.push(id);
        }
        Ok(cancelled)
    }

    /// Append one of the two events that carry a reason rather than a decision.
    async fn append_cause(
        &self,
        session_id: &SessionId,
        event_type: &str,
        approval_id: &ApprovalId,
        cause: &str,
    ) -> Result<(), ApprovalError> {
        let next = self.next_seq(session_id).await?;
        let payload = serde_json::json!({ "approval_id": approval_id, "cause": cause });
        let draft = self.draft(session_id, event_type, payload, Actor::Scheduler);
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(())
    }

    async fn record(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
    ) -> Result<ApprovalRecord, ApprovalError> {
        self.fold(session_id)
            .await?
            .into_iter()
            .find(|r| &r.approval_id == approval_id)
            .ok_or_else(|| ApprovalError::NotFound(approval_id.clone()))
    }

    async fn fold(&self, session_id: &SessionId) -> Result<Vec<ApprovalRecord>, ApprovalError> {
        self.fold_at(session_id, Utc::now()).await
    }

    /// Fold as of an explicit instant. Only the clock differs from `fold`, and
    /// the tests are the whole reason it is a parameter: an expiry that takes a
    /// day to observe is a test that never runs.
    async fn fold_at(
        &self,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Vec<ApprovalRecord>, ApprovalError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(fold_approvals(&events, now)?)
    }

    async fn next_seq(&self, session_id: &SessionId) -> Result<EventSeq, ApprovalError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        Ok(events.last().map(|e| e.seq.next()).unwrap_or(EventSeq::ZERO))
    }

    fn draft(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: serde_json::Value,
        actor: Actor,
    ) -> EventDraft {
        EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(event_type),
            schema_version: SchemaVersion::current(),
            session_id: session_id.clone(),
            run_id: None,
            turn_id: None,
            step_id: None,
            actor,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload,
            metadata: serde_json::json!({}),
            ignorable: false,
        }
    }
}

/// Fold approval events into their latest records, in append order.
///
/// `now` is what makes the expiries derived: two of them are facts about the
/// clock rather than about an event, and folding against the clock is what lets
/// every reader agree without any of them having to sweep first.
fn fold_approvals(
    events: &[StoredEvent],
    now: DateTime<Utc>,
) -> Result<Vec<ApprovalRecord>, serde_json::Error> {
    let mut by_id: HashMap<String, ApprovalRecord> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for stored in events {
        let e = &stored.event;
        match e.event_type.as_str() {
            EventType::APPROVAL_REQUESTED => {
                let req: ApprovalRequest = serde_json::from_value(e.payload.clone())?;
                let key = req.approval_id.to_string();
                if !by_id.contains_key(&key) {
                    order.push(key.clone());
                }
                by_id.insert(
                    key,
                    ApprovalRecord {
                        approval_id: req.approval_id,
                        capability: req.capability,
                        tool_name: req.tool_name,
                        args_summary: req.args_summary,
                        status: req.status,
                        granted_scope: None,
                        created_at: req.created_at,
                        granted_at: None,
                        expiry_recorded: false,
                    },
                );
            }
            EventType::APPROVAL_GRANTED => {
                let reply: ApprovalReply = serde_json::from_value(e.payload.clone())?;
                if let Some(r) = by_id.get_mut(&reply.approval_id.to_string()) {
                    r.status = ApprovalStatus::Granted;
                    r.granted_scope = Some(reply.scope);
                    r.granted_at = Some(reply.created_at);
                }
            }
            EventType::APPROVAL_DENIED => {
                let reply: ApprovalReply = serde_json::from_value(e.payload.clone())?;
                if let Some(r) = by_id.get_mut(&reply.approval_id.to_string()) {
                    r.status = ApprovalStatus::Denied;
                }
            }
            // Where a sweep writes down an expiry the fold had already derived.
            // Reading it back is what tells a later sweep that the reason is
            // already in the log.
            EventType::APPROVAL_EXPIRED => {
                if let Some(r) = by_id.get_mut(&cause_approval_id(&e.payload)) {
                    r.status = ApprovalStatus::Expired;
                    r.expiry_recorded = true;
                }
            }
            EventType::APPROVAL_CANCELLED => {
                if let Some(r) = by_id.get_mut(&cause_approval_id(&e.payload)) {
                    r.status = ApprovalStatus::Cancelled;
                }
            }
            // A turn-scoped grant is worth something only for the turn it was
            // made for. A grant is only ever made while the session is held at
            // a yielded turn — nothing else can be waiting on an answer — so
            // the turn it feeds is the one that starts after it, and this is
            // where that turn ends. `turn/yielded` is deliberately not in this
            // list: the continuation of a yielded turn is a *new* turn, and the
            // grant is for exactly that one.
            EventType::TURN_COMPLETED
            | EventType::TURN_FAILED
            | EventType::TURN_CANCELLED
            | EventType::TURN_INTERRUPTED => {
                for record in by_id.values_mut() {
                    if record.status == ApprovalStatus::Granted
                        && record.granted_scope == Some(ApprovalScope::AllowForTurn)
                    {
                        record.status = ApprovalStatus::Expired;
                    }
                }
            }
            _ => {}
        }
    }

    // The expiries no event says. A question nobody answered, and a session
    // grant the clock has outlived: both are true the moment they are true, and
    // deriving them here is what makes them true for every reader at once.
    for record in by_id.values_mut() {
        if record.status == ApprovalStatus::Pending
            && now.signed_duration_since(record.created_at) >= PENDING_TTL
        {
            record.status = ApprovalStatus::Expired;
        }
        if record.status == ApprovalStatus::Granted
            && let Some(end) = record.expires_at()
            && now >= end
        {
            record.status = ApprovalStatus::Expired;
        }
    }

    Ok(order.into_iter().filter_map(|key| by_id.remove(&key)).collect())
}

/// The approval an `approval/expired` or `approval/cancelled` payload is about.
fn cause_approval_id(payload: &serde_json::Value) -> String {
    payload.get("approval_id").and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
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

        service.grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user").await.unwrap();

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
        service.grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user").await.unwrap();

        assert!(matches!(
            service.grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user").await,
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
            service.grant(&sid, &req.approval_id, ApprovalScope::AllowForSession, "user").await,
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
            .grant(&sid, &req.approval_id, ApprovalScope::AllowForSession, "user")
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
        service.grant(&sid, &once.approval_id, ApprovalScope::AllowOnce, "user").await.unwrap();

        // `once` authorised the one call `resume` dispatches itself. Read as a
        // standing permission it would authorise a second call, which is the
        // opposite of what the person answered.
        assert!(service.leases_in_effect(&sid).await.unwrap().is_empty());

        let session = service
            .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
            .await
            .unwrap();
        service
            .grant(&sid, &session.approval_id, ApprovalScope::AllowForSession, "user")
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
        service.grant(&sid, &req.approval_id, ApprovalScope::AllowForTurn, "user").await.unwrap();

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
            .grant(&sid, &fresh.approval_id, ApprovalScope::AllowForSession, "user")
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
        assert_eq!(
            service.status(&old_sid, &old.approval_id).await.unwrap(),
            ApprovalStatus::Expired
        );
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
        service.grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user").await.unwrap();

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
        service.grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user").await.unwrap();

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
        let other = service
            .request(&sid, Capability::new(Capability::FS_WRITE), "fs.edit", "c")
            .await
            .unwrap();

        let replies = service.grant_always(&sid, &first.approval_id, "user").await.unwrap();

        let settled: Vec<&str> = replies.iter().map(|r| r.approval_id.as_str()).collect();
        assert_eq!(settled.len(), 2, "{settled:?}");
        assert!(settled.contains(&first.approval_id.as_str()));
        assert!(settled.contains(&second.approval_id.as_str()));
        for reply in &replies {
            assert_eq!(reply.scope, ApprovalScope::AllowForSession);
        }
        assert_eq!(
            service.status(&sid, &other.approval_id).await.unwrap(),
            ApprovalStatus::Pending
        );
    }

    #[tokio::test]
    async fn grant_always_needs_the_session_scope_to_be_permitted() {
        let (service, _, sid) = service();
        let req = service
            .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "x")
            .await
            .unwrap();

        assert!(matches!(
            service.grant_always(&sid, &req.approval_id, "user").await,
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
        service.grant(&sid, &req.approval_id, ApprovalScope::AllowOnce, "user").await.unwrap();
        let other = service
            .request(&sid, Capability::new(Capability::FS_WRITE), "fs.write", "y")
            .await
            .unwrap();

        assert!(matches!(
            service.grant_always(&sid, &req.approval_id, "user").await,
            Err(ApprovalError::NotPending(_))
        ));
        // A cascade that could not name its subject settles nothing else.
        assert_eq!(
            service.status(&sid, &other.approval_id).await.unwrap(),
            ApprovalStatus::Pending
        );
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
        service.grant(&sid, &settled.approval_id, ApprovalScope::AllowOnce, "user").await.unwrap();
        let pending = service
            .request(&sid, Capability::new(Capability::PROCESS_SPAWN), "shell.run", "ls")
            .await
            .unwrap();

        assert!(matches!(
            service.deny_cascade(&sid, &settled.approval_id, "user").await,
            Err(ApprovalError::NotPending(_))
        ));
        // The earlier grant stands; the still-open question is untouched.
        assert_eq!(
            service.status(&sid, &settled.approval_id).await.unwrap(),
            ApprovalStatus::Granted
        );
        assert_eq!(
            service.status(&sid, &pending.approval_id).await.unwrap(),
            ApprovalStatus::Pending
        );
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
            .grant(&sid, &asked.approval_id, ApprovalScope::AllowOnce, "user")
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
}
