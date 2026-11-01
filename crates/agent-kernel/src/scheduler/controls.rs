//! Applying the controls waiting at a step boundary.
//!
//! Split out of the scheduler because the scheduler file is at its size ratchet
//! and a slice may not grow it back into a monolith. The behaviour is unchanged:
//! this is the same method, moved.

use std::collections::BTreeSet;

use agent_ports::control::{ControlError, ControlOutcome, ControlRequest};
use agent_protocol::ids::SessionId;

use super::{DiscardCause, Scheduler, SchedulerError};

impl Scheduler {
    /// Apply the changes waiting at this boundary, one at a time, in order.
    ///
    /// Each item is decided before it is recorded, because the record is the
    /// decision: a control that took effect is *claimed* — the queue's word for
    /// "taken out, and not by a run" — and one that did not is *discarded with
    /// the reason*. Recording first and deciding after would make a refused
    /// control look exactly like an applied one to anyone reading the log.
    ///
    /// The order matters for the failure it leaves behind: the handler acts
    /// before the claim is written, so a process that dies in between leaves the
    /// item pending and the change already made. Applying it again is then
    /// harmless — the second attempt refuses, because there is nothing left to
    /// do — which is why this is safe to run on every boundary rather than
    /// exactly once.
    pub(crate) async fn apply_controls(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SchedulerError> {
        for item in self.inbox.controls(session_id).await? {
            let control = item
                .payload
                .get("control")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let handler = self.controls.iter().find(|h| h.handles().contains(&control));

            // The three ways a control can fail to take effect are different
            // facts — nobody claimed it, a handler declined, a handler broke —
            // and the port says they are recorded differently. The cause is
            // what carries that distinction into the log; the sentence beside
            // it is for the person reading (C-1).
            let refusal = match handler {
                None => Some((DiscardCause::Unhandled, self.nothing_handles(&control))),
                Some(handler) => {
                    let request = ControlRequest {
                        session_id: session_id.clone(),
                        item_id: item.item_id.clone(),
                        control: control.clone(),
                        payload: item.payload.clone(),
                    };
                    match handler.apply(request).await {
                        Ok(ControlOutcome::Applied { .. }) => None,
                        Ok(ControlOutcome::Refused { reason }) => Some((
                            DiscardCause::Refused,
                            format!("{} refused it: {reason}", handler.name()),
                        )),
                        Err(ControlError::Failed { message, .. }) => Some((
                            DiscardCause::Failed,
                            format!("{} could not apply it: {message}", handler.name()),
                        )),
                    }
                }
            };

            match refusal {
                // Applied: the change is already made and recorded by whoever
                // made it. What is left is to take the item out of the queue.
                None => {
                    // Claim and append are one step, like every other writer
                    // here: the item is re-read to build the claim, and the
                    // append must land on the log that read saw. (YA-36)
                    let _lease = self.store.lease(session_id).await?;
                    let draft = self.inbox.claim_draft(session_id, &item.item_id, None).await?;
                    let next = self.next_seq(session_id).await?;
                    self.store.append(session_id, next, vec![draft]).await?;
                }
                Some((cause, reason)) => {
                    self.inbox.discard(session_id, &item.item_id, cause, Some(&reason)).await?;
                }
            }
        }
        Ok(())
    }

    /// What to say when nobody claims a control.
    ///
    /// A refusal that only said "no" would send the operator to read the source;
    /// naming the handlers is what lets them see that the control they sent is
    /// one this build does not have — or that it has none at all.
    fn nothing_handles(&self, control: &str) -> String {
        let handled: Vec<String> = self
            .controls
            .iter()
            .flat_map(|h| h.handles())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if handled.is_empty() {
            format!("no control '{control}' can be applied: this run registered no handlers")
        } else {
            format!(
                "no handler claims control '{control}'; this run handles: {}",
                handled.join(", ")
            )
        }
    }
}
