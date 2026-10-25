//! A durable, append-only JSONL `EventStore`.
//!
//! Layout: `<root>/<session_id>/events.jsonl`, one JSON-encoded `StoredEvent`
//! per line, plus `<root>/<session_id>/events.lock` holding the writer lease.
//! The file is the source of truth; the store keeps a copy of what it has read
//! so a read never has to rescan the log, and re-reads the tail whenever a
//! reader asks, so an append made by another process becomes visible without a
//! reopen.
//!
//! Durability: `append` writes lines but does not fsync; `flush` calls
//! `sync_all` to make prior appends crash-durable. A torn trailing line (a
//! write that crashed before reaching a newline) is truncated on reopen, and by
//! the next `append` — a session already open in a live process never gets that
//! reopen, and appending onto the fragment would splice the two into one line
//! no reader can parse.
//!
//! Writer exclusion: `append` holds the session lease for the length of the
//! write, so two *processes* cannot interleave appends or reuse a sequence.
//! Within one process, same-session callers queue on an in-process mutex rather
//! than erroring. The lease is also available directly, for callers whose
//! critical section is wider than a write — resuming an approved tool call runs
//! a side effect before it appends anything, and a lease held across that
//! sequence is what makes the effect happen once rather than twice.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_channel::mpsc;
use futures_util::StreamExt;
use futures_util::future::{self, Either};

use agent_ports::event_store::{
    AppendReceipt, EventStore, EventStoreError, EventSubscription, FlushReceipt, SessionLease,
    is_safe_dir_name,
};
use agent_protocol::events::{EventDraft, StoredEvent};
use agent_protocol::ids::{EventSeq, SessionId};
use agent_protocol::versioning;
use chrono::Utc;

/// How long `append` waits for the lease before giving up.
///
/// Short on purpose, and the shortness is not a preference. An `append` carries
/// an `expected_next_seq` read before the wait, so a writer that only got the
/// lease after a long wait would find the log moved and fail with
/// `SequenceConflict` anyway — waiting longer cannot turn a refusal into a
/// write, only into a less clear error. And the lease an append takes covers a
/// single buffered write, so the wait only has to outlast another process's
/// append, which is orders of magnitude less than this.
///
/// The wide wait belongs to the explicit [`EventStore::lease`], whose contract
/// is to be held across a side effect and so across an unknown stretch of time;
/// that one is the store's to set ([`JsonlEventStore::with_lease_wait`]).
const LEASE_APPEND_WAIT: Duration = Duration::from_millis(200);

/// How often a wait looks again while a peer holds the lease.
const LEASE_RETRY: Duration = Duration::from_millis(20);

/// How long a delete keeps trying to remove a directory whose files are still
/// open elsewhere.
///
/// The usual holder is this process's own stream for the session being deleted:
/// the client is told to go away by the same request, but the socket closes a
/// moment later. A second is long enough for that and short enough that a real
/// sharing violation is still reported instead of hidden behind a wait.
const DELETE_RETRY: Duration = Duration::from_millis(50);
const DELETE_RETRIES: usize = 20;

/// How long a live subscription waits for a signal before re-reading the log
/// anyway.
///
/// Only this process's appends can raise a signal, so without this a
/// subscription would answer exclusively from its own writes — losing, on the
/// one path that claims to watch the log, the cross-process visibility
/// `refresh` exists to provide. The cost is one wake-up every interval while
/// idle; the gain is that a foreign append arrives within it instead of never.
const DEFAULT_SUBSCRIBE_POLL: Duration = Duration::from_millis(250);

/// Durable JSONL `EventStore` rooted at a directory.
pub struct JsonlEventStore {
    root: PathBuf,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    leases: Arc<LeaseTable>,
    wakeups: Wakeups,
    poll: Duration,
    /// How long an explicit `lease()` waits for a peer process, or `None` for as
    /// long as it takes. Not used by `append`, which has its own short wait.
    lease_wait: Option<Duration>,
}

/// Wake signals for this process's live subscriptions, one list per session.
///
/// A signal carries nothing: it says "look at the log again". Keeping the log
/// the only source of order and content is what lets a subscriber read a batch
/// and be sure it is neither reordered nor duplicated by the notification path.
/// The queue is one deep and a signal that cannot be queued is dropped; a
/// subscriber that already holds a pending signal will read the new event on the
/// same pass, so dropping it loses nothing and a subscriber that stops reading
/// does not make the store grow.
#[derive(Default)]
struct Wakeups {
    sessions: Mutex<HashMap<String, Vec<mpsc::Sender<()>>>>,
}

impl Wakeups {
    fn register(&self, key: &str, tx: mpsc::Sender<()>) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        // A subscription that was dropped without ever being woken leaves a
        // sender behind; this is where most of them get collected.
        let list = sessions.entry(key.to_string()).or_default();
        list.retain(|tx| !tx.is_closed());
        list.push(tx);
    }

    fn wake(&self, key: &str) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        let Some(list) = sessions.get_mut(key) else {
            return;
        };
        list.retain_mut(|tx| match tx.try_send(()) {
            Ok(()) => true,
            // Full means a signal is already waiting there, which is enough;
            // disconnected means nobody is listening any more.
            Err(e) => e.is_full(),
        });
    }
}

/// A live view of one session's log.
///
/// Two things make an event visible: this process appending to the session,
/// which raises a signal, and the log having grown for any other reason, which
/// cannot — so it is polled. Reads always go to the log, never to a signal, so
/// the same event cannot arrive twice and a signal that was coalesced away
/// cannot take an event with it.
struct LiveSubscription {
    session: Arc<Session>,
    /// Sequence expected next; events strictly below it have been yielded.
    next_expected: u64,
    buffered: VecDeque<StoredEvent>,
    wake: mpsc::Receiver<()>,
    poll: Duration,
}

struct Session {
    inner: Mutex<SessionInner>,
}

struct SessionInner {
    /// Append handle. Kept open for the life of the process.
    file: File,
    /// Separate read handle for the tail refresh. Mixing reads into the append
    /// handle would mean sharing a cursor with writes that ignore it.
    read: File,
    /// The log these handles are open on. Kept so a torn tail can be shortened
    /// through a handle that is allowed to do it — see [`JsonlEventStore::drop_torn_tail`].
    path: PathBuf,
    /// Byte offset of the last complete line this process has accounted for.
    /// Always sits on a line boundary, so a partially written line is re-read
    /// whole rather than half-parsed.
    read_offset: u64,
    events: Vec<StoredEvent>,
    seen: HashMap<String, EventSeq>,
    next_seq: u64,
    /// Bytes this process dropped from a torn trailing fragment, across the
    /// open and any later append-side repair. See
    /// [`JsonlEventStore::repaired_tail`].
    repaired_tail: Option<u64>,
}

/// What replaying one log found.
struct Replay {
    /// Durable events in seq order.
    events: Vec<StoredEvent>,
    /// The `event_id -> seq` index.
    seen: HashMap<String, EventSeq>,
    /// The next sequence to assign.
    next_seq: u64,
    /// The offset of the last complete line.
    read_offset: u64,
    /// Bytes dropped from a torn trailing fragment during this replay.
    repaired_tail: Option<u64>,
}

/// Cross-process writer exclusion, one lock file per session.
struct LeaseTable {
    root: PathBuf,
    sessions: Mutex<HashMap<String, Arc<SessionLock>>>,
}

struct SessionLock {
    path: PathBuf,
    state: Mutex<LockState>,
}

struct LockState {
    /// `Some` exactly while the lock file is held. Dropping the handle releases
    /// the lock, which is why the lease needs no explicit close path.
    file: Option<File>,
    /// How many callers in this process hold it. Above one, a caller is nested
    /// inside another's critical section and must not try to lock again.
    depth: usize,
}

/// Releases the session lease when dropped.
struct LeaseGuard {
    table: Arc<LeaseTable>,
    key: String,
    session_id: SessionId,
}

impl SessionLease for LeaseGuard {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.table.release(&self.key);
    }
}

impl LeaseTable {
    fn new(root: PathBuf) -> Self {
        Self { root, sessions: Mutex::new(HashMap::new()) }
    }

    fn lock_for(&self, key: &str) -> Arc<SessionLock> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .entry(key.to_string())
            .or_insert_with(|| {
                Arc::new(SessionLock {
                    path: self.root.join(key).join("events.lock"),
                    state: Mutex::new(LockState { file: None, depth: 0 }),
                })
            })
            .clone()
    }

    /// Take the lease, or report that another process is holding it.
    ///
    /// Returns the guard that gives it back: the lease is held for exactly as
    /// long as the guard lives, so there is no path that acquires without
    /// releasing. Within one process the per-session state mutex is held across
    /// the wait, which is what makes same-process callers queue instead of
    /// competing for the file lock — the second one blocks on the mutex and
    /// proceeds when the first lets go.
    ///
    /// `wait` is how long to hold out for a peer process, and it is the caller's
    /// to state because only the caller knows what it is waiting to do: an
    /// `append` is waiting to write one buffered line and gives up quickly, a
    /// caller of [`EventStore::lease`] may be waiting behind a tool that runs
    /// for minutes. `None` waits until the peer lets go, which is not a hang:
    /// the lock is released by the kernel when the holder exits, so `None` only
    /// ever waits out a holder that is still alive and still working.
    fn acquire(
        self: &Arc<Self>,
        session_id: &SessionId,
        wait: Option<Duration>,
    ) -> Result<LeaseGuard, EventStoreError> {
        let key = session_id.as_str().to_string();
        let lock = self.lock_for(&key);
        let mut state = lock.state.lock().unwrap_or_else(|e| e.into_inner());

        if state.depth == 0 {
            // The lock file's contents are nothing and are never written; the
            // handle exists to be locked. `truncate(false)` is required rather
            // than incidental: truncating would be a destructive write to a file
            // another process may be holding the lock on.
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&lock.path)?;
            let mut waited = Duration::ZERO;
            loop {
                match file.try_lock() {
                    Ok(()) => break,
                    Err(TryLockError::WouldBlock) => {
                        // Checked before the sleep, so a zero wait is a single
                        // attempt: a caller that wants to be told immediately
                        // gets "someone else has it" rather than one retry.
                        if wait.is_some_and(|wait| waited >= wait) {
                            return Err(EventStoreError::ConcurrentWriter(session_id.clone()));
                        }
                        std::thread::sleep(LEASE_RETRY);
                        waited += LEASE_RETRY;
                    }
                    Err(TryLockError::Error(e)) => return Err(e.into()),
                }
            }
            state.file = Some(file);
        }
        state.depth += 1;

        Ok(LeaseGuard { table: self.clone(), key, session_id: session_id.clone() })
    }

    fn release(&self, key: &str) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(lock) = sessions.get(key) else {
            return;
        };
        let mut state = lock.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.depth == 0 {
            return;
        }
        state.depth -= 1;
        if state.depth == 0 {
            // Closing the handle releases the lock; the explicit unlock is for
            // the platforms where that is worth doing before the close, and a
            // failure here has nothing left to report to.
            if let Some(file) = state.file.take() {
                let _ = file.unlock();
            }
        }
    }
}

impl JsonlEventStore {
    /// Create a store whose per-session directories live under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            leases: Arc::new(LeaseTable::new(root.clone())),
            root,
            sessions: Mutex::new(HashMap::new()),
            wakeups: Wakeups::default(),
            poll: DEFAULT_SUBSCRIBE_POLL,
            lease_wait: None,
        }
    }

    /// How long a subscription waits for a signal before re-reading the log.
    ///
    /// Exposed because it is the whole cross-process latency budget of a live
    /// subscription: a test that wants to observe a foreign append without
    /// waiting a quarter of a second asks for a shorter one, and it should not
    /// have to sleep through the production value to do it.
    pub fn with_poll_interval(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// How long an explicit `lease()` waits for a peer process before refusing.
    ///
    /// Unset, the wait is unbounded, which is the right default for an explicit
    /// `lease()`: its contract is to be held across a side effect — a resumed
    /// tool call runs before anything is appended — so the holder's work has no
    /// predictable length, and any fixed bound would refuse a writer that only
    /// needed to wait for a peer to finish. Waiting is safe because the kernel
    /// releases an advisory lock when its holder exits, so an unbounded wait can
    /// only ever be spent on a holder that is alive and working.
    ///
    /// A caller that would rather be refused than wait sets one. So does a test:
    /// the refusal is the thing under assertion, and it should not have to wait
    /// out a peer to observe it.
    pub fn with_lease_wait(mut self, wait: Duration) -> Self {
        self.lease_wait = Some(wait);
        self
    }

    fn poisoned() -> EventStoreError {
        EventStoreError::Io(std::io::Error::other("session lock poisoned"))
    }

    /// How many bytes of a torn trailing fragment this process has dropped from
    /// the session's log, or `None` when it has dropped none.
    ///
    /// Opening the log is what performs the repair, so this opens the session if
    /// it is not already open and reports what that open did. The fact lives
    /// with the open, not in the file: a torn fragment is not a record anyone
    /// can read back, so a repair performed by another process — or by an
    /// earlier, now-exited one — is not visible here. That is the limit ST-8
    /// asks to be stated rather than hidden.
    ///
    /// A caller that must not race a live writer holds the session lease first:
    /// `replay` has no way to tell a dead writer's fragment from one being
    /// written right now.
    pub fn repaired_tail(&self, session_id: &SessionId) -> Result<Option<u64>, EventStoreError> {
        let session = self.open(session_id)?;
        let inner = session.inner.lock().map_err(|_| Self::poisoned())?;
        Ok(inner.repaired_tail)
    }

    fn session_dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Open (or create) the session, replaying its file on first use.
    fn open(&self, session_id: &SessionId) -> Result<Arc<Session>, EventStoreError> {
        let key = session_id.as_str();
        if !is_safe_dir_name(key) {
            return Err(EventStoreError::unsafe_session_id());
        }

        let mut sessions = self.sessions.lock().map_err(|_| Self::poisoned())?;
        if let Some(session) = sessions.get(key) {
            return Ok(session.clone());
        }

        let inner = Self::load(&self.session_dir(key))?;
        let session = Arc::new(Session { inner: Mutex::new(inner) });
        sessions.insert(key.to_string(), session.clone());
        Ok(session)
    }

    /// Create the session directory, replay `events.jsonl`, and open it for
    /// both append and read.
    fn load(dir: &Path) -> Result<SessionInner, EventStoreError> {
        fs::create_dir_all(dir)?;
        let path = dir.join("events.jsonl");
        let replay = Self::replay(&path)?;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let read = OpenOptions::new().read(true).open(&path)?;
        Ok(SessionInner {
            file,
            read,
            path,
            read_offset: replay.read_offset,
            events: replay.events,
            seen: replay.seen,
            next_seq: replay.next_seq,
            repaired_tail: replay.repaired_tail,
        })
    }

    /// Replay `events.jsonl`, truncating any torn trailing fragment.
    ///
    /// Returns the durable events in seq order, the `event_id -> seq` index, the
    /// next sequence to assign, the offset just past the last complete line, and
    /// how many bytes of a torn fragment this replay dropped. A complete line
    /// that fails to parse, or a non-contiguous seq, is treated as corrupt
    /// history and fails closed.
    ///
    /// The dropped-byte count is a **diagnostic**: the repair itself is
    /// unchanged, but a caller that wants to know a crash left a half-written
    /// record can ask instead of inferring it from a file that no longer holds
    /// the evidence (ST-8).
    ///
    /// Dropping a torn fragment here is safe because this runs before anyone can
    /// be reading the file — but this is not the *only* place that may do it.
    /// [`Self::drop_torn_tail`] does the same thing from `append`, on the same
    /// grounds (the lease), and for a reason this path cannot serve: a session
    /// already open in a live process never comes back through here. See the
    /// note at that function.
    fn replay(path: &Path) -> Result<Replay, EventStoreError> {
        let mut bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Replay {
                    events: Vec::new(),
                    seen: HashMap::new(),
                    next_seq: 0,
                    read_offset: 0,
                    repaired_tail: None,
                });
            }
            Err(e) => return Err(e.into()),
        };

        // A write that crashed before its newline leaves a torn fragment; drop it.
        let complete_len = complete_len(&bytes);
        let repaired_tail = if complete_len < bytes.len() {
            let dropped = (bytes.len() - complete_len) as u64;
            bytes.truncate(complete_len);
            truncate_to(path, complete_len as u64)?;
            Some(dropped)
        } else {
            None
        };

        let mut events: Vec<StoredEvent> = Vec::new();
        for line in bytes.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            events.push(decode_stored(line)?);
        }

        for (idx, event) in events.iter().enumerate() {
            if event.seq.as_u64() != idx as u64 {
                return Err(EventStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "non-contiguous seq in events.jsonl",
                )));
            }
        }

        let next_seq = events.len() as u64;
        let seen = events.iter().map(|e| (e.event.event_id.to_string(), e.seq)).collect();
        Ok(Replay { events, seen, next_seq, read_offset: complete_len as u64, repaired_tail })
    }

    /// Pick up any lines appended since the last look, by this process or by
    /// another one.
    ///
    /// Only complete lines are taken. A line still being written is left alone
    /// for a later read rather than reported torn or repaired from here: a
    /// reader that truncated another process's half-written record would be
    /// deleting a fact it is about to need.
    fn refresh(inner: &mut SessionInner) -> Result<(), EventStoreError> {
        let len = inner.file.metadata()?.len();
        if len == inner.read_offset {
            return Ok(());
        }
        if len < inner.read_offset {
            // The log shrank under a reader. Nothing in this store truncates
            // complete lines, so this is not a state to guess at.
            return Err(EventStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "events.jsonl shrank while open",
            )));
        }

        let mut buf = vec![0u8; (len - inner.read_offset) as usize];
        inner.read.seek(SeekFrom::Start(inner.read_offset))?;
        inner.read.read_exact(&mut buf)?;

        let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
            return Ok(());
        };
        let complete = &buf[..=last_nl];

        for line in complete.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let stored = decode_stored(line)?;
            // A gap means someone wrote without the lease, or the log was
            // edited. Either way the sequence this process would assign next is
            // no longer the one the log expects, so stop rather than paper over.
            if stored.seq.as_u64() != inner.next_seq {
                return Err(EventStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "non-contiguous seq appended to events.jsonl",
                )));
            }
            inner.seen.insert(stored.event.event_id.to_string(), stored.seq);
            inner.next_seq += 1;
            inner.events.push(stored);
        }
        inner.read_offset += complete.len() as u64;
        Ok(())
    }

    /// Truncate a torn trailing fragment, so the next append lands on the end of
    /// the last complete line instead of on top of the fragment.
    ///
    /// **Requires the lease.** While it is held no live writer can be mid-write
    /// — every writer holds it across its whole write — so an unterminated tail
    /// is a dead writer's half-write, and bytes nobody finished writing are not
    /// facts to preserve. That is the same ground `replay` stands on, reached
    /// from the other end: `replay` runs before anyone can read, this runs
    /// before the only other writer can.
    ///
    /// Nothing folded into memory is dropped: `read_offset` always sits on a line
    /// boundary, so the bytes examined here are exactly the ones no fold has
    /// seen, and a complete line found among them is left for `refresh` to fold.
    fn drop_torn_tail(inner: &mut SessionInner) -> Result<(), EventStoreError> {
        let len = inner.file.metadata()?.len();
        if len <= inner.read_offset {
            // Nothing past what we have accounted for; a log that shrank is
            // `refresh`'s to report, and it does.
            return Ok(());
        }

        let mut tail = vec![0u8; (len - inner.read_offset) as usize];
        inner.read.seek(SeekFrom::Start(inner.read_offset))?;
        inner.read.read_exact(&mut tail)?;

        let keep = complete_len(&tail);
        if keep == tail.len() {
            return Ok(());
        }

        let dropped = (tail.len() - keep) as u64;
        truncate_to(&inner.path, inner.read_offset + keep as u64)?;
        inner.repaired_tail = Some(inner.repaired_tail.unwrap_or(0) + dropped);
        Ok(())
    }
}

/// Decode one durable record and refuse a version this build cannot read.
///
/// The check runs on **every** line rather than on the first as a sample. A log
/// whose records disagree about their schema is precisely the drift that
/// CONTRACT.md §6 C6.1 / `schema_version_boundaries.rs` exist to prevent, and a
/// reader that trusted the first record would accept the disagreement as long
/// as it started well. A malformed version is refused for the same reason as a
/// future one: it is not a shape this build knows how to interpret.
fn decode_stored(line: &[u8]) -> Result<StoredEvent, EventStoreError> {
    let stored: StoredEvent = serde_json::from_slice(line)?;
    versioning::check_event(&stored.event.schema_version)?;
    Ok(stored)
}

/// How much of `bytes` ends on a completed line — the offset just past the last
/// newline, or 0 when the bytes hold none.
///
/// The remainder is the shape a write leaves when it crashes before reaching its
/// newline: no writer of this log ends a record without one.
fn complete_len(bytes: &[u8]) -> usize {
    match bytes.iter().rposition(|&b| b == b'\n') {
        Some(last_nl) => last_nl + 1,
        None => 0,
    }
}

/// Shorten the log to `len`, through a handle opened for it.
///
/// Never through the session's append handle, which is the one handle in this
/// file that cannot do it: that handle is opened append-only, and an append-only
/// handle carries `FILE_APPEND_DATA` and no write right at all on Windows, so
/// `set_len` on it fails with access denied. The three callers need this for the
/// same reason at three different moments — `replay` before anyone can read,
/// `drop_torn_tail` under the lease, `append` to undo a partial write.
fn truncate_to(path: &Path, len: u64) -> Result<(), EventStoreError> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    Ok(())
}

/// Every event in the log from `next_expected` on, reading the tail first.
///
/// Free rather than a method on the store because a subscription outlives any
/// borrow of it: it holds the session, and the session is what a read needs.
fn read_after(session: &Session, next_expected: u64) -> Result<Vec<StoredEvent>, EventStoreError> {
    let mut inner = session.inner.lock().map_err(|_| JsonlEventStore::poisoned())?;
    JsonlEventStore::refresh(&mut inner)?;
    // Sequences in a replayed log are contiguous and ascending, so the first
    // wanted event is a partition point and not a scan.
    let start = inner.events.partition_point(|e| e.seq.as_u64() < next_expected);
    Ok(inner.events[start..].to_vec())
}

#[async_trait::async_trait]
impl EventSubscription for LiveSubscription {
    async fn next(&mut self) -> Result<Option<StoredEvent>, EventStoreError> {
        loop {
            if let Some(event) = self.buffered.pop_front() {
                self.next_expected = event.seq.as_u64() + 1;
                return Ok(Some(event));
            }

            // Read before waiting, not after: a signal raised while the previous
            // batch was being yielded is already sitting in the channel, and the
            // read covers it. Waiting first would still work but would answer a
            // burst with one event per wake-up.
            let events = read_after(&self.session, self.next_expected)?;
            if !events.is_empty() {
                self.buffered.extend(events);
                continue;
            }

            match future::select(self.wake.next(), futures_timer::Delay::new(self.poll)).await {
                // All senders gone: the store that made this subscription was
                // dropped, so no further event can arrive. A deadline is not
                // this: the log may simply be quiet.
                Either::Left((None, _)) => return Ok(None),
                Either::Left((Some(()), _)) => {}
                Either::Right(_) => {}
            }
        }
    }
}

#[async_trait::async_trait]
impl EventStore for JsonlEventStore {
    async fn append(
        &self,
        session_id: &SessionId,
        expected_next_seq: EventSeq,
        drafts: Vec<EventDraft>,
    ) -> Result<AppendReceipt, EventStoreError> {
        let session = self.open(session_id)?;

        // Hold the lease across the whole write. Nested inside a caller's own
        // lease this only bumps a counter, so a critical section can be written
        // as flat code instead of a callback. Named rather than `_`: the guard
        // is the release.
        let _lease = self.leases.acquire(session_id, Some(LEASE_APPEND_WAIT))?;

        let mut inner = session.inner.lock().map_err(|_| Self::poisoned())?;

        // Before reading the tail, clear anything a dead writer left there. The
        // write below appends at the file's end, so a fragment left in place
        // would become the head of our record — one line that parses as neither.
        // Ordered first because the fragment must be gone before `refresh`
        // decides what the tail holds.
        Self::drop_torn_tail(&mut inner)?;

        // Read the tail before comparing: with the lease held, `next_seq` is the
        // log's own next sequence rather than this process's memory of it, so a
        // conflict means someone else wrote and not merely that time passed.
        Self::refresh(&mut inner)?;

        if expected_next_seq.as_u64() != inner.next_seq {
            return Err(EventStoreError::SequenceConflict {
                expected: expected_next_seq,
                actual: EventSeq::new(inner.next_seq),
            });
        }

        // Build the batch in memory first, then persist in a single write. On a
        // partial write we roll the file length back so the log stays clean.
        let mut appended = Vec::new();
        let mut duplicates = 0usize;
        let mut next = inner.next_seq;
        let mut buf = Vec::new();
        for draft in drafts {
            let id_key = draft.event_id.to_string();
            if inner.seen.contains_key(&id_key) {
                duplicates += 1;
                continue;
            }
            let stored =
                StoredEvent { seq: EventSeq::new(next), recorded_at: Utc::now(), event: draft };
            serde_json::to_writer(&mut buf, &stored)?;
            buf.push(b'\n');
            next += 1;
            appended.push(stored);
        }

        let orig_len = inner.file.metadata()?.len();
        if let Err(e) = inner.file.write_all(&buf) {
            // Best effort, and deliberately not reported: the caller is about to
            // get the write's own error, and a rollback that failed has nothing
            // to add to it. It has to be *able* to succeed, though — this used
            // to go through the append handle, where on Windows it silently
            // never happened and a partial batch stayed half-applied (YA-70).
            let _ = truncate_to(&inner.path, orig_len);
            return Err(e.into());
        }

        // Commit to memory only after the write fully succeeded. These bytes are
        // ours and are known complete, so the read cursor moves past them too —
        // re-reading them would only parse what we just built.
        for stored in &appended {
            inner.seen.insert(stored.event.event_id.to_string(), stored.seq);
        }
        inner.next_seq = next;
        inner.read_offset += buf.len() as u64;
        inner.events.extend(appended.iter().cloned());

        // After the commit, never before: a subscriber woken by a write that
        // then failed would look for an event that is not there. It would find
        // nothing and wait again — a wasted pass, but a pass that cannot show a
        // fact the log does not contain.
        drop(inner);
        if !appended.is_empty() {
            self.wakeups.wake(session_id.as_str());
        }

        Ok(AppendReceipt { session_id: session_id.clone(), appended, duplicates })
    }

    async fn lease(
        &self,
        session_id: &SessionId,
    ) -> Result<Box<dyn SessionLease>, EventStoreError> {
        // Validate the id before it becomes a path, exactly as `open` does.
        let key = session_id.as_str();
        if !is_safe_dir_name(key) {
            return Err(EventStoreError::unsafe_session_id());
        }
        fs::create_dir_all(self.session_dir(key))?;
        Ok(Box::new(self.leases.acquire(session_id, self.lease_wait)?))
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), EventStoreError> {
        let key = session_id.as_str();
        if !is_safe_dir_name(key) {
            return Err(EventStoreError::unsafe_session_id());
        }
        let dir = self.session_dir(key);
        // A session that was never written is already gone: the caller asked
        // for it not to be there, and it is not. Left as a success so deleting
        // twice, or deleting a session another page just deleted, is not an
        // error the person has to interpret.
        if !dir.exists() {
            self.sessions.lock().map_err(|_| Self::poisoned())?.remove(key);
            return Ok(());
        }
        // The lease is taken so no append lands between the cache drop and the
        // removal. `lease_wait` (rather than the short append wait) because the
        // holder may be a granted tool call that runs for minutes, and waiting
        // for it to finish is what the delete is asking to do anyway.
        //
        // `acquire` creates the lock file, so the directory has to be there for
        // it: this recreates one a peer removed between the check above and
        // here, and the removal below takes it away again. Only reached when
        // the directory existed a moment ago, so a delete that fails on the
        // lease does not leave an empty session behind.
        fs::create_dir_all(&dir)?;
        let _lease = self.leases.acquire(session_id, self.lease_wait)?;

        // Drop this process's handles before the directory goes. On Windows an
        // open handle is what makes a removal fail, and a cached session would
        // otherwise let an id that is created again read a log that is gone.
        self.sessions.lock().map_err(|_| Self::poisoned())?.remove(key);

        // A live subscription holds its own `Arc<Session>`, so the files can
        // stay open a moment after the cache entry goes — an SSE stream ends
        // when its client disconnects, which is not synchronous with this call.
        // Retry briefly rather than reporting a sharing violation the caller
        // cannot act on.
        let mut last = None;
        for attempt in 0..DELETE_RETRIES {
            match fs::remove_dir_all(&dir) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => last = Some(error),
            }
            if attempt + 1 < DELETE_RETRIES {
                // A blocking sleep, for the same reason `acquire` waits with
                // one: this crate has no runtime of its own, and the whole
                // store treats a wait as "the calling thread is busy" rather
                // than "the executor may run something else".
                std::thread::sleep(DELETE_RETRY);
            }
        }
        last.map_or(Ok(()), |error| Err(error.into()))
    }

    async fn flush(&self, session_id: &SessionId) -> Result<FlushReceipt, EventStoreError> {
        let session = self.open(session_id)?;
        let inner = session.inner.lock().map_err(|_| Self::poisoned())?;
        inner.file.sync_all()?;
        let durable_seq = EventSeq::new(inner.next_seq.saturating_sub(1));
        Ok(FlushReceipt { session_id: session_id.clone(), durable_seq })
    }

    async fn read_stream(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, EventStoreError> {
        let session = self.open(session_id)?;
        let mut inner = session.inner.lock().map_err(|_| Self::poisoned())?;
        // Taking the lock is not enough to see another process's work — the tail
        // has to be picked up, or a long-lived reader answers from a snapshot
        // taken when it started.
        Self::refresh(&mut inner)?;
        Ok(inner.events.iter().filter(|e| e.seq >= from_seq).take(limit).cloned().collect())
    }

    async fn subscribe(
        &self,
        session_id: &SessionId,
        from_seq: EventSeq,
    ) -> Result<Box<dyn EventSubscription>, EventStoreError> {
        let session = self.open(session_id)?;
        let key = session_id.as_str().to_string();

        // Register before reading, never the other way round. A signal raised
        // between the two is lost by that order; by this one it is merely
        // redundant, because whatever happens after the registration is picked
        // up by the read below or by the pass that follows the signal.
        let (tx, wake) = mpsc::channel(1);
        self.wakeups.register(&key, tx);

        let next_expected = from_seq.as_u64();
        let buffered: VecDeque<StoredEvent> =
            read_after(&session, next_expected)?.into_iter().collect();
        Ok(Box::new(LiveSubscription { session, next_expected, buffered, wake, poll: self.poll }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::versioning::VersionError;
    use std::io::Write;

    /// A session id that is `..` passes every character test — it is non-empty,
    /// short, and made only of an allowed character — and resolves to the
    /// store root's *parent*. `open` creates the session directory and its log,
    /// so accepting it puts an `events.jsonl` one level above the root, beside
    /// the store rather than in it. `.` is the same shape of escape with a
    /// shorter walk: it lands in the root itself, where no session's log
    /// belongs either (YA-08).
    ///
    /// The probe is where the bytes land, not the error message: the message is
    /// the same for every refusal, and the defect is precisely that nothing
    /// refuses.
    #[test]
    fn a_session_id_that_is_only_dots_cannot_open_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let store = JsonlEventStore::new(&root);

        for id in [".", ".."] {
            // `Session` has no `Debug`, so the error is matched rather than
            // unwrapped.
            match store.open(&SessionId::from_string(id)) {
                Err(error) => assert!(
                    error.to_string().contains("not a safe directory name"),
                    "{id}: {error}"
                ),
                Ok(_) => panic!("{id} was accepted as a session id"),
            }
        }

        // `..` would have landed here, beside the store; `.` would have landed
        // in the store's own root, where no session directory belongs.
        assert!(!dir.path().join("events.jsonl").exists(), "a log was opened outside the root");
        assert!(!root.join("events.jsonl").exists(), "a log was opened in the root itself");

        // A dot inside a name is an ordinary character, and this is what the
        // rejection is careful not to catch.
        assert!(store.open(&SessionId::from_string("2026-10-11.run")).is_ok());
    }

    /// On Windows a segment can name something other than the directory it
    /// spells, and the character test above cannot see either way. A trailing
    /// `.` is dropped, so `foo.` and `foo` are one directory and two session ids
    /// would share one log; and the reserved device names route to a device, so
    /// `NUL` opens, "creates" nothing, and the log under it then fails with "not
    /// found". Both are refused here (YA-71). Windows-only: on POSIX the same
    /// names are ordinary directories and are still accepted.
    #[cfg(windows)]
    #[test]
    fn a_session_id_windows_would_rename_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let store = JsonlEventStore::new(&root);

        for id in
            ["NUL", "nul", "NUL.", "NUL.txt", "CON", "PRN", "AUX", "COM1", "COM9", "LPT1", "foo."]
        {
            match store.open(&SessionId::from_string(id)) {
                Err(error) => assert!(
                    error.to_string().contains("not a safe directory name"),
                    "{id}: {error}"
                ),
                Ok(_) => panic!("{id} was accepted as a session id"),
            }
        }

        // The name before the first `.` decides, and a `.` inside a name is
        // ordinary, so these are all real directories and still open.
        for id in ["foo.NUL", "foo", "2026-10-11.run"] {
            assert!(store.open(&SessionId::from_string(id)).is_ok(), "{id} was refused");
        }
    }

    /// The rollback after a partial write has to shorten the log, and the
    /// session's append handle is the one handle that cannot: it is opened
    /// append-only, and on Windows that means `FILE_APPEND_DATA` and no write
    /// right, so `set_len` on it fails with access denied. It failed silently,
    /// under a `let _ =`, which is what made a half-written batch stay
    /// half-applied (YA-70).
    ///
    /// This pins the mechanism the rollback depends on, while a session's append
    /// handle is open on the same file — which is the condition under which the
    /// rollback runs and the reason it cannot use that handle. The rollback's own
    /// path is not reachable from a test without injecting a write failure, so it
    /// is the mechanism that is pinned here, not the call site.
    #[test]
    fn shortening_the_log_works_with_the_append_handle_open() {
        let dir = tempfile::tempdir().unwrap();
        let inner = JsonlEventStore::load(dir.path()).unwrap();

        // Behind the store's back, as a write that died mid-record does.
        let half = b"{\"seq\":0,\"recorded_at\":\"2026-";
        let mut w = OpenOptions::new().append(true).open(&inner.path).unwrap();
        w.write_all(half).unwrap();
        drop(w);
        assert_eq!(inner.file.metadata().unwrap().len(), half.len() as u64);

        truncate_to(&inner.path, 0).unwrap();
        assert_eq!(inner.file.metadata().unwrap().len(), 0);
    }

    /// A torn tail is a fact, not just a repair: whoever opens the log can ask
    /// how many bytes of a half-written record were dropped (ST-8). Without
    /// this, the repair erased the only evidence that a crash had happened.
    #[test]
    fn a_torn_tail_is_repaired_and_the_repair_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let session = SessionId::from_string("s");
        let store = JsonlEventStore::new(&root);

        let complete = br#"{"seq":0,"recorded_at":"2026-10-17T00:00:00Z","event":{"event_id":"e0","event_type":"session/started","schema_version":"0.1.0","session_id":"s","run_id":null,"turn_id":null,"step_id":null,"actor":"system","created_at":"2026-10-17T00:00:00Z","causation_id":null,"correlation_id":null,"payload":{},"metadata":{},"ignorable":false}}"#;
        let fragment = br#"{"seq":1,"recorded_at":"2026-10-"#;

        let dir = root.join("s");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let mut file = OpenOptions::new().create(true).append(true).open(&path).unwrap();
        file.write_all(complete).unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(fragment).unwrap();
        drop(file);

        assert_eq!(
            store.repaired_tail(&session).unwrap(),
            Some(fragment.len() as u64),
            "the repair must report the bytes it dropped"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            complete.len() as u64 + 1,
            "the torn fragment is gone and the complete line is intact"
        );
        // The fact belongs to the process's open, so asking again does not
        // rewrite it as "nothing happened".
        assert_eq!(store.repaired_tail(&session).unwrap(), Some(fragment.len() as u64));
    }

    /// A log that ended on a record boundary reports no repair, so "nothing was
    /// wrong" and "something was repaired" stay distinguishable.
    #[test]
    fn an_intact_tail_reports_no_repair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        let store = JsonlEventStore::new(&root);
        let session = SessionId::from_string("s");

        let dir = root.join("s");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.jsonl"), b"").unwrap();

        assert_eq!(store.repaired_tail(&session).unwrap(), None);
    }

    /// One durable line, spelled the way the store writes it, with the envelope
    /// version a test chooses.
    fn event_line(seq: u64, schema_version: &str) -> String {
        format!(
            r#"{{"seq":{seq},"recorded_at":"2026-10-17T00:00:00Z","event":{{"event_id":"e{seq}","event_type":"session/started","schema_version":"{schema_version}","session_id":"s","run_id":null,"turn_id":null,"step_id":null,"actor":"system","created_at":"2026-10-17T00:00:00Z","causation_id":null,"correlation_id":null,"payload":{{}},"metadata":{{}},"ignorable":false}}}}"#
        )
    }

    fn write_log(root: &std::path::Path, lines: &[String]) {
        let dir = root.join("s");
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(dir.join("events.jsonl"), text).unwrap();
    }

    /// A record written by a newer build is refused when the session is opened,
    /// not parsed best-effort (YA-21). The second line is the one from the
    /// future: checking only the first would let every later disagreement in.
    #[test]
    fn a_future_event_schema_is_refused_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_log(&root, &[event_line(0, "0.1.0"), event_line(1, "99.0.0")]);
        let store = JsonlEventStore::new(&root);

        match store.open(&SessionId::from_string("s")) {
            Err(EventStoreError::Version(VersionError::Future { found, supported })) => {
                assert_eq!(found, "99.0.0");
                assert_eq!(supported, "0.1.0");
            }
            Err(other) => panic!("wrong error for a future record: {other}"),
            Ok(_) => panic!("a future event schema was read as if it were current"),
        }
    }

    /// An older record is refused explicitly rather than silently treated as
    /// current; no migration exists in this build (YA-21).
    #[test]
    fn an_older_event_schema_with_no_migration_is_refused_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_log(&root, &[event_line(0, "0.0.9")]);
        let store = JsonlEventStore::new(&root);

        assert!(matches!(
            store.open(&SessionId::from_string("s")),
            Err(EventStoreError::Version(VersionError::NoMigration { .. }))
        ));
    }

    /// The version is not trusted just because it parses as a string; a
    /// malformed one is a refusal naming the record's claim.
    #[test]
    fn a_malformed_event_schema_is_refused_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_log(&root, &[event_line(0, "not-a-version")]);
        let store = JsonlEventStore::new(&root);

        assert!(matches!(
            store.open(&SessionId::from_string("s")),
            Err(EventStoreError::Version(VersionError::Malformed { .. }))
        ));
    }

    /// The check must not refuse the log this build itself writes.
    #[test]
    fn a_current_event_schema_opens_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions");
        write_log(&root, &[event_line(0, "0.1.0"), event_line(1, "0.1.0")]);
        let store = JsonlEventStore::new(&root);

        assert!(store.open(&SessionId::from_string("s")).is_ok());
    }
}
