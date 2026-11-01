//! Inbox — the kernel's persistent pending-input queue.
//!
//! The Inbox is a first-class kernel capability. Inputs are persisted as
//! durable `inbox/*` events before they count as "received"; a Scheduler later
//! claims them. The Inbox is stateless: every read folds the event stream, so a
//! crashed session rebuilds its pending queue from the log rather than from
//! process memory.

use std::collections::HashMap;
use std::sync::Arc;

use agent_ports::event_store::{EventStore, EventStoreError};
use agent_protocol::events::{Actor, EventDraft, EventType, StoredEvent};
use agent_protocol::ids::{
    EventId, EventSeq, InboxItemId, IntentId, RunId, SchemaVersion, SessionId, TurnId,
};
use agent_protocol::inbox::{InboxItem, InboxItemStatus, InboxKind};
use agent_protocol::intent::{Intent, IntentStatus};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::adjudicate;

/// The payload key holding when a [`InboxKind::Scheduled`] item is due.
pub const SCHEDULED_DUE_AT: &str = "due_at";
/// The payload key holding the text a [`InboxKind::Scheduled`] turn carries.
pub const SCHEDULED_TEXT: &str = "text";
/// The payload key binding an item to the intent that asked for it.
pub const INTENT_ID: &str = "intent_id";
/// The payload key carrying what the rule layer made of the message that queued
/// the item ([`crate::adjudicate::Route::as_str`]).
///
/// Recorded at write time because the route is a function of the queue as it
/// was when the message arrived; a later reader looking at a different queue
/// would answer a different question. Slice A3 reads this to find exactly the
/// messages the rules left `unclear`, instead of re-running the rules against a
/// queue they were not written for.
pub const ROUTE: &str = "route";

/// Why a queued instruction was discarded when a newer one displaced it.
const SUPERSEDED_REASON: &str = "a newer instruction from the same person stands in its place";
/// Why a queued instruction was discarded when a stop withdrew it.
const STOPPED_REASON: &str = "withdrawn by a stop";

/// What a submitted message asked for, and what the queue did about it.
///
/// The route is reported back rather than left in the log alone: the caller
/// that took the message (the CLI, the web API) has to tell the person what
/// happened to it, and "your message was taken as a stop that withdrew two
/// queued instructions" is a different answer from "your message is queued".
#[derive(Debug, Clone)]
pub struct Submitted {
    /// What the rule layer made of the message.
    pub route: adjudicate::Route,
    /// The durable record of what was asked.
    pub intent: Intent,
    /// The item queued for it: a turn for anything that opens one, an interrupt
    /// for a stop that had a run to stop. `None` when nothing was queued.
    pub item: Option<InboxItem>,
    /// Every queue item this message took out, in queue order — including the
    /// ones queued before the ledger, which have no intent to name.
    pub displaced: Vec<InboxItemId>,
    /// Instructions this one displaced, in queue order.
    pub superseded: Vec<IntentId>,
    /// Instructions a stop withdrew, in queue order.
    pub cancelled: Vec<IntentId>,
}

/// The intent an item was queued for, if it was queued for one.
///
/// Items written before the ledger have no `intent_id`; that is a fact about
/// the log, not an error, so it reads as `None` rather than failing the fold.
pub fn item_intent_id(item: &InboxItem) -> Option<IntentId> {
    let raw = item.payload.get(INTENT_ID)?;
    serde_json::from_value(raw.clone()).ok()
}

/// The kernel's pending-input queue, backed by the durable event log.
pub struct Inbox {
    store: Arc<dyn EventStore>,
}

/// What a due check found.
///
/// Three answers, because "nothing is due" and "something is due but cannot be
/// read" are different decisions: the first means wait, the second means
/// discard the unreadable item and look again. Folding them into `None` would
/// leave a bad item at the head of the queue forever.
#[derive(Debug, Clone)]
pub enum Due {
    /// No pending scheduled item is due at or before the given time.
    None,
    /// The earliest due item, ready to run.
    Due(InboxItem),
    /// A scheduled item whose payload cannot be read as one. The caller should
    /// discard it and continue; its `due_at` cannot be compared with anything.
    Unreadable { item: InboxItem, reason: String },
}

/// When a scheduled item says it is due, read out of its payload.
///
/// Returns the reason as a sentence rather than a typed error: every caller
/// either discards the item with that sentence or prints it, and none of them
/// branches on the kind of malformation.
pub fn scheduled_due_at(item: &InboxItem) -> Result<DateTime<Utc>, String> {
    let raw = item
        .payload
        .get(SCHEDULED_DUE_AT)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("scheduled payload has no string `{SCHEDULED_DUE_AT}`"))?;
    DateTime::parse_from_rfc3339(raw)
        .map(|when| when.with_timezone(&Utc))
        .map_err(|e| format!("scheduled payload `{SCHEDULED_DUE_AT}` is not RFC3339: {e}"))
}

/// The due time and text of a scheduled item, or why it is unreadable as one.
fn scheduled_parts(item: &InboxItem) -> Result<(DateTime<Utc>, String), String> {
    let due_at = scheduled_due_at(item)?;
    match item.payload.get(SCHEDULED_TEXT).and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => Ok((due_at, text.to_string())),
        Some(_) => Err(format!("scheduled payload `{SCHEDULED_TEXT}` is blank")),
        None => Err(format!("scheduled payload has no string `{SCHEDULED_TEXT}`")),
    }
}

/// Why an item left the queue unworked.
///
/// `discarded` is one status for several different facts. The port already
/// says a refusal and a failure are recorded differently (`control.rs`); the
/// log can only honour that if the difference is a field, not a sentence
/// (C-1). `reason` stays as the human-readable supplement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardCause {
    /// No handler claimed the control at all.
    Unhandled,
    /// A handler claimed it and declined.
    Refused,
    /// A handler claimed it and could not complete.
    Failed,
    /// The item's payload was not readable as the kind it claims to be.
    Unreadable,
    /// The work the item stood for is already done (a spent wakeup).
    Spent,
    /// The user withdrew the request before it ran (a cancelled schedule).
    Cancelled,
    /// A later instruction from the same person outranks this one.
    ///
    /// Not [`Cancelled`](Self::Cancelled): nothing was taken back, the request
    /// still stands in the record and only stopped being the newest one. The
    /// distinction is what lets a reader tell "I changed my mind" from "I said
    /// what I wanted next".
    Superseded,
}

impl DiscardCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unhandled => "unhandled",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Unreadable => "unreadable",
            Self::Spent => "spent",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }
}

/// Errors from Inbox operations.
#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("inbox item not found: {0}")]
    ItemNotFound(InboxItemId),
    #[error("inbox item is not pending: {0}")]
    ItemNotPending(InboxItemId),
    #[error("inbox item is not a scheduled item: {0}")]
    NotScheduled(InboxItemId),
    #[error("scheduled text is empty")]
    EmptyText,
    /// A direct message with nothing in it. Nothing was written: an empty turn
    /// would ask the model to act on nothing, and the queue would carry an item
    /// no reader could tell apart from a blank steer.
    #[error("the message is empty; there is nothing to ask for")]
    EmptyInstruction,
    #[error("inbox payload error: {0}")]
    Payload(#[from] serde_json::Error),
}

impl Inbox {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self { store }
    }

    /// Persist a new pending item and return it.
    pub async fn append(
        &self,
        session_id: &SessionId,
        kind: InboxKind,
        payload: serde_json::Value,
        priority: u32,
        actor: Actor,
    ) -> Result<InboxItem, InboxError> {
        let item = InboxItem {
            item_id: InboxItemId::new(),
            session_id: session_id.clone(),
            kind,
            payload,
            priority,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };
        self.append_item(session_id, item.clone(), Vec::new(), actor).await?;
        Ok(item)
    }

    /// Record a direct user instruction, decide what it displaces, and queue
    /// whatever it asks for — as one append.
    ///
    /// This is the path a person's message takes into the session. It exists
    /// because the 2026-11-01 incident had one shape: a new instruction
    /// arrived, the queue still held the old one, and the scheduler answered
    /// the old one. Reading the log afterwards could not say which request an
    /// answer belonged to. So the intent, the queue item it asks for, and the
    /// withdrawals it causes all land in **one** batch under **one** lease:
    /// there is no moment where a queued turn exists without the intent that
    /// asked for it, or where an item was discarded by a message that was never
    /// recorded.
    ///
    /// What it displaces is [`adjudicate::route`]'s decision, not this
    /// function's: this one only writes down what that decision says. A route
    /// that displaces nothing writes no withdrawal; a route that displaces
    /// writes one per instruction it overtakes, and each names the new intent
    /// as what overtook it.
    ///
    /// The stamp is taken once and reused for every record in the batch
    /// (CONTRACT §7 裁决 #20): the request, the item that waits for it, and the
    /// withdrawals it caused happened at the same instant by construction. It
    /// is a parameter rather than a clock read because a caller that already
    /// has a `now` (the CLI, the web loop) must not have a second one read
    /// behind its back.
    pub async fn submit_user_message(
        &self,
        session_id: &SessionId,
        text: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<Submitted, InboxError> {
        let text = text.into();
        // Read and append under one lease: a decision about what this message
        // displaces is only sound against the queue as it is when the batch
        // lands, and another process driving this session — `agent steer` is
        // exactly that — can move the sequence in between. (YA-36)
        let _lease = self.store.lease(session_id).await?;
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        let next = events.last().map(|stored| stored.seq.next()).unwrap_or(EventSeq::ZERO);
        let pending_turns: Vec<InboxItem> = fold_inbox(&events)?
            .into_iter()
            .filter(|item| item.status == InboxItemStatus::Pending)
            .filter(|item| item.kind == InboxKind::NextTurn)
            .collect();
        let lifecycle = crate::lifecycle::fold(&events);
        // Whether a run is in flight — the only state an interrupt is addressed
        // to. A turn waiting on a decision counts: interrupting it is exactly
        // what a person means by stopping a session that is asking them
        // something.
        let has_run = lifecycle.open_run.is_some() || lifecycle.yielded_turn.is_some();
        // Anything this message could be *about*: a run in flight, a turn
        // waiting on a decision, or an instruction queued ahead of it.
        let has_work = has_run || !pending_turns.is_empty();
        let route = adjudicate::route(&text, adjudicate::RouteState { has_work });
        let Some(kind) = route.intent_kind() else {
            return Err(InboxError::EmptyInstruction);
        };

        // A stop only has an item when there is a run to address: an interrupt
        // is a request to *a run*, and one queued with nothing running would
        // kill the next turn before its first step (the same rule
        // `commands::interrupt` follows). A stop that only empties the queue
        // does so directly and queues nothing.
        let item_kind = if route.queues_a_turn() {
            Some(InboxKind::NextTurn)
        } else if route == adjudicate::Route::Stop && has_run {
            Some(InboxKind::Interrupt)
        } else {
            None
        };
        let mut item = item_kind.map(|kind| InboxItem {
            item_id: InboxItemId::new(),
            session_id: session_id.clone(),
            kind,
            payload: serde_json::Value::Null,
            priority: 0,
            created_at: now,
            status: InboxItemStatus::Pending,
        });

        let mut intent = Intent::user_direct(
            session_id.clone(),
            text.clone(),
            item.as_ref().map(|i| i.item_id.clone()).unwrap_or_else(InboxItemId::new),
            now,
        );
        // `user_direct` records what the write path can assert on its own. The
        // relation and the discharge are the rule layer's answers, so they are
        // overwritten here rather than guessed in the constructor.
        intent.kind = kind;
        if item.is_none() {
            // Nothing was queued for it: the instruction names no item.
            intent.item_id = None;
        }
        if route == adjudicate::Route::Stop {
            // A stop does its whole job at once — it withdraws what is queued
            // and asks the run to stop — so there is no turn that will answer
            // it later. Recording it as still standing would leave `status`
            // reporting an instruction that nothing is going to run.
            intent.status = IntentStatus::Consumed;
        }

        // What this message takes out of the queue, decided before anything is
        // written so the new intent can name the intents it replaces.
        let displaced: Vec<(&InboxItem, Option<IntentId>)> =
            if route.displaces_pending() || route == adjudicate::Route::Stop {
                pending_turns.iter().map(|item| (item, item_intent_id(item))).collect()
            } else {
                Vec::new()
            };

        let mut drafts: Vec<EventDraft> = Vec::new();
        if next == EventSeq::ZERO {
            drafts.push(crate::session_lifecycle::started_draft(session_id, Actor::User));
        }

        let stopping = route == adjudicate::Route::Stop;
        // The intents behind the items this message displaces. An item queued
        // before the ledger (by a test, a tool, or an older build) is still
        // displaced — the queue is the queue — but there is no record to mark.
        let displaced_intents: Vec<IntentId> =
            displaced.iter().filter_map(|(_, intent)| intent.clone()).collect();
        if !stopping {
            // The new request names what it replaced, in the same record that
            // says it arrived.
            intent.supersedes = displaced_intents.clone();
        }

        // The request first, then what it displaced, then where it waits: a
        // reader following the batch sees the instruction arrive before it sees
        // anything taken out of the queue because of it.
        drafts.push(self.draft(
            session_id,
            EventType::INTENT_RECORDED,
            serde_json::to_value(&intent)?,
            Actor::User,
        ));
        let (cause, reason) = if stopping {
            (DiscardCause::Cancelled, STOPPED_REASON)
        } else {
            (DiscardCause::Superseded, SUPERSEDED_REASON)
        };
        let mut superseded = Vec::new();
        let mut cancelled = Vec::new();
        for (old, old_intent) in &displaced {
            drafts.push(self.discard_draft(session_id, &old.item_id, cause, Some(reason)).await?);
            let Some(old_intent) = old_intent else { continue };
            if stopping {
                drafts.push(self.draft(
                    session_id,
                    EventType::INTENT_CANCELLED,
                    serde_json::json!({ "intent_id": old_intent, "reason": reason }),
                    Actor::User,
                ));
                cancelled.push(old_intent.clone());
            } else {
                drafts.push(self.draft(
                    session_id,
                    EventType::INTENT_SUPERSEDED,
                    serde_json::json!({
                        "intent_id": old_intent,
                        "superseded_by": intent.intent_id,
                    }),
                    Actor::User,
                ));
                superseded.push(old_intent.clone());
            }
        }

        if let Some(item) = &mut item {
            item.payload = match item.kind {
                InboxKind::Interrupt => serde_json::json!({
                    "reason": "user_stop",
                    "intent_id": intent.intent_id,
                }),
                _ => serde_json::json!({
                    "text": text,
                    "intent_id": intent.intent_id,
                    "route": route.as_str(),
                }),
            };
            drafts.push(self.draft(
                session_id,
                EventType::INBOX_APPENDED,
                serde_json::to_value(&*item)?,
                Actor::User,
            ));
        }

        self.store.append(session_id, next, drafts).await?;
        let displaced_items = displaced.iter().map(|(item, _)| item.item_id.clone()).collect();
        Ok(Submitted { route, intent, item, displaced: displaced_items, superseded, cancelled })
    }

    /// Append one item, plus any events that must land with it, atomically.
    ///
    /// Order in the batch is fixed: `session/started` (when this is the
    /// session's first event), then `leading`, then `inbox/appended`. A reader
    /// that sees the item can therefore see everything the item depends on.
    async fn append_item(
        &self,
        session_id: &SessionId,
        item: InboxItem,
        leading: Vec<EventDraft>,
        actor: Actor,
    ) -> Result<(), InboxError> {
        // Read the log's end and append at it as one step: another process on
        // this session — `agent steer` is exactly this path — can move the
        // sequence in between, and an append from a stale read is refused.
        // (YA-36)
        let _lease = self.store.lease(session_id).await?;
        let next = self.next_seq(session_id).await?;
        let mut drafts = Vec::with_capacity(leading.len() + 2);
        // A session used for the first time starts here. `next == 0` means this
        // append is the first event the session will ever have, so writing
        // `session/started` beside it is what makes "a session's log says it
        // started" true rather than something a reader has to infer from
        // whichever event happens to come first. One append, one lease: an item
        // queued without the opening line, or an opening line for an item that
        // was never queued, cannot exist.
        if next == EventSeq::ZERO {
            drafts.push(crate::session_lifecycle::started_draft(session_id, actor.clone()));
        }
        drafts.extend(leading);
        drafts.push(self.draft(
            session_id,
            EventType::INBOX_APPENDED,
            serde_json::to_value(&item)?,
            actor,
        ));
        self.store.append(session_id, next, drafts).await?;
        Ok(())
    }

    /// Queue text for the turn already in flight.
    ///
    /// The Scheduler reads it at the next step boundary, so the current step is
    /// never interrupted — that is the whole difference between steering and
    /// cancelling.
    ///
    /// `expected_turn_id` is the turn this text is *for*. When the caller knows
    /// which turn it is steering, passing it is what keeps a note written for
    /// one turn from being injected into a different one — a race that a
    /// cross-process caller cannot avoid by checking first, because the check
    /// and the write are not one step.
    pub async fn steer(
        &self,
        session_id: &SessionId,
        text: impl Into<String>,
        expected_turn_id: Option<&TurnId>,
    ) -> Result<InboxItem, InboxError> {
        let payload = serde_json::json!({
            "text": text.into(),
            "expected_turn_id": expected_turn_id,
        });
        self.append(session_id, InboxKind::NextStep, payload, 0, Actor::User).await
    }

    /// Queue a change for the runtime to apply at the next safe boundary.
    ///
    /// `control` names what to do — `plugin.disable` is the first of these — and
    /// `fields` carries whatever else it needs. The name is **not** checked
    /// here: a queue that refused names it did not recognise would be answering
    /// a question only an assembled runtime can, and guessing it from the queue
    /// would turn "nobody handles this" into "this was never asked".
    ///
    /// Unlike [`steer`](Self::steer), this is not addressed to a turn: a change
    /// to the runtime belongs to the session, and the boundary it waits for is
    /// whichever comes first.
    pub async fn control(
        &self,
        session_id: &SessionId,
        control: impl Into<String>,
        fields: Value,
    ) -> Result<InboxItem, InboxError> {
        let name = control.into();
        let payload = match fields {
            Value::Object(mut map) => {
                map.insert("control".to_string(), Value::String(name));
                Value::Object(map)
            }
            // No fields is the common case. Anything that is not an object is
            // kept whole under a key rather than merged or dropped: a payload
            // this queue cannot lay out is not one it should reshape.
            Value::Null => serde_json::json!({ "control": name }),
            other => serde_json::json!({ "control": name, "fields": other }),
        };
        self.append(session_id, InboxKind::Control, payload, 0, Actor::User).await
    }

    /// Every pending control item, in claim order.
    ///
    /// All of them, for the reason [`steering`](Self::steering) gives: a
    /// boundary is where the queue drains, and leaving one for a boundary that
    /// may never come would silently drop an operator's command.
    pub async fn controls(&self, session_id: &SessionId) -> Result<Vec<InboxItem>, InboxError> {
        Ok(self
            .pending(session_id)
            .await?
            .into_iter()
            .filter(|i| i.kind == InboxKind::Control)
            .collect())
    }

    /// Ask a running turn to stop at its next step boundary.
    ///
    /// Like steering, this does not interrupt the step in progress; it decides
    /// whether there is another one.
    pub async fn interrupt(
        &self,
        session_id: &SessionId,
        reason: impl Into<String>,
    ) -> Result<InboxItem, InboxError> {
        let payload = serde_json::json!({ "reason": reason.into() });
        self.append(session_id, InboxKind::Interrupt, payload, 0, Actor::User).await
    }

    /// Mark a pending item as claimed by a run.
    pub async fn claim(
        &self,
        session_id: &SessionId,
        item_id: &InboxItemId,
        run_id: &RunId,
    ) -> Result<(), InboxError> {
        // The validation and the append are one step — see `append` above. (YA-36)
        let _lease = self.store.lease(session_id).await?;
        let draft = self.claim_draft(session_id, item_id, Some(run_id)).await?;
        let next = self.next_seq(session_id).await?;
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(())
    }

    /// Build the claim draft for a pending item without appending it, so a
    /// caller can include it in a larger atomic batch (e.g. the Scheduler's run
    /// batch). The item is validated as pending but not mutated here.
    ///
    /// `run_id` is optional because not every claim is a run's: a control item is
    /// taken by the boundary that applies it, and no run was involved. The
    /// payload records that as `null` rather than naming a run that did not take
    /// it — a reader asking "which run consumed this" must be able to get "none"
    /// as an answer.
    pub async fn claim_draft(
        &self,
        session_id: &SessionId,
        item_id: &InboxItemId,
        run_id: Option<&RunId>,
    ) -> Result<EventDraft, InboxError> {
        let item = self.get(session_id, item_id).await?;
        if item.status != InboxItemStatus::Pending {
            return Err(InboxError::ItemNotPending(item_id.clone()));
        }
        let payload = serde_json::json!({ "item_id": item_id, "run_id": run_id });
        Ok(self.draft(session_id, EventType::INBOX_CLAIMED, payload, Actor::Scheduler))
    }

    /// Build the discard draft for a pending item without appending it.
    ///
    /// This exists for the same reason `claim_draft` does: a decision made
    /// inside a turn cannot append on its own, because the turn's events land as
    /// one batch at settle. The item is validated as pending here; the record
    /// lands with the rest of the turn.
    ///
    /// The cause is what makes a discard readable as a decision rather than as
    /// a disappearance: an item that was answered "no", one no handler claimed,
    /// and one whose handler failed look the same in the status alone. The
    /// reason stays as the human-readable sentence beside it (C-1).
    pub async fn discard_draft(
        &self,
        session_id: &SessionId,
        item_id: &InboxItemId,
        cause: DiscardCause,
        reason: Option<&str>,
    ) -> Result<EventDraft, InboxError> {
        let item = self.get(session_id, item_id).await?;
        if item.status != InboxItemStatus::Pending {
            return Err(InboxError::ItemNotPending(item_id.clone()));
        }
        let payload =
            serde_json::json!({ "item_id": item_id, "cause": cause.as_str(), "reason": reason });
        Ok(self.draft(session_id, EventType::INBOX_DISCARDED, payload, Actor::Scheduler))
    }

    /// Discard a pending item, saying why in both a machine-readable cause and
    /// a human-readable sentence.
    pub async fn discard(
        &self,
        session_id: &SessionId,
        item_id: &InboxItemId,
        cause: DiscardCause,
        reason: Option<&str>,
    ) -> Result<(), InboxError> {
        // The pending check and the append are one step — see `append` above.
        // (YA-36)
        let _lease = self.store.lease(session_id).await?;
        let draft = self.discard_draft(session_id, item_id, cause, reason).await?;
        let next = self.next_seq(session_id).await?;
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(())
    }

    /// All pending items, ordered by priority (lower first) then FIFO.
    pub async fn pending(&self, session_id: &SessionId) -> Result<Vec<InboxItem>, InboxError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        let mut items = fold_inbox(&events)?;
        items.retain(|i| i.status == InboxItemStatus::Pending);
        // Stable sort: equal priorities keep their original append order.
        items.sort_by_key(|i| i.priority);
        Ok(items)
    }

    /// The first pending `next_turn` item, if any.
    pub async fn next_turn(&self, session_id: &SessionId) -> Result<Option<InboxItem>, InboxError> {
        Ok(self.pending(session_id).await?.into_iter().find(|i| i.kind == InboxKind::NextTurn))
    }

    /// Every pending `next_step` item, in claim order.
    ///
    /// All of them, not the first: a step boundary is where the queue drains,
    /// and leaving the second note for a boundary that may never come would
    /// silently drop it. There is deliberately no "just the head" accessor — it
    /// would be the one a caller reached for, and it would lose those notes.
    pub async fn steering(&self, session_id: &SessionId) -> Result<Vec<InboxItem>, InboxError> {
        Ok(self
            .pending(session_id)
            .await?
            .into_iter()
            .filter(|i| i.kind == InboxKind::NextStep)
            .collect())
    }

    /// The first pending `interrupt` item, if any.
    pub async fn pending_interrupt(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<InboxItem>, InboxError> {
        Ok(self.pending(session_id).await?.into_iter().find(|i| i.kind == InboxKind::Interrupt))
    }

    /// Queue a turn to start at `due_at`.
    ///
    /// `due_at` is an input from the caller, not a stamp: the item's
    /// `created_at` remains the append time, and whether it is due is decided
    /// later against a `now` the caller supplies. An empty or blank text is
    /// refused rather than stored — a scheduled turn that says nothing is one
    /// the model cannot act on, and refusing here keeps it out of the log
    /// instead of leaving a bad item for the scheduler to discard.
    pub async fn schedule(
        &self,
        session_id: &SessionId,
        due_at: DateTime<Utc>,
        text: impl Into<String>,
        priority: u32,
        actor: Actor,
    ) -> Result<InboxItem, InboxError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(InboxError::EmptyText);
        }
        let payload = serde_json::json!({
            SCHEDULED_DUE_AT: due_at.to_rfc3339(),
            SCHEDULED_TEXT: text,
        });
        self.append(session_id, InboxKind::Scheduled, payload, priority, actor).await
    }

    /// Every pending scheduled item, in queue order.
    ///
    /// Queue order is priority then FIFO, which is what breaks a tie between
    /// two items due at the same instant.
    pub async fn scheduled(&self, session_id: &SessionId) -> Result<Vec<InboxItem>, InboxError> {
        Ok(self
            .pending(session_id)
            .await?
            .into_iter()
            .filter(|i| i.kind == InboxKind::Scheduled)
            .collect())
    }

    /// The next scheduled item due at or before `now`.
    ///
    /// An unreadable item is reported before any due item, in queue order: its
    /// `due_at` cannot be compared, so leaving it in place would block the
    /// queue forever. The caller discards it and asks again.
    ///
    /// Among readable due items the earliest `due_at` wins; equal instants fall
    /// back to queue order (priority, then append order).
    pub async fn next_due(
        &self,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Due, InboxError> {
        let items = self.scheduled(session_id).await?;

        // Readability is checked across the whole queue first, so a bad item
        // behind a future-dated one still gets discarded rather than waiting
        // for the future item to become due.
        for item in &items {
            if let Err(reason) = scheduled_parts(item) {
                return Ok(Due::Unreadable { item: item.clone(), reason });
            }
        }

        let mut earliest: Option<(DateTime<Utc>, InboxItem)> = None;
        for item in items {
            let Ok((due_at, _)) = scheduled_parts(&item) else {
                continue;
            };
            if due_at > now {
                continue;
            }
            let replace = earliest.as_ref().is_none_or(|(seen, _)| due_at < *seen);
            if replace {
                earliest = Some((due_at, item));
            }
        }

        Ok(match earliest {
            Some((_, item)) => Due::Due(item),
            None => Due::None,
        })
    }

    /// Withdraw a pending scheduled item, recording that the user did it.
    ///
    /// This does not go through [`discard`](Self::discard): that records the
    /// scheduler as the actor, and a cancellation is the user's decision, not
    /// the scheduler's. The item is re-read under the lease so the kind and
    /// status checks and the append are one step.
    pub async fn cancel_scheduled(
        &self,
        session_id: &SessionId,
        item_id: &InboxItemId,
    ) -> Result<(), InboxError> {
        let _lease = self.store.lease(session_id).await?;
        let item = self.get(session_id, item_id).await?;
        if item.kind != InboxKind::Scheduled {
            return Err(InboxError::NotScheduled(item_id.clone()));
        }
        if item.status != InboxItemStatus::Pending {
            return Err(InboxError::ItemNotPending(item_id.clone()));
        }
        let payload = serde_json::json!({
            "item_id": item_id,
            "cause": DiscardCause::Cancelled.as_str(),
            "reason": "cancelled by the user",
        });
        let draft = self.draft(session_id, EventType::INBOX_DISCARDED, payload, Actor::User);
        let next = self.next_seq(session_id).await?;
        self.store.append(session_id, next, vec![draft]).await?;
        Ok(())
    }

    /// The current item state for `item_id`, regardless of status.
    async fn get(
        &self,
        session_id: &SessionId,
        item_id: &InboxItemId,
    ) -> Result<InboxItem, InboxError> {
        let events = self.store.read_stream(session_id, EventSeq::ZERO, usize::MAX).await?;
        fold_inbox(&events)?
            .into_iter()
            .find(|i| i.item_id == *item_id)
            .ok_or_else(|| InboxError::ItemNotFound(item_id.clone()))
    }

    /// The sequence number the next append must target.
    async fn next_seq(&self, session_id: &SessionId) -> Result<EventSeq, InboxError> {
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

/// Fold the inbox-related events into their latest item states, in append order.
fn fold_inbox(events: &[StoredEvent]) -> Result<Vec<InboxItem>, InboxError> {
    let mut by_id: HashMap<String, InboxItem> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    for event in events {
        match event.event.event_type.as_str() {
            EventType::INBOX_APPENDED => {
                let item: InboxItem = serde_json::from_value(event.event.payload.clone())?;
                let key = item.item_id.to_string();
                if !by_id.contains_key(&key) {
                    order.push(key.clone());
                }
                by_id.insert(key, item);
            }
            EventType::INBOX_CLAIMED => {
                let item_id: InboxItemId = serde_json::from_value(
                    event.event.payload.get("item_id").cloned().unwrap_or_default(),
                )?;
                if let Some(item) = by_id.get_mut(&item_id.to_string()) {
                    item.status = InboxItemStatus::Claimed;
                }
            }
            EventType::INBOX_DISCARDED => {
                let item_id: InboxItemId = serde_json::from_value(
                    event.event.payload.get("item_id").cloned().unwrap_or_default(),
                )?;
                if let Some(item) = by_id.get_mut(&item_id.to_string()) {
                    item.status = InboxItemStatus::Discarded;
                }
            }
            _ => {}
        }
    }

    Ok(order.into_iter().filter_map(|key| by_id.remove(&key)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::inbox::InboxKind;
    use agent_testkit::FakeEventStore;

    fn inbox() -> (Inbox, Arc<FakeEventStore>, SessionId) {
        let store = Arc::new(FakeEventStore::new());
        let inbox = Inbox::new(store.clone());
        let sid = SessionId::from_string("test-session");
        (inbox, store, sid)
    }

    /// Every writer that reads the log and appends at it does so under the
    /// session lease. The store refuses an append that arrives without one, so
    /// each call below fails unless its read → append is covered. (YA-36)
    #[tokio::test]
    async fn every_inbox_writer_holds_the_lease_across_its_read_and_append() {
        let store = Arc::new(FakeEventStore::new().requiring_lease());
        let inbox = Inbox::new(store);
        let sid = SessionId::from_string("test-session");

        let claimed = inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({}), 0, Actor::User)
            .await
            .unwrap();
        let discarded = inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({}), 0, Actor::User)
            .await
            .unwrap();

        inbox.claim(&sid, &claimed.item_id, &RunId::new()).await.unwrap();
        inbox.discard(&sid, &discarded.item_id, DiscardCause::Refused, None).await.unwrap();
        inbox.submit_user_message(&sid, "lease please", Utc::now()).await.unwrap();
    }

    #[tokio::test]
    async fn submitting_a_user_turn_records_the_intent_beside_the_item() {
        let (inbox, store, sid) = inbox();

        let submitted =
            inbox.submit_user_message(&sid, "fix the junction escape", Utc::now()).await.unwrap();
        let intent = submitted.intent.clone();
        let item = submitted.item.expect("an instruction opens a turn");

        assert_eq!(item.kind, InboxKind::NextTurn);
        assert_eq!(item.payload["text"], "fix the junction escape");
        assert_eq!(item.payload["intent_id"], intent.intent_id.as_str());
        assert_eq!(intent.item_id.as_ref(), Some(&item.item_id));
        assert_eq!(intent.text, "fix the junction escape");

        // One append batch: the intent and the item it asks for cannot exist
        // apart in the log.
        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let types: Vec<&str> = events.iter().map(|e| e.event.event_type.as_str()).collect();
        assert_eq!(
            types,
            vec![EventType::SESSION_STARTED, EventType::INTENT_RECORDED, EventType::INBOX_APPENDED]
        );
        assert_eq!(events[1].event.actor, Actor::User);
        assert_eq!(events[1].event.payload["summary"], "fix the junction escape");
    }

    #[tokio::test]
    async fn a_new_instruction_supersedes_the_turn_queued_before_it() {
        let (inbox, store, sid) = inbox();

        let first =
            inbox.submit_user_message(&sid, "修复 junction 逃逸", Utc::now()).await.unwrap();
        let second = inbox.submit_user_message(&sid, "先把越权检查补上", Utc::now()).await.unwrap();

        assert_eq!(second.route, adjudicate::Route::NewTask);
        let first_intent = first.intent.intent_id.clone();
        assert_eq!(second.superseded, vec![first_intent.clone()]);
        assert_eq!(second.intent.supersedes, vec![first_intent.clone()]);

        // The old item left the queue as `superseded`, not as work that ran.
        let old = first.item.expect("the first instruction queued a turn");
        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let discarded = events
            .iter()
            .find(|e| {
                e.event.event_type.as_str() == EventType::INBOX_DISCARDED
                    && e.event.payload["item_id"] == serde_json::json!(old.item_id)
            })
            .expect("the old item is recorded as discarded");
        assert_eq!(discarded.event.payload["cause"], "superseded");

        // Both directions of the relation are on the record: the old intent says
        // what overtook it, the new one says what it replaced.
        let ledger = crate::intent::IntentLedger::new(store);
        let all = ledger.all(&sid).await.unwrap();
        assert_eq!(all[0].status, IntentStatus::Superseded);
        assert_eq!(all[0].superseded_by.as_ref(), Some(&second.intent.intent_id));
        assert_eq!(all[1].status, IntentStatus::Active);

        // And the queue now holds exactly the newer instruction.
        let pending = inbox.pending(&sid).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload["text"], "先把越权检查补上");
    }

    #[tokio::test]
    async fn a_continuation_is_queued_beside_the_work_not_instead_of_it() {
        let (inbox, _, sid) = inbox();

        let first =
            inbox.submit_user_message(&sid, "修复 junction 逃逸", Utc::now()).await.unwrap();
        let second = inbox.submit_user_message(&sid, "继续", Utc::now()).await.unwrap();

        assert_eq!(second.route, adjudicate::Route::Continue);
        assert_eq!(second.intent.kind, agent_protocol::intent::IntentKind::Continuation);
        assert!(second.superseded.is_empty(), "an acknowledgement displaces nothing");

        let pending = inbox.pending(&sid).await.unwrap();
        assert_eq!(pending.len(), 2, "both instructions are still queued");
        let first_item = first.item.expect("the first instruction queued a turn");
        assert!(pending.iter().any(|item| item.item_id == first_item.item_id));
    }

    #[tokio::test]
    async fn a_stop_withdraws_the_queue_and_queues_no_turn() {
        let (inbox, store, sid) = inbox();

        let first =
            inbox.submit_user_message(&sid, "修复 junction 逃逸", Utc::now()).await.unwrap();
        let stop = inbox.submit_user_message(&sid, "停止", Utc::now()).await.unwrap();

        assert_eq!(stop.route, adjudicate::Route::Stop);
        assert!(stop.item.is_none(), "a stop that has stopped nothing is not a turn");
        assert_eq!(stop.intent.status, IntentStatus::Consumed);
        assert_eq!(stop.intent.kind, agent_protocol::intent::IntentKind::Interrupt);
        assert_eq!(stop.cancelled, vec![first.intent.intent_id.clone()]);

        assert!(inbox.pending(&sid).await.unwrap().is_empty(), "nothing is left to run");

        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let discarded = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .expect("the withdrawal is recorded");
        assert_eq!(discarded.event.payload["cause"], "cancelled");
        assert!(
            events.iter().any(|e| e.event.event_type.as_str() == EventType::INTENT_CANCELLED),
            "the withdrawn instruction is marked, not silently dropped"
        );

        let ledger = crate::intent::IntentLedger::new(store);
        let all = ledger.all(&sid).await.unwrap();
        assert_eq!(all[0].status, IntentStatus::Cancelled, "withdrawn is not superseded");
    }

    #[tokio::test]
    async fn a_stop_while_a_run_is_open_queues_an_interrupt_that_names_it() {
        let (inbox, store, sid) = inbox();
        // A run the lifecycle fold can see: `run/started` with the run it
        // opens (a draft built by `Inbox::draft` names no run, which is right
        // for the events the queue writes itself and wrong for this one).
        let started = EventDraft {
            event_id: EventId::new(),
            event_type: EventType::new(EventType::RUN_STARTED),
            schema_version: SchemaVersion::current(),
            session_id: sid.clone(),
            run_id: Some(RunId::new()),
            turn_id: None,
            step_id: None,
            actor: Actor::Scheduler,
            created_at: Utc::now(),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::json!({}),
            metadata: serde_json::json!({}),
            ignorable: false,
        };
        store.append(&sid, EventSeq::ZERO, vec![started]).await.unwrap();

        let stop = inbox.submit_user_message(&sid, "停止", Utc::now()).await.unwrap();

        let interrupt = stop.item.expect("a running session gets an interrupt");
        assert_eq!(interrupt.kind, InboxKind::Interrupt);
        assert_eq!(interrupt.payload["reason"], "user_stop");
        assert_eq!(interrupt.payload["intent_id"], stop.intent.intent_id.as_str());
        assert_eq!(
            inbox.pending_interrupt(&sid).await.unwrap().map(|item| item.item_id),
            Some(interrupt.item_id)
        );
    }

    #[tokio::test]
    async fn an_empty_message_is_refused_and_writes_nothing() {
        let (inbox, store, sid) = inbox();
        assert!(matches!(
            inbox.submit_user_message(&sid, "   ", Utc::now()).await,
            Err(InboxError::EmptyInstruction)
        ));
        assert!(store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_makes_item_pending() {
        let (inbox, _, sid) = inbox();
        let item = inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();
        assert_eq!(item.status, InboxItemStatus::Pending);

        let pending = inbox.pending(&sid).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].item_id, item.item_id);
        assert_eq!(pending[0].kind, InboxKind::NextTurn);
    }

    #[tokio::test]
    async fn claim_removes_item_from_pending() {
        let (inbox, _, sid) = inbox();
        let item = inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({}), 0, Actor::User)
            .await
            .unwrap();

        inbox.claim(&sid, &item.item_id, &RunId::new()).await.unwrap();
        assert!(inbox.pending(&sid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn discard_removes_item_from_pending() {
        let (inbox, _, sid) = inbox();
        let item = inbox
            .append(&sid, InboxKind::NextStep, serde_json::json!({}), 0, Actor::User)
            .await
            .unwrap();

        inbox.discard(&sid, &item.item_id, DiscardCause::Refused, None).await.unwrap();
        assert!(inbox.pending(&sid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_is_ordered_by_priority_then_fifo() {
        let (inbox, _, sid) = inbox();
        inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({}), 5, Actor::User)
            .await
            .unwrap();
        inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({}), 1, Actor::User)
            .await
            .unwrap();
        inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({}), 5, Actor::User)
            .await
            .unwrap();

        let pending = inbox.pending(&sid).await.unwrap();
        let priorities: Vec<u32> = pending.iter().map(|i| i.priority).collect();
        assert_eq!(priorities, vec![1, 5, 5]);
    }

    #[tokio::test]
    async fn claiming_unknown_or_claimed_item_fails() {
        let (inbox, _, sid) = inbox();
        let item = inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({}), 0, Actor::User)
            .await
            .unwrap();

        let unknown = InboxItemId::new();
        assert!(matches!(
            inbox.claim(&sid, &unknown, &RunId::new()).await,
            Err(InboxError::ItemNotFound(_))
        ));

        inbox.claim(&sid, &item.item_id, &RunId::new()).await.unwrap();
        assert!(matches!(
            inbox.claim(&sid, &item.item_id, &RunId::new()).await,
            Err(InboxError::ItemNotPending(_))
        ));
    }

    #[tokio::test]
    async fn a_fresh_inbox_rebuilds_state_from_the_log() {
        let (inbox, store, sid) = inbox();
        inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();

        // A second Inbox over the same store (e.g. after a crash restart) must
        // see the same pending item — proof that state comes from the log.
        let rebuilt = Inbox::new(store);
        let pending = rebuilt.pending(&sid).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload, serde_json::json!({"text": "hi"}));
    }

    #[tokio::test]
    async fn steer_and_interrupt_land_in_the_queues_the_scheduler_reads() {
        let (inbox, _, sid) = inbox();
        let steer = inbox.steer(&sid, "keep going", None).await.unwrap();
        let interrupt = inbox.interrupt(&sid, "stop").await.unwrap();

        assert_eq!(steer.kind, InboxKind::NextStep);
        assert!(steer.payload["expected_turn_id"].is_null(), "{:?}", steer.payload);
        assert_eq!(interrupt.kind, InboxKind::Interrupt);

        // Each boundary read sees its own queue and nothing else. Draining
        // steering must not consume the interrupt, and an interrupt must not be
        // mistaken for something to hand the model.
        let steering = inbox.steering(&sid).await.unwrap();
        assert_eq!(steering.len(), 1);
        assert_eq!(steering[0].item_id, steer.item_id);
        assert_eq!(
            inbox.pending_interrupt(&sid).await.unwrap().unwrap().item_id,
            interrupt.item_id
        );
        assert!(inbox.next_turn(&sid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn steering_drains_all_of_it_and_remembers_the_turn_it_is_for() {
        let (inbox, _, sid) = inbox();
        let turn_id = TurnId::new();
        inbox.steer(&sid, "first", None).await.unwrap();
        inbox.steer(&sid, "second", Some(&turn_id)).await.unwrap();

        // Both, not only the first: leaving the second for a boundary that may
        // never come would silently drop the user's words.
        let steering = inbox.steering(&sid).await.unwrap();
        assert_eq!(steering.len(), 2);
        let texts: Vec<&str> =
            steering.iter().map(|i| i.payload["text"].as_str().unwrap()).collect();
        assert_eq!(texts, vec!["first", "second"]);
        assert_eq!(steering[1].payload["expected_turn_id"], turn_id.as_str());
    }

    #[tokio::test]
    async fn a_rebuilt_inbox_still_knows_what_was_steered() {
        // Steering is durable like any other input, so a process that restarts
        // mid-turn resumes with the notes the user typed at the old one.
        let (inbox, store, sid) = inbox();
        inbox.steer(&sid, "keep going", None).await.unwrap();

        let rebuilt = Inbox::new(store);
        let steering = rebuilt.steering(&sid).await.unwrap();
        assert_eq!(steering.len(), 1);
        assert_eq!(steering[0].payload["text"], "keep going");
    }

    fn at(when: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(when).unwrap().with_timezone(&Utc)
    }

    #[tokio::test]
    async fn a_scheduled_item_is_not_a_turn_that_runs_now() {
        // The two queues are read by different decisions: `next_turn` is "the
        // user asked for a turn", `next_due` is "the time has come". A
        // scheduled item must not be picked up by the former.
        let (inbox, _, sid) = inbox();
        inbox.schedule(&sid, at("2026-10-18T10:00:00Z"), "later", 0, Actor::User).await.unwrap();

        assert!(inbox.next_turn(&sid).await.unwrap().is_none());
        assert_eq!(inbox.scheduled(&sid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn only_an_item_due_at_or_before_now_is_selected() {
        let (inbox, _, sid) = inbox();
        let future =
            inbox.schedule(&sid, at("2026-10-18T10:05:00Z"), "future", 0, Actor::User).await;

        let found = inbox.next_due(&sid, at("2026-10-18T10:04:59Z")).await.unwrap();
        assert!(matches!(found, Due::None));
        assert_eq!(inbox.scheduled(&sid).await.unwrap()[0].item_id, future.unwrap().item_id);

        let found = inbox.next_due(&sid, at("2026-10-18T10:05:00Z")).await.unwrap();
        assert!(matches!(found, Due::Due(_)), "due at exactly `now` counts");
    }

    #[tokio::test]
    async fn the_earliest_due_item_wins_and_ties_follow_queue_order() {
        let (inbox, _, sid) = inbox();
        let late =
            inbox.schedule(&sid, at("2026-10-18T10:10:00Z"), "late", 0, Actor::User).await.unwrap();
        let low_priority = inbox
            .schedule(&sid, at("2026-10-18T10:00:00Z"), "low priority", 5, Actor::User)
            .await
            .unwrap();
        // Same instant as the low-priority item; higher priority wins the tie
        // because it comes first in queue order.
        let high_priority = inbox
            .schedule(&sid, at("2026-10-18T10:00:00Z"), "high priority", 1, Actor::User)
            .await
            .unwrap();
        let now = at("2026-10-18T10:00:00Z");

        let first = match inbox.next_due(&sid, now).await.unwrap() {
            Due::Due(item) => item,
            other => panic!("expected a due item, got {other:?}"),
        };
        assert_eq!(first.item_id, high_priority.item_id);

        // Once it is claimed the tie falls to the next queue entry.
        inbox.claim(&sid, &first.item_id, &RunId::new()).await.unwrap();
        let second = match inbox.next_due(&sid, now).await.unwrap() {
            Due::Due(item) => item,
            other => panic!("expected a due item, got {other:?}"),
        };
        assert_eq!(second.item_id, low_priority.item_id);

        inbox.claim(&sid, &second.item_id, &RunId::new()).await.unwrap();
        assert!(
            matches!(inbox.next_due(&sid, now).await.unwrap(), Due::None),
            "the later item is not due yet"
        );
        match inbox.next_due(&sid, at("2026-10-18T10:10:00Z")).await.unwrap() {
            Due::Due(item) => assert_eq!(item.item_id, late.item_id),
            other => panic!("expected the later item once it is due, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unreadable_scheduled_item_is_reported_before_any_due_item() {
        // Its `due_at` cannot be compared, so it is not "not yet due" — it is
        // unreadable, and it would block the queue forever if left in place.
        let (inbox, _, sid) = inbox();
        inbox
            .append(
                &sid,
                InboxKind::Scheduled,
                serde_json::json!({ "due_at": "not a time", "text": "x" }),
                0,
                Actor::User,
            )
            .await
            .unwrap();
        inbox.schedule(&sid, at("2026-10-18T09:00:00Z"), "readable", 0, Actor::User).await.unwrap();

        match inbox.next_due(&sid, at("2026-10-18T10:00:00Z")).await.unwrap() {
            Due::Unreadable { reason, .. } => assert!(reason.contains("due_at"), "{reason}"),
            other => panic!("expected the bad item first, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_scheduled_item_without_usable_text_is_unreadable() {
        for payload in [
            serde_json::json!({ "due_at": "2026-10-18T09:00:00Z" }),
            serde_json::json!({ "due_at": "2026-10-18T09:00:00Z", "text": "" }),
            serde_json::json!({ "due_at": "2026-10-18T09:00:00Z", "text": "   " }),
            serde_json::json!({ "due_at": "2026-10-18T09:00:00Z", "text": 42 }),
        ] {
            let (inbox, _, sid) = inbox();
            inbox
                .append(&sid, InboxKind::Scheduled, payload.clone(), 0, Actor::User)
                .await
                .unwrap();
            match inbox.next_due(&sid, at("2026-10-18T10:00:00Z")).await.unwrap() {
                Due::Unreadable { reason, .. } => assert!(reason.contains("text"), "{reason}"),
                other => panic!("expected an unreadable item for {payload}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn schedule_refuses_a_blank_text() {
        let (inbox, _, sid) = inbox();
        assert!(matches!(
            inbox.schedule(&sid, at("2026-10-18T09:00:00Z"), "  ", 0, Actor::User).await,
            Err(InboxError::EmptyText)
        ));
        assert!(inbox.scheduled(&sid).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_schedule_never_becomes_due() {
        let (inbox, store, sid) = inbox();
        let item = inbox
            .schedule(&sid, at("2026-10-18T09:00:00Z"), "later", 0, Actor::User)
            .await
            .unwrap();

        inbox.cancel_scheduled(&sid, &item.item_id).await.unwrap();

        assert!(matches!(
            inbox.next_due(&sid, at("2026-10-18T10:00:00Z")).await.unwrap(),
            Due::None
        ));
        assert!(inbox.scheduled(&sid).await.unwrap().is_empty());

        // The record says who cancelled it and why, which is the difference
        // between a withdrawal and a scheduler discard.
        let events = store.read_stream(&sid, EventSeq::ZERO, usize::MAX).await.unwrap();
        let discarded = events
            .iter()
            .find(|e| e.event.event_type.as_str() == EventType::INBOX_DISCARDED)
            .expect("the cancellation is recorded");
        assert_eq!(discarded.event.actor, Actor::User);
        assert_eq!(discarded.event.payload["cause"], "cancelled");
    }

    #[tokio::test]
    async fn cancelling_a_non_scheduled_or_settled_item_is_refused() {
        let (inbox, _, sid) = inbox();
        let turn = inbox
            .append(&sid, InboxKind::NextTurn, serde_json::json!({"text": "hi"}), 0, Actor::User)
            .await
            .unwrap();
        assert!(matches!(
            inbox.cancel_scheduled(&sid, &turn.item_id).await,
            Err(InboxError::NotScheduled(_))
        ));

        let scheduled = inbox
            .schedule(&sid, at("2026-10-18T09:00:00Z"), "later", 0, Actor::User)
            .await
            .unwrap();
        inbox.claim(&sid, &scheduled.item_id, &RunId::new()).await.unwrap();
        assert!(matches!(
            inbox.cancel_scheduled(&sid, &scheduled.item_id).await,
            Err(InboxError::ItemNotPending(_))
        ));
    }

    #[tokio::test]
    async fn a_rebuilt_inbox_still_knows_what_is_scheduled() {
        let (inbox, store, sid) = inbox();
        inbox.schedule(&sid, at("2026-10-18T09:00:00Z"), "wake", 0, Actor::User).await.unwrap();

        let rebuilt = Inbox::new(store);
        match rebuilt.next_due(&sid, at("2026-10-18T10:00:00Z")).await.unwrap() {
            Due::Due(item) => assert_eq!(item.payload["text"], "wake"),
            other => panic!("expected the scheduled item to survive a rebuild, got {other:?}"),
        }
    }
}
