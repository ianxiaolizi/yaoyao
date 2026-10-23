//! A repeatable performance baseline for the operations that scale with a
//! session.
//!
//! The plan's criterion for this stage is not "fast": it is "repeatable and
//! comparable". This binary therefore measures a fixed synthetic workload,
//! reports the middle sample of a fixed number of runs, and writes the numbers
//! as JSON so a later run can be diffed against them. It deliberately does not
//! fail on a speed threshold by default — a shared machine has noise, and a
//! gate that cries wolf gets ignored. `--strict` opts into the gate when a
//! caller wants one.
//!
//! What is measured, and why these five:
//!
//! - `append_batch_10k` — one `append` of 10,000 events. Serialization, the
//!   single write, and the store's in-memory bookkeeping all happen here.
//! - `append_single_1k` — 1,000 appends of one event. Every call also takes
//!   the lease, refreshes the tail, and writes; this is the per-call overhead
//!   that a batch amortizes away.
//! - `replay_10k` — a fresh store reads the 10,000-event log. This is the JSONL
//!   parse plus the per-line schema-version check.
//! - `fold_10k` — `Projection::build_full` folds that log into model context.
//!   The fold clones its state per event, so this is the workload whose cost
//!   grows with both event count and transcript size.
//! - `export_10k` — `run_export` renders both halves of the diagnostic bundle.
//!   It reads the whole stream and writes `session.md` and `bundle.json`.
//!
//! Not measured: `flush`, which makes acknowledged appends crash-durable. The
//! kernel flushes rarely (on resume paths), so timing it on every operation
//! would describe a workload the system does not have.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -p agent --example perf_baseline -- --save perf/baseline.json
//! cargo run --release -p agent --example perf_baseline -- --check perf/baseline.json
//! ```
//!
//! The three read-side workloads share one prebuilt 10,000-event log, kept in
//! the system temp directory and reused across processes. A log written anew
//! on every run was read from disk on some runs and from the page cache on
//! others — a freshly created file triggers host file scanning, and whether
//! that scan collides with the timed read is timing-dependent — so replay and
//! export swung by 10–30% between otherwise identical processes. Reusing an
//! existing file puts every run in the same state.
//!
//! Every workload carries a correctness sentinel — the event count read back,
//! the number of messages folded, the size of the exported bundle — so a
//! workload that accidentally measures nothing fails instead of reporting a
//! fast number. Compare numbers taken on the same host and build profile;
//! `--check` reports deltas and only fails when `--strict` is passed.

#![deny(unsafe_code)]

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_cli::run_export;
use agent_ports::event_store::EventStore;
use agent_projection::{ModelContextProjection, Projection};
use agent_protocol::events::{Actor, EventDraft, EventType};
use agent_protocol::ids::{
    AttemptId, EventId, EventSeq, InboxItemId, SchemaVersion, SessionId, TurnId,
};
use agent_protocol::inbox::{InboxItem, InboxItemStatus, InboxKind};
use agent_protocol::model::ModelResponse;
use agent_store_jsonl::JsonlEventStore;
use chrono::Utc;
use serde_json::{Value, json};

const SESSION_EVENTS: usize = 10_000;
const BATCH_EVENTS: usize = 10_000;
const SINGLE_EVENTS: usize = 1_000;

/// Fixed id of the session whose log the read-side workloads share.
const FIXTURE_SESSION: &str = "perf-baseline-fixture";

/// One workload's samples. Every sample is a full run of the operation, in
/// milliseconds; the reported figure is the median, because a single outlier
/// from a background process should not become "the baseline".
struct Reading {
    name: &'static str,
    events: u64,
    samples: Vec<Duration>,
}

impl Reading {
    fn new(name: &'static str, events: u64, samples: Vec<Duration>) -> Self {
        Self { name, events, samples }
    }

    fn sorted(&self) -> Vec<Duration> {
        let mut sorted = self.samples.clone();
        sorted.sort();
        sorted
    }

    fn median(&self) -> Duration {
        let sorted = self.sorted();
        sorted.get(sorted.len() / 2).copied().unwrap_or(Duration::ZERO)
    }

    /// The fastest run. Unlike the median it is not moved by one slow sample,
    /// but it can be a lucky one; both are recorded so a reader can see when
    /// they disagree.
    fn min(&self) -> Duration {
        self.sorted().first().copied().unwrap_or(Duration::ZERO)
    }

    fn p95(&self) -> Duration {
        let sorted = self.sorted();
        let index = (sorted.len() * 95).div_ceil(100).saturating_sub(1);
        sorted.get(index).copied().unwrap_or(Duration::ZERO)
    }

    fn events_per_second(&self, elapsed: Duration) -> f64 {
        self.events as f64 / elapsed.as_secs_f64()
    }

    fn median_events_per_second(&self) -> f64 {
        self.events_per_second(self.median())
    }

    fn min_events_per_second(&self) -> f64 {
        self.events_per_second(self.min())
    }
}

struct Options {
    save: Option<PathBuf>,
    check: Option<PathBuf>,
    label: String,
    iterations: usize,
    warmups: usize,
    strict: bool,
    tolerance: f64,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, Box<dyn Error>> {
        let mut options = Self {
            save: None,
            check: None,
            label: "local".to_string(),
            iterations: 5,
            warmups: 1,
            strict: false,
            tolerance: 20.0,
        };
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--save" => options.save = Some(PathBuf::from(value(&mut args, "--save")?)),
                "--check" => options.check = Some(PathBuf::from(value(&mut args, "--check")?)),
                "--label" => options.label = value(&mut args, "--label")?,
                "--iterations" => {
                    options.iterations = value(&mut args, "--iterations")?.parse()?;
                }
                "--warmups" => options.warmups = value(&mut args, "--warmups")?.parse()?,
                "--tolerance" => options.tolerance = value(&mut args, "--tolerance")?.parse()?,
                "--strict" => options.strict = true,
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown option `{other}`\n\n{USAGE}").into()),
            }
        }
        if options.iterations == 0 {
            return Err("--iterations must be at least 1".into());
        }
        Ok(options)
    }
}

fn value(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<String, Box<dyn Error>> {
    args.next().ok_or_else(|| format!("{flag} needs a value").into())
}

const USAGE: &str = "\
usage: perf_baseline [options]

  --save <path>        write this run's numbers as the baseline JSON
  --check <path>       compare this run against a saved baseline
  --label <name>       free-form machine label stored in the baseline (default: local)
  --iterations <n>     measured runs per workload (default: 5)
  --warmups <n>        unrecorded runs per workload (default: 1)
  --tolerance <pct>    regression tolerated by --strict (default: 20)
  --strict             exit non-zero when a workload is slower than the baseline

Run with --release: a debug build measures the debug profile, not the product.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse(std::env::args().skip(1))?;

    let root = std::env::temp_dir().join(format!(
        "yaoyao-perf-{}-{}",
        std::process::id(),
        Utc::now().timestamp_millis()
    ));
    std::fs::create_dir_all(&root)?;

    let result = run(&root, &options).await;
    let _ = std::fs::remove_dir_all(&root);
    result
}

async fn run(root: &Path, options: &Options) -> Result<(), Box<dyn Error>> {
    // A fixed session id and a fixed fixture location, so the same log file is
    // reused across processes. A log written anew on every run was read from
    // disk on some runs and from the page cache on others — the freshly
    // created file triggers host file scanning, and whether that scan collides
    // with the timed read is timing-dependent. Reusing an existing file puts
    // every run in the same state and made replay stable across processes.
    let session = SessionId::from_string(FIXTURE_SESSION);
    let log_root = std::env::temp_dir().join(format!("yaoyao-perf-fixture-{SESSION_EVENTS}"));
    prepare_fixture(&log_root, &session).await?;

    let mut readings = vec![
        append_batch(root, &session, options).await?,
        append_single(root, &session, options).await?,
    ];
    readings.push(replay(&log_root, &session, options).await?);
    readings.push(fold(&log_root, root, &session, options).await?);
    readings.push(export(&log_root, root, &session, options).await?);

    println!(
        "perf baseline — {} / {} / {} ({} measured, {} warmup)",
        options.label,
        std::env::consts::OS,
        std::env::consts::ARCH,
        options.iterations,
        options.warmups
    );
    println!(
        "{:<20} {:>10} {:>10} {:>14} {:>10}",
        "workload", "median ms", "min ms", "events/s", "spread"
    );
    for reading in &readings {
        let median = reading.median();
        let p95 = reading.p95();
        let spread = p95.as_secs_f64() / median.as_secs_f64();
        println!(
            "{:<20} {:>10.3} {:>10.3} {:>14.0} {:>9.2}x",
            reading.name,
            median.as_secs_f64() * 1000.0,
            reading.min().as_secs_f64() * 1000.0,
            reading.median_events_per_second(),
            spread
        );
    }

    if let Some(path) = &options.check {
        compare(path, &readings, options)?;
    }
    if let Some(path) = &options.save {
        save(path, &readings, options)?;
        println!("\nbaseline written to {}", path.display());
    }
    Ok(())
}

/// Ensure the shared fixture log exists and is complete.
///
/// The fixture is harness-owned cache in the system temp directory, not user
/// data. A complete log is reused as is; anything else is rebuilt: a run that
/// was killed mid-write, or a fixture from a build with a different event
/// count, would otherwise make the sentinels fail with a confusing count.
async fn prepare_fixture(log_root: &Path, session: &SessionId) -> Result<(), Box<dyn Error>> {
    let file = log_root.join(session.as_str()).join("events.jsonl");
    let complete = std::fs::read_to_string(&file)
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count() == SESSION_EVENTS)
        .unwrap_or(false);
    if complete {
        return Ok(());
    }
    if log_root.exists() {
        std::fs::remove_dir_all(log_root)?;
    }
    let store = JsonlEventStore::new(log_root);
    let drafts = transcript(session, SESSION_EVENTS)?;
    store.append(session, EventSeq::ZERO, drafts).await?;
    Ok(())
}

/// The synthetic transcript the read-side workloads share.
///
/// A turn is three events: the item enters the inbox, the turn starts and
/// consumes it, the model answers. Event count is the unit under measurement,
/// so a trailing partial turn is padded with `session/started` — a core event
/// the projection skips — to keep the log exactly `events` long.
fn transcript(session: &SessionId, events: usize) -> Result<Vec<EventDraft>, serde_json::Error> {
    let mut drafts = Vec::with_capacity(events);
    while drafts.len() + 3 <= events {
        let item = InboxItem {
            item_id: InboxItemId::new(),
            session_id: session.clone(),
            kind: InboxKind::NextTurn,
            payload: json!({ "text": "a question long enough to look like a real one" }),
            priority: 0,
            created_at: Utc::now(),
            status: InboxItemStatus::Pending,
        };
        let turn_id = TurnId::new();
        let attempt_id = AttemptId::new();
        drafts.push(draft(
            session,
            EventType::INBOX_APPENDED,
            Actor::User,
            None,
            serde_json::to_value(&item)?,
        ));
        drafts.push(draft(
            session,
            EventType::TURN_STARTED,
            Actor::Scheduler,
            Some(turn_id.clone()),
            json!({ "inbox_item": item.item_id.as_str() }),
        ));
        drafts.push(draft(
            session,
            EventType::MODEL_RESPONSE_COMPLETED,
            Actor::Agent,
            Some(turn_id),
            json!({
                "attempt_id": attempt_id.as_str(),
                "response": serde_json::to_value(ModelResponse::text("the answer for this turn"))?,
            }),
        ));
    }
    while drafts.len() < events {
        drafts.push(draft(session, EventType::SESSION_STARTED, Actor::System, None, json!({})));
    }
    Ok(drafts)
}

fn draft(
    session: &SessionId,
    event_type: &str,
    actor: Actor,
    turn_id: Option<TurnId>,
    payload: Value,
) -> EventDraft {
    EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session.clone(),
        run_id: None,
        turn_id,
        step_id: None,
        actor,
        created_at: Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: json!({}),
        ignorable: false,
    }
}

async fn append_batch(
    root: &Path,
    session: &SessionId,
    options: &Options,
) -> Result<Reading, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(options.iterations);
    for run in 0..options.warmups + options.iterations {
        let dir = root.join(format!("append-batch-{run}"));
        let store = JsonlEventStore::new(&dir);
        let drafts = transcript(session, BATCH_EVENTS)?;
        let start = Instant::now();
        store.append(session, EventSeq::ZERO, drafts).await?;
        let elapsed = start.elapsed();

        let read = store.read_stream(session, EventSeq::ZERO, usize::MAX).await?;
        if read.len() != BATCH_EVENTS {
            return Err(format!(
                "append_batch read back {} events, expected {BATCH_EVENTS}",
                read.len()
            )
            .into());
        }
        if run >= options.warmups {
            samples.push(elapsed);
        }
    }
    Ok(Reading::new("append_batch_10k", BATCH_EVENTS as u64, samples))
}

async fn append_single(
    root: &Path,
    session: &SessionId,
    options: &Options,
) -> Result<Reading, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(options.iterations);
    for run in 0..options.warmups + options.iterations {
        let dir = root.join(format!("append-single-{run}"));
        let store = JsonlEventStore::new(&dir);
        let drafts = transcript(session, SINGLE_EVENTS)?;
        let start = Instant::now();
        for (seq, draft) in drafts.into_iter().enumerate() {
            store.append(session, EventSeq::new(seq as u64), vec![draft]).await?;
        }
        let elapsed = start.elapsed();

        let read = store.read_stream(session, EventSeq::ZERO, usize::MAX).await?;
        if read.len() != SINGLE_EVENTS {
            return Err(format!(
                "append_single read back {} events, expected {SINGLE_EVENTS}",
                read.len()
            )
            .into());
        }
        if run >= options.warmups {
            samples.push(elapsed);
        }
    }
    Ok(Reading::new("append_single_1k", SINGLE_EVENTS as u64, samples))
}

async fn replay(
    log_root: &Path,
    session: &SessionId,
    options: &Options,
) -> Result<Reading, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(options.iterations);
    for run in 0..options.warmups + options.iterations {
        // A new store per run: the first `read_stream` is what parses the log,
        // so reusing one store would measure a memory read instead.
        let store = JsonlEventStore::new(log_root);
        let start = Instant::now();
        let events = store.read_stream(session, EventSeq::ZERO, usize::MAX).await?;
        let elapsed = start.elapsed();

        if events.len() != SESSION_EVENTS {
            return Err(
                format!("replay read {} events, expected {SESSION_EVENTS}", events.len()).into()
            );
        }
        if run >= options.warmups {
            samples.push(elapsed);
        }
    }
    Ok(Reading::new("replay_10k", SESSION_EVENTS as u64, samples))
}

async fn fold(
    log_root: &Path,
    root: &Path,
    session: &SessionId,
    options: &Options,
) -> Result<Reading, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(options.iterations);
    for run in 0..options.warmups + options.iterations {
        let store: Arc<dyn EventStore> = Arc::new(JsonlEventStore::new(log_root));
        let projection = Projection::new(
            ModelContextProjection::new(),
            store,
            root.join(format!("checkpoints-{run}")),
        );
        let start = Instant::now();
        let projected = projection.build_full(session).await?;
        let elapsed = start.elapsed();

        let messages = projection.view(&projected.state);
        // Each turn contributes two model-visible messages: the user's text
        // (at `turn/started`) and the assistant's answer (at
        // `model/response.completed`). The trailing pad event is core traffic
        // the fold skips.
        let expected_messages = (SESSION_EVENTS / 3) * 2;
        if messages.len() != expected_messages {
            return Err(format!(
                "fold produced {} messages, expected {expected_messages}",
                messages.len()
            )
            .into());
        }
        let expected_seq = EventSeq::new(SESSION_EVENTS as u64 - 1);
        if projected.as_of_seq != Some(expected_seq) {
            return Err(format!(
                "fold stopped at {:?}, expected {expected_seq:?}",
                projected.as_of_seq
            )
            .into());
        }
        if run >= options.warmups {
            samples.push(elapsed);
        }
    }
    Ok(Reading::new("fold_10k", SESSION_EVENTS as u64, samples))
}

async fn export(
    log_root: &Path,
    root: &Path,
    session: &SessionId,
    options: &Options,
) -> Result<Reading, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(options.iterations);
    for run in 0..options.warmups + options.iterations {
        let out = root.join(format!("export-{run}"));
        let out = out.to_str().ok_or("the temporary export path is not UTF-8")?;
        let store = JsonlEventStore::new(log_root);
        let start = Instant::now();
        run_export(&store, session, Some(out), false, false).await?;
        let elapsed = start.elapsed();

        let bundle: Value =
            serde_json::from_str(&std::fs::read_to_string(Path::new(out).join("bundle.json"))?)?;
        let events =
            bundle["events"].as_array().ok_or("the exported bundle has no events array")?;
        if events.len() != SESSION_EVENTS {
            return Err(
                format!("export wrote {} events, expected {SESSION_EVENTS}", events.len()).into()
            );
        }
        if run >= options.warmups {
            samples.push(elapsed);
        }
    }
    Ok(Reading::new("export_10k", SESSION_EVENTS as u64, samples))
}

fn save(path: &Path, readings: &[Reading], options: &Options) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let workloads: Vec<Value> = readings
        .iter()
        .map(|reading| {
            json!({
                "name": reading.name,
                "events": reading.events,
                "unit": "events_per_second",
                "median_ms": reading.median().as_secs_f64() * 1000.0,
                "min_ms": reading.min().as_secs_f64() * 1000.0,
                "p95_ms": reading.p95().as_secs_f64() * 1000.0,
                "events_per_second": reading.median_events_per_second(),
                "min_events_per_second": reading.min_events_per_second(),
                "samples_ms": reading
                    .samples
                    .iter()
                    .map(|sample| sample.as_secs_f64() * 1000.0)
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let baseline = json!({
        "schema_version": "1",
        "generated_at": Utc::now().to_rfc3339(),
        "label": options.label,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "iterations": options.iterations,
        "warmups": options.warmups,
        "workloads": workloads,
    });
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&baseline)?))?;
    Ok(())
}

fn compare(path: &Path, readings: &[Reading], options: &Options) -> Result<(), Box<dyn Error>> {
    let baseline: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let workloads =
        baseline["workloads"].as_array().ok_or("the baseline has no workloads array")?;

    println!("\nagainst {} (label: {})", path.display(), baseline["label"]);
    println!("{:<20} {:>14} {:>14} {:>10}", "workload", "baseline ev/s", "current ev/s", "delta");
    let mut regressions = Vec::new();
    for reading in readings {
        let Some(saved) =
            workloads.iter().find(|entry| entry["name"].as_str() == Some(reading.name))
        else {
            println!("{:<20} {:>14} {:>14} {:>10}", reading.name, "missing", "-", "new");
            continue;
        };
        let Some(saved_rate) = saved["events_per_second"].as_f64() else {
            return Err(format!("baseline entry for {} has no rate", reading.name).into());
        };
        let current_rate = reading.median_events_per_second();
        let delta = (current_rate - saved_rate) / saved_rate * 100.0;
        println!(
            "{:<20} {:>14.0} {:>14.0} {:>9.1}%",
            reading.name, saved_rate, current_rate, delta
        );
        if delta < -options.tolerance {
            regressions.push(format!("{} {delta:.1}%", reading.name));
        }
    }

    if options.strict && !regressions.is_empty() {
        return Err(format!(
            "slower than the baseline beyond {:.1}%: {}",
            options.tolerance,
            regressions.join(", ")
        )
        .into());
    }
    Ok(())
}
