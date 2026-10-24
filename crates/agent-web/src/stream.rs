//! The session event stream.
//!
//! This is one SSE response per browser tab. Two sources are merged here:
//!
//! - **durable items** from the session log, folded one event at a time, so a
//!   tool row that started at seq 10 and finished at seq 11 is sent twice under
//!   the same id and the page upserts it;
//! - **live previews** from the process driving a turn, which never enter the
//!   log and may be dropped when a client is slow.
//!
//! The order of setup is the contract's: subscribe to the log *before* reading
//! it, then drop the replayed prefix. A turn that opens between those two steps
//! is therefore in the subscription as well as in the read, and the page sees it
//! exactly once.

use std::convert::Infallible;
use std::time::Duration;

use agent_ports::event_store::EventSubscription;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::PROTOCOL;
use crate::backend::{LiveChunk, LiveKind};
use crate::sessions::LiveTranscript;

/// The response stream is gone. Nothing else can make an SSE frame fail here:
/// the payload is JSON we built ourselves.
#[derive(Debug, Clone, Copy)]
struct SseError;

/// Build the response and start the pump.
///
/// `initial` is what a client at `since` has not seen; `transcript` already
/// watched the full log, so a replay from the subscription can be ignored by
/// sequence rather than by guessing from item ids.
pub(crate) fn response(
    session: String,
    head: u64,
    initial: Vec<Value>,
    mut transcript: LiveTranscript,
    mut events: Box<dyn EventSubscription>,
    mut live: broadcast::Receiver<LiveChunk>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Event>(256);

    tokio::spawn(async move {
        // Hello first: it carries the head the page should use if it has to
        // reconnect before the first item arrives.
        if send(
            &tx,
            "hello",
            json!({
                "v": PROTOCOL,
                "protocol": PROTOCOL,
                "session": session,
                "head": head,
            }),
        )
        .await
        .is_err()
        {
            return;
        }
        for item in initial {
            if send(&tx, "item", json!({ "v": PROTOCOL, "op": "upsert", "item": item }))
                .await
                .is_err()
            {
                return;
            }
        }

        let mut last_seq = head;
        let mut events_open = true;
        let mut live_open = true;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; the hello already told the client the
        // head, so skip it rather than sending two hellos in one breath.
        heartbeat.tick().await;

        loop {
            tokio::select! {
                _ = tx.closed() => break,

                event = events.next(), if events_open => match event {
                    Ok(Some(stored)) => {
                        // The subscription starts at zero so that a write made
                        // between subscribing and reading is not lost. Those
                        // writes are already in the fold; only later sequence
                        // numbers are news.
                        if stored.seq.as_u64() <= last_seq {
                            continue;
                        }
                        last_seq = stored.seq.as_u64();
                        match transcript.apply(&stored) {
                            Ok(items) => {
                                for item in items {
                                    if send(&tx, "item", json!({
                                        "v": PROTOCOL,
                                        "op": "upsert",
                                        "item": item,
                                    }))
                                    .await
                                    .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            Err(error) => {
                                let _ = send(&tx, "error", json!({
                                    "v": PROTOCOL,
                                    "code": "fold_failed",
                                    "message": error.to_string(),
                                }))
                                .await;
                            }
                        }
                    }
                    // No further durable event will ever arrive, but live text
                    // from a writer that already holds the log open can still
                    // be worth showing. Keep the connection for that.
                    Ok(None) => events_open = false,
                    Err(error) => {
                        // A read error is not an end (the store's contract).
                        // Say it once and give the subscription a breath rather
                        // than spinning on a disk that is saying no.
                        let _ = send(&tx, "error", json!({
                            "v": PROTOCOL,
                            "code": "read_failed",
                            "message": error.to_string(),
                        }))
                        .await;
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                },

                chunk = live.recv(), if live_open => match chunk {
                    Ok(chunk) => {
                        if send(&tx, "live", live_payload(chunk)).await.is_err() {
                            return;
                        }
                    }
                    // Slow client: a preview is exactly the thing that may be
                    // dropped. Durable items are still in the log and will be
                    // sent by the subscription above.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => live_open = false,
                },

                _ = heartbeat.tick() => {
                    if send(&tx, "heartbeat", json!({ "v": PROTOCOL, "head": last_seq })).await.is_err() {
                        return;
                    }
                }
            }

            if !events_open && !live_open {
                break;
            }
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok::<Event, Infallible>(event), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("爻爻"))
}

/// The wire shape of a live preview.
fn live_payload(chunk: LiveChunk) -> Value {
    json!({
        "v": PROTOCOL,
        "step": chunk.step,
        "kind": match chunk.kind {
            LiveKind::Text => "text",
            LiveKind::Reasoning => "reasoning",
            LiveKind::Notice => "notice",
        },
        "text": chunk.text,
    })
}

/// Send one event to the response stream, and say whether the client is still
/// there. `SseError` exists so a caller can use `?` without pretending an SSE
/// frame can fail for any reason other than the receiver being gone.
async fn send(tx: &mpsc::Sender<Event>, name: &'static str, data: Value) -> Result<(), SseError> {
    let event = Event::default().event(name).json_data(data).map_err(|_| SseError)?;
    tx.send(event).await.map_err(|_| SseError)
}
