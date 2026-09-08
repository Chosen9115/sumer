//! The multiplexer: id issuance and lifecycle, the bounded outbound queue,
//! per-request deadlines, backpressure, and the inbound reply loop.
//!
//! **Id lifecycle** (spec/wire.md §6). `id` is a `u64` drawn from a counter
//! that is never reused for the process lifetime. Three states:
//!
//! - **issued** (here: [`Slot::Pending`]) -- sent, no reply yet, deadline
//!   not expired.
//! - **tombstoned** ([`Slot::Tombstoned`]) -- deadline expired; a reply
//!   naming this id is counted and silently discarded. The connection
//!   survives: an honest, merely slow adapter must not be killed for a
//!   host-side deadline.
//! - **never-issued / already-answered** -- above the counter's high-water
//!   mark, an impossible gap, or a second reply to an id already answered
//!   ([`Slot::Answered`]). Fatal: [`MuxViolation::UnknownId`] /
//!   `DuplicateId`. There is no legitimate way for either to happen, and
//!   the alternative -- an id-reuse scheme with no cancel frame -- lets a
//!   late reply to a recycled id cross-attribute one account's balance onto
//!   another's, undetectably (the envelope carries no `op`/`params` echo).
//!
//! **The deadline starts at send, not enqueue** (spec/wire.md §7). A
//! request sitting in the bounded queue has not started its clock; if it
//! sits long enough to exceed its own deadline before ever reaching the
//! front of the queue *and* being written, it is purged and never sent
//! (checked in [`pump`], both before and after waiting for a concurrency
//! permit). Once it reaches the front, ONE `deadline`-long timer covers the
//! whole send: writing the frame *and* waiting for its reply. The write is
//! not free -- an adapter that stops reading its stdin while staying alive
//! blocks the host in `write_all` behind a full pipe -- so leaving it
//! outside the deadline would let a request outlive its own deadline
//! without bound, and stall every request queued behind it.
//!
//! **A write cut short by the deadline ends the connection.** Half a frame
//! is on the adapter's stdin and there is no resync point in JSON Lines
//! (spec/wire.md §2), so the rest of that frame is never written: the pump
//! stops, the child is killed, and the caller is told `Timeout` while every
//! other request on the connection resolves `AdapterCrashed`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sumer_wire::{ErrorBody, ProtocolViolationKind, Reply, RequestId, Rfc3339, WireErrorCode};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, Notify, Semaphore};

use crate::HostError;

/// Outbound queue capacity (frozen contract, section (f)): "Bounded mpsc
/// (64) outbound; a full queue makes the caller await." `tokio::sync::mpsc`
/// already blocks `send().await` on a full bounded channel, which is
/// exactly this backpressure -- no extra machinery needed.
pub const OUTBOUND_QUEUE_CAPACITY: usize = 64;

/// What a pending call's reply channel eventually carries: a genuine wire
/// reply, or one of the two host-side terminal outcomes that end every
/// still-pending call on this connection at once.
#[derive(Debug)]
enum Delivery {
    /// The frame's original text, plus the host's receipt stamp. Text, not
    /// a parsed `Value`: `Value` collapses duplicate object keys, so every
    /// typed decode downstream of one is blind to a duplicate the adapter
    /// actually sent. The caller re-deserializes these same bytes into the
    /// shape its op expects.
    Reply(String, Rfc3339),
    Crashed(Option<i32>),
    Violation(ProtocolViolationKind),
}

impl From<Terminal> for Delivery {
    fn from(t: Terminal) -> Self {
        match t {
            Terminal::Crashed(status) => Delivery::Crashed(status),
            Terminal::Violation(kind) => Delivery::Violation(kind),
        }
    }
}

impl From<Terminal> for HostError {
    fn from(t: Terminal) -> Self {
        match t {
            Terminal::Crashed(status) => HostError::AdapterCrashed { status },
            Terminal::Violation(kind) => HostError::ProtocolViolation(kind),
        }
    }
}

/// Why a connection is finished. Set at most once per [`Mux`] -- see
/// [`Mux::finish`].
#[derive(Debug, Clone, Copy)]
pub enum Terminal {
    Crashed(Option<i32>),
    Violation(ProtocolViolationKind),
}

enum Slot {
    Pending(oneshot::Sender<Delivery>),
    Tombstoned,
    Answered,
}

struct MuxInner {
    next_id: u64,
    slots: HashMap<u64, Slot>,
    terminal: Option<Terminal>,
}

/// The outcome of delivering a reply to a legitimately-tracked id.
pub enum ReplyOutcome {
    Delivered,
    /// The id was tombstoned: counted, then discarded. The connection
    /// survives.
    Discarded,
}

/// A reply named an id with no legitimate meaning left. Fatal by
/// construction -- see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxViolation {
    UnknownId,
    DuplicateId,
}

impl From<MuxViolation> for ProtocolViolationKind {
    fn from(v: MuxViolation) -> Self {
        match v {
            MuxViolation::UnknownId => ProtocolViolationKind::UnknownId,
            MuxViolation::DuplicateId => ProtocolViolationKind::DuplicateId,
        }
    }
}

/// The id/lifecycle/backpressure state for one adapter connection.
pub struct Mux {
    inner: Mutex<MuxInner>,
    /// **The publication latch for condition (9)**, and deliberately NOT
    /// part of `inner`.
    ///
    /// The first fatal protocol violation this connection was **caught
    /// committing**. Two publishers write it, and neither is privileged:
    /// [`Mux::record_violation`], from the reader task at the instant a
    /// frame is decoded, and [`Mux::finish`], for a terminal reason that
    /// is itself a violation -- `AdapterHandle::close`'s `StdinEofIgnored`
    /// / `StdoutHeldOpen`, which are only knowable at the close and never
    /// pass through the reader at all. First one wins; a caller asking
    /// "has this connection broken the contract?" does not care which
    /// published it.
    ///
    /// It gets its own lock because [`Mux::with_violation_held`] holds
    /// that lock across a caller's commit. Held on `inner`, that froze the
    /// whole connection lifecycle for the duration: a late, tombstoned
    /// reply then blocks in [`Mux::deliver`] -- inside the reader task, on
    /// a runtime worker that keeps its core while it waits, so the
    /// runtime's timers and readiness stop with it. `process::READER_DRAIN`
    /// is one of those timers, and the adapter that merely answered late
    /// is charged with `StdoutHeldOpen`: a violation manufactured by the
    /// mechanism that exists to report violations honestly. Only the
    /// verdict needs freezing, so only the verdict is frozen.
    verdict: Mutex<Option<ProtocolViolationKind>>,
    outbox: mpsc::Sender<QueuedRequest>,
    /// Concurrency gate for `max_in_flight`. Starts at 1 permit (enough for
    /// the hello call); [`Mux::raise_concurrency`] tops it up once the
    /// adapter's declared value is known.
    semaphore: Arc<Semaphore>,
    /// `None` on every production connection ([`Mux::spawn`]); only
    /// [`Mux::spawn_recorded`] fills this in. See [`Transcript`].
    transcript: Option<Arc<Transcript>>,
    /// Fired by [`Mux::begin_close`]: tells [`pump`] to stop and drop the
    /// child's stdin, which is what gives a connection a defined END.
    closing: Notify,
}

/// One request/reply pair as it passed through [`Mux::call`].
///
/// **The transcript is DISPATCH order**: an `Exchange` is pushed the
/// instant `call` hands a request to the mux, before anything about its
/// reply -- if it ever gets one at all -- is known. **Reply CONTENT is
/// only ordered the same way when dispatch is itself serial.** An adapter
/// declaring `max_in_flight` above `1` can answer out of the order its
/// requests were dispatched in, so on such a connection two entries'
/// `frame`/`received_at` can fill in in a different order than the
/// entries themselves appear in the vector.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub op: String,
    pub params: Value,
    pub frame: Option<String>,
    pub received_at: Option<Rfc3339>,
}

/// Raw request/reply evidence for one adapter connection, recorded from
/// the SAME execution that produces the host's typed replies -- never a
/// second, separate pass an adapter could distinguish (e.g. by fixture
/// run index) and behave differently on. Built only by
/// [`Mux::spawn_recorded`]; a production connection ([`Mux::spawn`]) has
/// none, and records nothing.
///
/// **A discarded, tombstoned reply is never recorded.** [`Mux::deliver`]
/// drops a reply naming a tombstoned id before it reaches any caller --
/// including this transcript -- and that drop is legally not a violation
/// (spec/wire.md §6: "the connection survives"). An oversized or
/// otherwise non-conforming observation riding in on a reply that arrives
/// after its own deadline is therefore invisible to measurement forever.
///
/// **The aperture is one id, not one deadline.** A tombstoned slot is
/// never cleared: `deliver` discards *every* later reply naming that id,
/// for the rest of the connection's life, however long after the deadline
/// it arrives. What bounds this is that only a request the host gave up on
/// is ever tombstoned, and each such request is one id -- not that late
/// replies stop being discarded once the deadline is some distance past.
/// This is an accepted hole, not a bug to fix, and it is written down
/// accurately here so a future reviewer does not rediscover it as a
/// surprise -- an overstated safety claim would be worse than the hole.
#[derive(Default)]
pub(crate) struct Transcript(Mutex<Vec<Exchange>>);

impl Transcript {
    /// Appends a not-yet-answered exchange and returns its index.
    fn push(&self, op: String, params: Value) -> usize {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.push(Exchange {
            op,
            params,
            frame: None,
            received_at: None,
        });
        inner.len() - 1
    }

    /// Fills in the reply half of a previously-pushed exchange. A no-op if
    /// `index` is somehow out of range (never expected in practice: this
    /// is only ever called with an index this same `Transcript` just
    /// handed out).
    fn fill(&self, index: usize, frame: String, received_at: Rfc3339) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(exchange) = inner.get_mut(index) {
            exchange.frame = Some(frame);
            exchange.received_at = Some(received_at);
        }
    }

    /// A snapshot of everything recorded so far, in dispatch order.
    pub(crate) fn snapshot(&self) -> Vec<Exchange> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

struct QueuedRequest {
    op: String,
    params: Value,
    deadline: Duration,
    enqueued_at: Instant,
    responder: oneshot::Sender<Result<(String, Rfc3339), HostError>>,
}

impl Mux {
    /// Builds a `Mux` and starts its pump task, which owns `stdin` for the
    /// lifetime of the connection. `max_in_flight` starts at `1` (enough
    /// for the hello call, which is sent through the same machinery as any
    /// other call); call [`Mux::raise_concurrency`] once hello succeeds to
    /// open it up to the adapter's declared value.
    ///
    /// Records nothing: production traffic has no [`Transcript`]. Use
    /// [`Mux::spawn_recorded`] for a connection the conformance suite
    /// needs raw wire evidence from.
    #[must_use]
    pub fn spawn(
        stdin: ChildStdin,
        kill_tx: mpsc::Sender<Option<ProtocolViolationKind>>,
    ) -> Arc<Mux> {
        Mux::spawn_inner(stdin, kill_tx, None)
    }

    /// As [`Mux::spawn`], additionally recording every request/reply that
    /// passes through [`Mux::call`] into the returned [`Transcript`] --
    /// see that type's docs for why this exists and its one accepted gap.
    #[must_use]
    pub(crate) fn spawn_recorded(
        stdin: ChildStdin,
        kill_tx: mpsc::Sender<Option<ProtocolViolationKind>>,
    ) -> (Arc<Mux>, Arc<Transcript>) {
        let transcript = Arc::new(Transcript::default());
        let mux = Mux::spawn_inner(stdin, kill_tx, Some(transcript.clone()));
        (mux, transcript)
    }

    fn spawn_inner(
        stdin: ChildStdin,
        kill_tx: mpsc::Sender<Option<ProtocolViolationKind>>,
        transcript: Option<Arc<Transcript>>,
    ) -> Arc<Mux> {
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let semaphore = Arc::new(Semaphore::new(1));
        let mux = Arc::new(Mux {
            inner: Mutex::new(MuxInner {
                next_id: 0,
                slots: HashMap::new(),
                terminal: None,
            }),
            verdict: Mutex::new(None),
            outbox: tx,
            semaphore: semaphore.clone(),
            transcript,
            closing: Notify::new(),
        });
        tokio::spawn(pump(mux.clone(), rx, stdin, semaphore, kill_tx));
        mux
    }

    /// Adds concurrency once the adapter's `max_in_flight` is known. A
    /// no-op for the legal serial default (`1`): "the host never assumes
    /// concurrency" it wasn't told about.
    pub fn raise_concurrency(&self, max_in_flight: u32) {
        let extra = max_in_flight.saturating_sub(1);
        if extra > 0 {
            self.semaphore
                .add_permits(usize::try_from(extra).unwrap_or(usize::MAX));
        }
    }

    /// Allocates the next id, or `None` once the counter is exhausted.
    /// Ids are monotonic and never reused (spec/wire.md §6), so the counter
    /// has an end; wrapping past it would reissue an id whose late reply
    /// could be cross-attributed, which is the one failure the never-reuse
    /// rule exists to make impossible. Exhaustion is reported to the caller
    /// as [`HostError::IdsExhausted`] instead -- no wrap, no panic, no
    /// abandoned caller.
    fn issue(&self) -> Option<(RequestId, oneshot::Receiver<Delivery>)> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = inner.next_id;
        inner.next_id = id.checked_add(1)?;
        let (tx, rx) = oneshot::channel();
        inner.slots.insert(id, Slot::Pending(tx));
        Some((RequestId(id), rx))
    }

    /// Expires an id whose deadline has passed with no reply. A race
    /// against a reply that is *already* being delivered is resolved in
    /// the reply's favor: this only touches a slot still `Pending`.
    fn tombstone(&self, id: RequestId) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot @ Slot::Pending(_)) = inner.slots.get_mut(&id.0) {
            *slot = Slot::Tombstoned;
        }
    }

    /// Routes a decoded reply to the id it names. `received_at` is the
    /// host-stamped receipt time of the *frame*, captured by the reader
    /// loop the instant the frame was decoded -- not later, so it reflects
    /// when the host actually received the bytes, not when some downstream
    /// task got scheduled.
    fn deliver(
        &self,
        id: RequestId,
        frame: String,
        received_at: Rfc3339,
    ) -> Result<ReplyOutcome, MuxViolation> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if id.0 >= inner.next_id {
            return Err(MuxViolation::UnknownId);
        }
        match inner.slots.get_mut(&id.0) {
            Some(slot @ Slot::Pending(_)) => {
                let prev = std::mem::replace(slot, Slot::Answered);
                drop(inner);
                if let Slot::Pending(tx) = prev {
                    let _ = tx.send(Delivery::Reply(frame, received_at));
                }
                Ok(ReplyOutcome::Delivered)
            }
            Some(Slot::Tombstoned) => Ok(ReplyOutcome::Discarded),
            Some(Slot::Answered) => Err(MuxViolation::DuplicateId),
            None => Err(MuxViolation::UnknownId),
        }
    }

    /// Records the terminal reason for this connection and resolves every
    /// still-pending call with it. Idempotent: only the *first* call sticks
    /// (returns `true`); a later call is a no-op (returns `false`). This is
    /// what lets both the reader loop (on a protocol violation) and the
    /// exit-watcher (on process exit) call this unconditionally without
    /// racing each other for which reason "wins".
    pub fn finish(&self, reason: Terminal) -> bool {
        // **Only a reason that publishes takes the publication latch.** An
        // ordinary crash is not a violation and has nothing to publish, so
        // it must not queue behind a held verdict: `supervise` calls this
        // from a runtime worker, and a worker blocked on a lock keeps its
        // core -- no timers, no readiness, for the rest of the caller's
        // commit. Ending an honest connection is not worth stopping the
        // runtime for.
        //
        // A violation-carrying reason does wait, and that wait IS the
        // guarantee: publication is what `with_violation_held` excludes.
        //
        // `verdict` FIRST, then `inner`, and never the other way round --
        // that is the lock order this connection has, and this is the only
        // place that holds both. Publishing under the same lock that
        // latches `terminal` keeps the two in step: a violation this
        // finish establishes is never visible LATER than the terminal
        // reason carrying it, so a gate that saw no violation was not
        // racing one that had already been decided.
        let mut verdict = match reason {
            Terminal::Violation(_) => Some(self.verdict.lock().unwrap_or_else(|e| e.into_inner())),
            Terminal::Crashed(_) => None,
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.terminal.is_some() {
            return false;
        }
        inner.terminal = Some(reason);
        // Only the reason that WON publishes. A loser announcing its kind
        // would report a violation on a connection whose terminal reason
        // is an ordinary crash -- a false violation, which is the one
        // thing worse than a missed one.
        if let (Terminal::Violation(kind), Some(verdict)) = (reason, verdict.as_mut()) {
            verdict.get_or_insert(kind);
        }
        let pending: Vec<_> = inner
            .slots
            .values_mut()
            .filter_map(|slot| match std::mem::replace(slot, Slot::Answered) {
                Slot::Pending(tx) => Some(tx),
                other => {
                    *slot = other;
                    None
                }
            })
            .collect();
        drop(inner);
        for tx in pending {
            let _ = tx.send(Delivery::from(reason));
        }
        true
    }

    /// Ends the connection from the host's side: [`pump`] stops pulling and
    /// **drops the child's stdin**, so an adapter blocked on reading its
    /// stdin sees EOF and exits.
    ///
    /// This is what gives an execution a defined CLOSE. Without it a
    /// connection has no end at all: the last measured reply is delivered,
    /// the caller judges what it has, and anything the adapter writes
    /// afterwards -- a malformed frame, a second answer to an id already
    /// answered -- lands (or does not) in whatever order the scheduler
    /// happens to pick. After this call a conforming adapter exits, its
    /// exit closes its stdout, the reader loop decodes what it reads and
    /// finalizes the framing at that end of stream (a read *error* also
    /// ends it, and is charged to the pipe rather than to the adapter --
    /// see the loop in `reader`), and `supervise` latches the terminal
    /// reason. So "the connection ended clean" becomes
    /// a fact that can be waited for and checked, rather than a race nobody
    /// looks at.
    ///
    /// **The exit is not the end of the stream**, and neither the reader
    /// loop nor `supervise` pretends otherwise: a process that inherited
    /// the adapter's stdout keeps it open past the exit, which is reported
    /// as [`ProtocolViolationKind::StdoutHeldOpen`] rather than waited out
    /// and called an ordinary exit. See [`crate::AdapterHandle::close`].
    ///
    /// Requests already queued are still sent: this is a close, not a
    /// cancel. Idempotent, and safe on a connection that is already gone.
    pub(crate) fn begin_close(&self) {
        self.closing.notify_one();
    }

    /// Publishes a fatal protocol violation **the moment it is decoded**,
    /// before the child has been killed and before a terminal reason is
    /// latched. First one wins, for the same reason [`Mux::finish`] keeps
    /// the first reason: what a connection did first is what it did.
    ///
    /// This is deliberately not `finish`: finishing resolves every pending
    /// call, which is `supervise`'s decision to make once it owns the
    /// child's fate. All this does is record what the host now knows.
    ///
    /// **It is not the only publisher.** `finish(Terminal::Violation(..))`
    /// publishes into the same latch, under the same lock, for the two
    /// kinds that are only knowable at the close. Any claim that a single
    /// function publishes violations is wrong.
    fn record_violation(&self, kind: ProtocolViolationKind) {
        self.verdict
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(kind);
    }

    /// Has this connection been **established** to have broken the wire
    /// contract, as of this instant? The live query behind §8.1 condition
    /// (9).
    ///
    /// It is a snapshot, and honestly so: it answers for what the host has
    /// judged by now, not for what the adapter has written. A frame still
    /// in flight is not yet a violation to anyone. It is also stale the
    /// instant it returns -- the lock is gone before the caller sees the
    /// value. A caller whose *decision* depends on the answer wants
    /// [`Mux::with_violation_held`] instead.
    pub(crate) fn violation(&self) -> Option<ProtocolViolationKind> {
        *self.verdict.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the verdict latch is free **at this instant**. Test support
    /// for the one property that has no outcome to assert on: that a
    /// caller inside [`Mux::with_violation_held`] really does exclude
    /// every publisher, rather than merely failing to be overtaken by one.
    /// A witness thread calling this from inside another thread's held
    /// section gets `false` iff the exclusion is real.
    ///
    /// Not a synchronization primitive: `true` says only that nothing held
    /// it when it was asked. Never branch on it outside a test.
    #[doc(hidden)]
    #[must_use]
    pub fn violation_latch_is_free(&self) -> bool {
        self.verdict.try_lock().is_ok()
    }

    /// Reads the violation verdict and **holds it frozen** for the whole of
    /// `f`.
    ///
    /// [`Mux::violation`] answers truthfully and is immediately stale: the
    /// lock is released before the caller can act on the answer, and
    /// [`Mux::record_violation`] runs on the reader task, on another
    /// runtime worker thread. A caller that reads, then decides, then
    /// makes its decision durable has a window between the three in which
    /// the verdict it is acting on can be overtaken -- and no absence of
    /// `.await` closes it, because another thread does not need this one to
    /// yield.
    ///
    /// So the verdict and the act are put in the same critical section.
    /// **Both** publishers take this lock -- `record_violation` (the
    /// reader, at decode) and `finish` (a terminal reason that is itself a
    /// violation) -- so a violation is published either strictly BEFORE
    /// the value `f` is handed, or strictly AFTER `f` has returned. There
    /// is no third possibility, and that is the ordering
    /// `spec/observation.md` §8.1 condition (9) needs to mean anything.
    ///
    /// # What is frozen, and what deliberately is not
    ///
    /// Only [`Mux::verdict`] -- not `MuxInner`. Freezing the connection's
    /// whole lifecycle here stops `deliver` too, and a late reply blocked
    /// there blocks the reader task on a runtime worker that holds its
    /// core while it waits -- which stops the runtime's timers, including
    /// the drain budget `supervise` measures adapters against, and reports
    /// an adapter that merely answered late as `StdoutHeldOpen`. So ids
    /// keep being issued, tombstoned and answered while `f` runs. The only
    /// thing that waits is a violation trying to become established, which
    /// is exactly the thing that must wait -- and that wait is why `f`
    /// must be short.
    ///
    /// # The contract on `f`
    ///
    /// * `f` MUST NOT publish or read this connection's verdict --
    ///   `record_violation`, `finish`, `violation` and this function all
    ///   take this same non-reentrant lock, so that is a deadlock, not a
    ///   wait. Simplest rule: `f` does not touch this connection at all.
    /// * it MUST NOT `.await` -- the signature already forbids it, and the
    ///   reason is that a violation cannot be established for as long as
    ///   `f` runs.
    ///
    /// ponytail: publication blocks for as long as `f` runs. Its one
    /// caller runs a local SQLite commit there, which is microseconds to
    /// milliseconds.
    pub(crate) fn with_violation_held<T>(
        &self,
        f: impl FnOnce(Option<ProtocolViolationKind>) -> T,
    ) -> T {
        let verdict = self.verdict.lock().unwrap_or_else(|e| e.into_inner());
        f(*verdict)
    }

    pub(crate) fn terminal(&self) -> Option<Terminal> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal
    }

    /// What to report for a call that cannot be completed because the
    /// connection is finished. The latched terminal reason if there is one
    /// -- it is the *true* reason, and a caller that is handed anything
    /// else is being told the adapter died of unknown causes when the host
    /// knows perfectly well it killed it for a named protocol violation.
    ///
    /// `AdapterCrashed { status: None }` is the fallback, and it means
    /// exactly one thing: the adapter is gone and nothing has established
    /// why. It is never a stand-in for a reason that exists.
    fn terminal_error(&self) -> HostError {
        self.terminal()
            .map(HostError::from)
            .unwrap_or(HostError::AdapterCrashed { status: None })
    }

    /// Enqueues one request and awaits its outcome. Backpressure is the
    /// bounded channel itself: `send().await` blocks the caller while the
    /// queue is full, per the frozen contract.
    ///
    /// Returns the reply frame's original text and the host's receipt
    /// stamp; deserializing it is the caller's job, and doing it from
    /// these bytes (rather than from a `Value` this layer parsed) is what
    /// keeps duplicate-key and unknown-field rejection alive end to end.
    pub async fn call(
        &self,
        op: String,
        params: Value,
        deadline: Duration,
    ) -> Result<(String, Rfc3339), HostError> {
        if let Some(t) = self.terminal() {
            return Err(t.into());
        }
        // Pushed here, at dispatch, with no reply yet -- so a request that
        // never gets one (tombstoned, or the connection ends first) still
        // shows up in the transcript instead of a killed run silently
        // under-reporting its final page.
        let recorded = self
            .transcript
            .as_ref()
            .map(|t| t.push(op.clone(), params.clone()));
        let (responder, rx) = oneshot::channel();
        let entry = QueuedRequest {
            op,
            params,
            deadline,
            enqueued_at: Instant::now(),
            responder,
        };
        if self.outbox.send(entry).await.is_err() {
            return Err(self.terminal_error());
        }
        let result = rx.await.unwrap_or_else(|_| Err(self.terminal_error()));
        if let (Some(transcript), Some(index)) = (&self.transcript, recorded) {
            if let Ok((frame, received_at)) = &result {
                transcript.fill(index, frame.clone(), received_at.clone());
            }
        }
        result
    }

    /// The reader loop's entry point for one decoded frame. `hello_done`
    /// tracks whether the handshake's reply has already been seen: **any**
    /// problem with the very first frame -- a decode violation, or a
    /// frame that isn't the hello reply -- collapses to `PreHelloOutput`
    /// rather than its underlying cause, because (spec/wire.md §3) "the
    /// host has no way to distinguish a banner from a malformed frame."
    fn on_frame(
        &self,
        hello_done: &mut bool,
        frame: String,
        received_at: Rfc3339,
    ) -> Result<(), ProtocolViolationKind> {
        // Envelope validation runs on the frame's own bytes -- id,
        // exactly-one-of ok/err, no unknown or repeated keys -- and the
        // same bytes are then handed to the caller for its typed decode.
        // The payload is skipped rather than materialized here: this layer
        // only needs the id, and the caller decodes the body into the
        // shape its op expects anyway.
        let reply: Reply<serde::de::IgnoredAny> = match serde_json::from_str(&frame) {
            Ok(r) => r,
            Err(_) => {
                return Err(if *hello_done {
                    ProtocolViolationKind::NotJson
                } else {
                    ProtocolViolationKind::PreHelloOutput
                });
            }
        };
        if !*hello_done {
            if reply.id() != RequestId::HELLO {
                return Err(ProtocolViolationKind::PreHelloOutput);
            }
            *hello_done = true;
        }
        match self.deliver(reply.id(), frame, received_at) {
            Ok(_) => Ok(()),
            Err(violation) => Err(violation.into()),
        }
    }
}

/// The outbound pump: pulls queued requests, enforces `max_in_flight` via
/// `semaphore`, purges anything that expired while queued, assigns an id
/// and writes the frame under the request's deadline, then races what is
/// left of that same deadline against the reply in its own task so the
/// pump keeps moving.
///
/// It owns `stdin` for the connection's whole life, so **returning from here
/// closes the child's stdin**. That is the one lever [`Mux::begin_close`]
/// pulls: already-queued requests are drained first (`biased`, queue before
/// close), and only an empty queue lets the close win.
async fn pump(
    mux: Arc<Mux>,
    mut rx: mpsc::Receiver<QueuedRequest>,
    mut stdin: ChildStdin,
    semaphore: Arc<Semaphore>,
    kill_tx: mpsc::Sender<Option<ProtocolViolationKind>>,
) {
    loop {
        let req = tokio::select! {
            biased;
            queued = rx.recv() => match queued {
                Some(req) => req,
                None => return,
            },
            () = mux.closing.notified() => return,
        };
        if let Some(t) = mux.terminal() {
            let _ = req.responder.send(Err(t.into()));
            continue;
        }
        if req.enqueued_at.elapsed() >= req.deadline {
            // Never written: the deadline starts at send, but a request
            // that never gets there has no send to start it from.
            let _ = req.responder.send(Err(HostError::Timeout));
            continue;
        }

        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            let _ = req.responder.send(Err(mux.terminal_error()));
            continue;
        };

        if let Some(t) = mux.terminal() {
            let _ = req.responder.send(Err(t.into()));
            drop(permit);
            continue;
        }
        if req.enqueued_at.elapsed() >= req.deadline {
            let _ = req.responder.send(Err(HostError::Timeout));
            drop(permit);
            continue;
        }

        let Some((id, delivery_rx)) = mux.issue() else {
            let _ = req.responder.send(Err(HostError::IdsExhausted));
            drop(permit);
            continue;
        };
        let line = match build_frame(id, &req.op, &req.params) {
            Ok(line) => line,
            Err(e) => {
                // The host could not serialize its own request. Nothing
                // about the adapter is known to be wrong, so it is not
                // blamed for it.
                let _ = req.responder.send(Err(HostError::Wire(ErrorBody::new(
                    WireErrorCode::Internal,
                    format!("could not serialize {} request: {e}", req.op),
                ))));
                drop(permit);
                continue;
            }
        };

        // One deadline covers the whole send. An adapter that stops
        // reading stdin blocks this write behind a full pipe indefinitely,
        // and nothing queued behind it can move until it returns.
        let send_deadline = tokio::time::Instant::now() + req.deadline;
        match tokio::time::timeout_at(send_deadline, stdin.write_all(line.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                // The pipe is broken, so this request will never be
                // answered -- but *why* it is broken is not known here and
                // frequently is not known yet at all: the exit watcher is
                // still reaping the child, or the reader loop has just
                // asked for a kill over a protocol violation it can name.
                // Resolving the caller now means resolving it with a guess
                // that loses that name. Fall through instead: the id's slot
                // stays pending, `Mux::finish` resolves it with the real
                // terminal reason the moment one is latched, and the
                // deadline below still bounds the wait if none ever is
                // (an adapter that closed its stdin but stayed alive).
            }
            Err(_elapsed) => {
                // A partial frame is already on the adapter's stdin and
                // JSON Lines has no resync point: the rest of it is never
                // written. Tear the connection down instead.
                mux.tombstone(id);
                let _ = req.responder.send(Err(HostError::Timeout));
                drop(permit);
                let _ = kill_tx.try_send(None);
                mux.finish(Terminal::Crashed(None));
                return;
            }
        }

        let mux2 = mux.clone();
        tokio::spawn(async move {
            let outcome = tokio::time::timeout_at(send_deadline, delivery_rx).await;
            let result = match outcome {
                Ok(Ok(Delivery::Reply(frame, received_at))) => Ok((frame, received_at)),
                Ok(Ok(Delivery::Crashed(status))) => Err(HostError::AdapterCrashed { status }),
                Ok(Ok(Delivery::Violation(kind))) => Err(HostError::ProtocolViolation(kind)),
                Ok(Err(_)) => Err(mux2.terminal_error()),
                Err(_elapsed) => {
                    mux2.tombstone(id);
                    Err(HostError::Timeout)
                }
            };
            let _ = req.responder.send(result);
            drop(permit);
        });
    }
}

fn build_frame(id: RequestId, op: &str, params: &Value) -> Result<String, serde_json::Error> {
    let request = sumer_wire::Request::new(id, op.to_owned(), params.clone());
    let mut line = serde_json::to_string(&request)?;
    line.push('\n');
    Ok(line)
}

/// The inbound reader loop: decodes frames off `stdout` and routes each to
/// [`Mux::on_frame`]. On any fatal condition it requests the child's death
/// via `kill_tx` (no resync -- a truncated JSON-Lines stream has no
/// recovery point, spec/wire.md §2) and returns; [`crate::process::supervise`]
/// -- the sole owner of the `Child` handle -- does the actual killing and
/// finishes the mux with the matching [`Terminal::Violation`].
///
/// **Framing is finalized at end of stream.** A decoder still holding bytes
/// when stdout ends stopped in the middle of a frame: those bytes were
/// received and never judged, and returning without looking at them let the
/// boundary drain output it had no verdict on
/// ([`ProtocolViolationKind::UnterminatedFrame`]). The one byte between
/// `garbage\n` and `garbage` used to be the whole difference between a
/// caught violation and a clean exit.
pub async fn read_loop(
    mux: Arc<Mux>,
    mut stdout: tokio::process::ChildStdout,
    kill_tx: mpsc::Sender<Option<ProtocolViolationKind>>,
) {
    use tokio::io::AsyncReadExt;

    let mut decoder = sumer_wire::FrameDecoder::new();
    let mut hello_done = false;
    let mut buf = [0_u8; 8192];
    // Pre-hello, the host cannot tell a banner from a malformed frame
    // (spec/wire.md §3), so every frame-level cause collapses to one kind.
    let classify = |hello_done: bool, kind| {
        if hello_done {
            kind
        } else {
            ProtocolViolationKind::PreHelloOutput
        }
    };
    // Every fatal exit from this loop goes through here, and the ORDER
    // inside it is the contract: the violation is published on the mux
    // *first*, then the kill is requested. `supervise` will latch the same
    // kind as the terminal reason once it has reaped the child, but that
    // is a channel hop and a process wait away -- and a caller holding a
    // reply this connection has already handed it can commit on that reply
    // in between. Publishing at the point of decode is what makes
    // `Mux::violation` answer for the frame the host has just judged
    // rather than for the frame it judged some scheduling ago.
    let fatal = |kind: ProtocolViolationKind| {
        mux.record_violation(kind);
        let _ = kill_tx.try_send(Some(kind));
    };
    loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) => {
                // End of stream. The exit itself is not a violation --
                // `supervise` classifies that once it reaps the status --
                // but an in-progress frame at this point is: nothing will
                // ever terminate it, and it is not the host's to discard.
                if decoder.pending_bytes() > 0 {
                    fatal(classify(
                        hello_done,
                        ProtocolViolationKind::UnterminatedFrame,
                    ));
                }
                return;
            }
            Err(_) => {
                // A read error says the pipe is gone, not what the adapter
                // did: whatever is buffered may have been truncated by the
                // failure itself, so it is not held against the adapter.
                return;
            }
            Ok(n) => n,
        };
        let mut frames = Vec::new();
        let push_result = decoder.push(&buf[..n], &mut frames);
        for frame in frames {
            let received_at = crate::time::now_rfc3339();
            if let Err(kind) = mux.on_frame(&mut hello_done, frame, received_at) {
                fatal(kind);
                return;
            }
        }
        if let Err(kind) = push_result {
            fatal(classify(hello_done, kind));
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A child that never reads or writes anything meaningful -- enough to
    /// give [`Mux::spawn`] a real `ChildStdin` to own, for a test that only
    /// exercises queueing/deadline mechanics and never expects an actual
    /// reply.
    fn silent_child() -> (tokio::process::Child, ChildStdin) {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn `sleep`");
        let stdin = child.stdin.take().expect("piped stdin");
        (child, stdin)
    }

    #[tokio::test]
    async fn id_exhaustion_is_an_explicit_outcome_not_a_panic() {
        // `id` is monotonic and never reused, so the counter can in
        // principle run out. Overflowing it panics the pump (release
        // builds keep overflow-checks on) and abandons the caller with a
        // dropped responder; exhaustion has to be something a caller is
        // told about instead.
        let (mut child, stdin) = silent_child();
        let (kill_tx, _kill_rx) = mpsc::channel(1);
        let mux = Mux::spawn(stdin, kill_tx);
        mux.inner.lock().unwrap_or_else(|e| e.into_inner()).next_id = u64::MAX;

        let result = mux
            .call(
                "noop".to_owned(),
                serde_json::json!({}),
                Duration::from_millis(200),
            )
            .await;
        assert!(
            matches!(result, Err(HostError::IdsExhausted)),
            "id exhaustion must be an explicit outcome, not a dropped responder: {result:?}"
        );

        let _ = child.start_kill();
    }

    /// **A violation cannot be published while the verdict is held.**
    ///
    /// The read half of this is easy to get accidentally right and just as
    /// easy to get wrong: a caller that reads the verdict and then acts on
    /// it has released the lock, and both publishers -- `record_violation`
    /// on the reader task and `finish` on the supervisor -- run on other
    /// threads. No absence of `.await` in the caller excludes them.
    ///
    /// # Why this asserts exclusion and not a counter
    ///
    /// The tempting shape is a publisher hammering in a loop and an
    /// assertion that its counter did not move. That is unsound twice
    /// over: the counter not moving is also what you see when the
    /// publisher had not reached the lock yet (a false PASS on a broken
    /// build), and the increment lives outside the lock, so a scheduler
    /// hiccup can move it after a correct hold released (a false FAIL on a
    /// good one). A flaky guard on a safety property teaches people to
    /// re-run.
    ///
    /// So the witness asserts the exclusion directly: from inside the held
    /// section, by a handshake that cannot happen anywhere else, it
    /// `try_lock`s the very latch both publishers write. Held means the
    /// lock is taken -- there is nothing to time and nothing to lose a
    /// race to. The channel round-trip pins the observation inside the
    /// window from both ends: the witness cannot look before the hold is
    /// entered (it is waiting on `opened`), and the hold cannot return
    /// before the witness has looked (it is waiting on `observed`).
    ///
    /// # Which weaker implementation would pass this?
    ///
    /// Not a stale snapshot (`violation()` then act): nothing is held, so
    /// the try_lock succeeds. Not a re-read just before the act: same.
    /// Not a hold that releases before the act: the witness looks during
    /// the act. Not a hold on some OTHER lock: the witness takes this one,
    /// and the second half -- a real `record_violation` that lands only
    /// after the hold -- pins that this latch is the one publication goes
    /// through.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_violation_cannot_be_published_while_the_verdict_is_held() {
        let (mut child, stdin) = silent_child();
        let (kill_tx, _kill_rx) = mpsc::channel(1);
        let mux = Mux::spawn(stdin, kill_tx);

        let (opened_tx, opened_rx) = std::sync::mpsc::channel::<()>();
        let (observed_tx, observed_rx) = std::sync::mpsc::channel::<bool>();
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let witness = {
            let mux = mux.clone();
            let published = published.clone();
            std::thread::spawn(move || {
                opened_rx.recv().expect("the hold to open the window");
                let free = mux.violation_latch_is_free();
                // A real publisher, on the real path, taking the real
                // lock. It cannot complete until the hold releases; the
                // assertions below check both halves of that.
                let publisher = {
                    let mux = mux.clone();
                    let published = published.clone();
                    std::thread::spawn(move || {
                        mux.record_violation(ProtocolViolationKind::DuplicateId);
                        published.store(true, std::sync::atomic::Ordering::SeqCst);
                    })
                };
                observed_tx
                    .send(free)
                    .expect("the hold to still be waiting");
                publisher.join().expect("the publisher thread");
            })
        };

        let (free, published_during) = mux.with_violation_held(|_| {
            opened_tx.send(()).expect("the witness thread");
            let free = observed_rx.recv().expect("the witness to report");
            (free, published.load(std::sync::atomic::Ordering::SeqCst))
        });
        witness.join().expect("the witness thread");

        assert!(
            !free,
            "the violation latch was free while the verdict was held: whatever this \
             connection was about to commit was decided on a verdict any other thread \
             could still move"
        );
        assert!(
            !published_during,
            "a violation was published while the verdict was held"
        );
        assert!(
            published.load(std::sync::atomic::Ordering::SeqCst),
            "and it lands once the hold is released -- held, not lost"
        );
        assert_eq!(
            mux.violation(),
            Some(ProtocolViolationKind::DuplicateId),
            "through the same latch the witness proved was locked"
        );

        let _ = child.start_kill();
    }

    #[tokio::test]
    async fn queued_request_expires_before_ever_being_sent() {
        // max_in_flight defaults to 1 (the semaphore starts with exactly
        // one permit), so a call that never gets a reply holds that
        // permit forever -- long enough for a second, short-deadline call
        // to sit queued past its own deadline without the pump ever
        // reaching it.
        let (mut child, stdin) = silent_child();
        let (kill_tx, _kill_rx) = mpsc::channel(1);
        let mux = Mux::spawn(stdin, kill_tx);

        // Deliberately short (not the crate's 30s default): dropping a
        // tokio `Runtime` blocks until every task it spawned finishes,
        // including the pump's internal per-request timeout race this
        // call starts -- so this test's wall-clock cost is bounded by
        // whichever deadline here is largest.
        let occupying_deadline = Duration::from_millis(300);
        let occupying = mux.clone();
        tokio::spawn(async move {
            let _ = occupying
                .call("noop".to_owned(), serde_json::json!({}), occupying_deadline)
                .await;
        });
        // Give the pump a moment to dequeue the occupying call and take
        // the sole permit.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = mux
            .call(
                "noop".to_owned(),
                serde_json::json!({}),
                Duration::from_millis(100),
            )
            .await;
        assert!(
            matches!(result, Err(HostError::Timeout)),
            "expected the queued call to expire without ever being sent, got {result:?}"
        );

        let _ = child.start_kill();
    }
}
