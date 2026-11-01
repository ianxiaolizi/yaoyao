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
use agent_protocol::permission::{ActionFacts, ActionTarget, Reversibility, RiskLevel};
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

/// Where a person's answer came from, checked at the entry point and carried
/// into the service so the decision records it (B4).
///
/// The type has no public constructor but the three below, and each is a check
/// rather than a label:
///
/// * [`Self::interactive_tty`] refuses when stdin is not a terminal. A tool
///   child process that runs `agent approve` inherits a pipe, so it cannot
///   produce this value at all — which is the whole defence against the main
///   model approving its own calls.
/// * [`Self::local_http`] needs the loopback server's session token, which only
///   the authenticated GUI handler holds.
/// * [`Self::test`] exists for tests and for nothing else; a shipped build has
///   no path that constructs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalAuthority(AuthorityKind);

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorityKind {
    InteractiveTty,
    LocalHttp { token: String },
    Test,
}

/// Why an authority could not be established.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalAuthorityError {
    #[error(
        "an approval must come from an interactive terminal; a process without one cannot answer \
         for a person"
    )]
    NotInteractive,
    #[error("a local HTTP approval needs the loopback session token")]
    MissingToken,
}

impl ApprovalAuthority {
    /// The authority a command-line answer has, when this process has a terminal.
    pub fn interactive_tty(stdin_is_terminal: bool) -> Result<Self, ApprovalAuthorityError> {
        if !stdin_is_terminal {
            return Err(ApprovalAuthorityError::NotInteractive);
        }
        Ok(Self(AuthorityKind::InteractiveTty))
    }

    /// The authority a GUI answer has, carrying the token the request proved.
    pub fn local_http(token: &str) -> Result<Self, ApprovalAuthorityError> {
        if token.is_empty() {
            return Err(ApprovalAuthorityError::MissingToken);
        }
        Ok(Self(AuthorityKind::LocalHttp { token: token.to_string() }))
    }

    /// An authority for tests.
    pub fn test() -> Self {
        Self(AuthorityKind::Test)
    }

    /// The word durable replies keep. Never the token.
    pub fn described(&self) -> &'static str {
        match self.0 {
            AuthorityKind::InteractiveTty => "interactive_tty",
            AuthorityKind::LocalHttp { .. } => "local_http",
            AuthorityKind::Test => "test",
        }
    }
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
    #[error("approval is not granted: {0}")]
    NotGranted(ApprovalId),
    #[error("allow_for_session scope is not permitted by configuration")]
    SessionScopeNotAllowed,
    #[error(
        "a high-risk action needs one confirmation per action: approval {approval_id} cannot be \
         widened beyond `once`"
    )]
    HighRiskNeedsPerActionConfirmation { approval_id: ApprovalId },
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
    /// The one action this question is about (B4). Absent on a request written
    /// before the field existed, which makes the answer a capability-wide one.
    action_digest: Option<String>,
    risk: Option<RiskLevel>,
    task_id: Option<String>,
    task_revision: Option<u64>,
    reversibility: Option<Reversibility>,
    targets: Vec<ActionTarget>,
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
            action_digest: self.action_digest.clone(),
            task_id: self.task_id.clone(),
            task_revision: self.task_revision,
            risk: self.risk,
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
    ///
    /// The capability-wide form. The gateway uses
    /// [`Self::request_for_action`], which binds the question to the exact
    /// action; this one stays for callers that have no action facts (a fixture,
    /// a session assembled by hand) and produces a question a grant cannot
    /// widen over an `L2`/`L3` action.
    pub async fn request(
        &self,
        session_id: &SessionId,
        capability: Capability,
        tool_name: &str,
        args_summary: &str,
    ) -> Result<ApprovalRequest, ApprovalError> {
        self.append_request(session_id, capability, tool_name, args_summary, None).await
    }

    /// Raise the question a boundary action needs, bound to that action.
    ///
    /// The capability asked about is the first one the action declares, which
    /// is the same one the policy engine evaluated first and the one a lease
    /// will be matched against.
    pub async fn request_for_action(
        &self,
        session_id: &SessionId,
        action: &ActionFacts,
        tool_name: &str,
        args_summary: &str,
    ) -> Result<ApprovalRequest, ApprovalError> {
        let capability =
            action.capabilities.first().cloned().unwrap_or_else(|| Capability::new("unknown"));
        self.append_request(session_id, capability, tool_name, args_summary, Some(action)).await
    }

    async fn append_request(
        &self,
        session_id: &SessionId,
        capability: Capability,
        tool_name: &str,
        args_summary: &str,
        action: Option<&ActionFacts>,
    ) -> Result<ApprovalRequest, ApprovalError> {
        let req = ApprovalRequest {
            approval_id: ApprovalId::new(),
            session_id: session_id.clone(),
            capability,
            tool_name: tool_name.to_string(),
            args_summary: args_summary.to_string(),
            status: ApprovalStatus::Pending,
            created_at: Utc::now(),
            action_digest: action.map(|action| action.digest.clone()),
            risk: action.map(|action| action.risk),
            task_id: action.map(|action| action.task_id.clone()),
            task_revision: action.map(|action| action.task_revision),
            reversibility: action.map(|action| action.reversibility),
            targets: action.map(|action| action.targets.clone()).unwrap_or_default(),
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

    /// Record a grant decision, subject to the session-scope policy and to the
    /// per-action limits a high-risk request carries (B4).
    ///
    /// `authority` is an attestation the entry point had to earn — see
    /// [`ApprovalAuthority`]. The answer it produces is bound to the action the
    /// request named, so the reply says which fingerprint a person allowed.
    pub async fn grant(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        scope: ApprovalScope,
        actor: &str,
        authority: &ApprovalAuthority,
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
        // `L3` is confirmed one action at a time. A turn- or session-wide answer
        // would authorise calls nobody saw, and the risk is precisely that the
        // next one is different.
        if record.risk.is_some_and(|risk| risk >= RiskLevel::L3)
            && scope != ApprovalScope::AllowOnce
        {
            return Err(ApprovalError::HighRiskNeedsPerActionConfirmation {
                approval_id: approval_id.clone(),
            });
        }
        let reply = ApprovalReply {
            approval_id: approval_id.clone(),
            decision: ApprovalDecision::Grant,
            scope,
            actor: actor.to_string(),
            created_at: Utc::now(),
            action_digest: record.action_digest.clone(),
            risk: record.risk,
            authority: Some(authority.described().to_string()),
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
            action_digest: record.action_digest.clone(),
            risk: record.risk,
            // A refusal needs no authority: it cannot widen anything, and the
            // answer that would (a grant) is the one this field is for.
            authority: None,
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
        authority: &ApprovalAuthority,
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
        // "Always" is an answer about a kind of request. A high-risk one is not
        // a kind: the person confirmed *that* action, and the same tool with
        // other arguments is a different risk nobody has looked at.
        if subject.risk.is_some_and(|risk| risk >= RiskLevel::L3) {
            return Err(ApprovalError::HighRiskNeedsPerActionConfirmation {
                approval_id: approval_id.clone(),
            });
        }
        let targets = self
            .fold(session_id)
            .await?
            .into_iter()
            .filter(|r| {
                r.status == ApprovalStatus::Pending
                    && r.capability == subject.capability
                    && r.tool_name == subject.tool_name
                    // A high-risk request in the batch keeps its own question
                    // rather than failing the whole command: the rest of the
                    // answer is still what the person said.
                    && r.risk.is_none_or(|risk| risk < RiskLevel::L3)
            })
            .map(|r| r.approval_id)
            .collect::<Vec<_>>();
        let mut replies = Vec::with_capacity(targets.len());
        for id in targets {
            replies.push(
                self.grant(session_id, &id, ApprovalScope::AllowForSession, actor, authority)
                    .await?,
            );
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
                action_digest: r.action_digest,
                risk: r.risk,
                task_id: r.task_id,
                task_revision: r.task_revision,
                reversibility: r.reversibility,
                targets: r.targets,
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
            action_digest: record.action_digest,
            task_id: record.task_id,
            task_revision: record.task_revision,
            risk: record.risk,
        }))
    }

    /// Whether this approval's answer still covers `action` — the question the
    /// resume path asks before it runs a call nobody is going to see again.
    ///
    /// A standing lease is matched by the policy engine through
    /// [`LeaseSpec::matches_action`]; this is the narrower check for the one
    /// grant that leaves no standing lease, `allow_once`. Both answer the same
    /// question — "does the person's answer name this exact action?" — and both
    /// say no to `L3` unless the answer was made about that action and has not
    /// been spent on it.
    pub async fn covers_action(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        action: &ActionFacts,
    ) -> Result<bool, ApprovalError> {
        let record = self.record(session_id, approval_id).await?;
        if record.status != ApprovalStatus::Granted {
            return Ok(false);
        }
        // A request written before the fingerprint existed has no answer bound
        // to an action; it cannot cover one.
        if record.action_digest.as_deref() != Some(action.digest.as_str()) {
            return Ok(false);
        }
        // The digest is over the whole fact set, so these can only disagree if
        // the request and the call came from different revisions of the work.
        // Checked anyway: a digest is a hash, and a hash is not a proof.
        if record.task_id.as_deref().is_some_and(|id| id != action.task_id) {
            return Ok(false);
        }
        if record.task_revision.is_some_and(|revision| revision != action.task_revision) {
            return Ok(false);
        }
        if record.risk.is_some_and(|risk| risk < action.risk) {
            return Ok(false);
        }
        if action.risk >= RiskLevel::L3 {
            // One confirmation, one action. A wider scope cannot cover an `L3`
            // call at all, and an `once` grant stops covering it the moment
            // that call has run — otherwise a retry of a *failed* action would
            // be indistinguishable from a replay of one that succeeded.
            if record.granted_scope != Some(ApprovalScope::AllowOnce) {
                return Ok(false);
            }
            if self.completed_since_grant(session_id, approval_id, &action.digest).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the log records a successful run of `digest` after the grant of
    /// `approval_id`.
    ///
    /// A tool event carries the policy record it ran under, and that record
    /// carries the fingerprint — so a completion is the log's own word that this
    /// exact action happened. A failure is not a completion: the action did not
    /// happen, and a person who confirmed it was confirming a retry.
    async fn completed_since_grant(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        digest: &str,
    ) -> Result<bool, ApprovalError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        let Some(granted) = events.iter().find(|stored| {
            stored.event.event_type.as_str() == EventType::APPROVAL_GRANTED
                && cause_approval_id(&stored.event.payload) == approval_id.as_str()
        }) else {
            return Ok(false);
        };
        Ok(events.iter().any(|stored| {
            stored.seq > granted.seq
                && stored.event.event_type.as_str() == EventType::TOOL_COMPLETED
                && stored.event.payload.get("policy").and_then(|policy| policy.get("action_digest"))
                    == Some(&serde_json::Value::String(digest.to_string()))
        }))
    }

    /// Withdraw a grant that is still in effect.
    ///
    /// A grant is a durable answer, and a durable answer needs a way to stop
    /// being one. The fold treats this as the end of the approval — the same
    /// state as a cancellation, because the outcome for the work is the same: it
    /// did not run and will not run under this answer — while the event keeps
    /// the distinction for whoever reads the log: a person took it back.
    pub async fn revoke(
        &self,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        actor: &str,
        authority: &ApprovalAuthority,
    ) -> Result<(), ApprovalError> {
        let _lease = self.store.lease(session_id).await?;
        let record = self.record(session_id, approval_id).await?;
        if record.status != ApprovalStatus::Granted {
            return Err(ApprovalError::NotGranted(approval_id.clone()));
        }
        let next = self.next_seq(session_id).await?;
        let payload = serde_json::json!({
            "approval_id": approval_id,
            "actor": actor,
            "authority": authority.described(),
        });
        let draft = self.draft(session_id, EventType::APPROVAL_REVOKED, payload, Actor::User);
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(())
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
                        action_digest: req.action_digest,
                        risk: req.risk,
                        task_id: req.task_id,
                        task_revision: req.task_revision,
                        reversibility: req.reversibility,
                        targets: req.targets,
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
            // A withdrawn grant ends the answer, and the state that means "did
            // not run and will not" is the one a cancellation already names —
            // the work does not care which of the two happened. The event stays
            // distinct because the person who reads the log does.
            EventType::APPROVAL_REVOKED => {
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

/// Out of line because `#[cfg(test)]` must be the last thing a production
/// file says (the boundary guards in agent-testkit assert exactly that).
#[cfg(test)]
mod tests;
