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
//! permit). Once actually written, a fresh `deadline`-long timer starts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sumer_wire::{ProtocolViolationKind, Reply, RequestId, Rfc3339};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, Semaphore};

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
    Reply(Reply<Value>, Rfc3339),
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
    outbox: mpsc::Sender<QueuedRequest>,
    /// Concurrency gate for `max_in_flight`. Starts at 1 permit (enough for
    /// the hello call); [`Mux::raise_concurrency`] tops it up once the
    /// adapter's declared value is known.
    semaphore: Arc<Semaphore>,
}

struct QueuedRequest {
    op: String,
    params: Value,
    deadline: Duration,
    enqueued_at: Instant,
    responder: oneshot::Sender<Result<(Reply<Value>, Rfc3339), HostError>>,
}

impl Mux {
    /// Builds a `Mux` and starts its pump task, which owns `stdin` for the
    /// lifetime of the connection. `max_in_flight` starts at `1` (enough
    /// for the hello call, which is sent through the same machinery as any
    /// other call); call [`Mux::raise_concurrency`] once hello succeeds to
    /// open it up to the adapter's declared value.
    #[must_use]
    pub fn spawn(stdin: ChildStdin) -> Arc<Mux> {
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let semaphore = Arc::new(Semaphore::new(1));
        let mux = Arc::new(Mux {
            inner: Mutex::new(MuxInner {
                next_id: 0,
                slots: HashMap::new(),
                terminal: None,
            }),
            outbox: tx,
            semaphore: semaphore.clone(),
        });
        tokio::spawn(pump(mux.clone(), rx, stdin, semaphore));
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

    fn issue(&self) -> (RequestId, oneshot::Receiver<Delivery>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let id = inner.next_id;
        inner.next_id += 1;
        let (tx, rx) = oneshot::channel();
        inner.slots.insert(id, Slot::Pending(tx));
        (RequestId(id), rx)
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
        reply: Reply<Value>,
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
                    let _ = tx.send(Delivery::Reply(reply, received_at));
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
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.terminal.is_some() {
            return false;
        }
        inner.terminal = Some(reason);
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

    fn terminal(&self) -> Option<Terminal> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal
    }

    /// Enqueues one request and awaits its outcome. Backpressure is the
    /// bounded channel itself: `send().await` blocks the caller while the
    /// queue is full, per the frozen contract.
    pub async fn call(
        &self,
        op: String,
        params: Value,
        deadline: Duration,
    ) -> Result<(Reply<Value>, Rfc3339), HostError> {
        if let Some(t) = self.terminal() {
            return Err(t.into());
        }
        let (responder, rx) = oneshot::channel();
        let entry = QueuedRequest {
            op,
            params,
            deadline,
            enqueued_at: Instant::now(),
            responder,
        };
        if self.outbox.send(entry).await.is_err() {
            return Err(self
                .terminal()
                .map(HostError::from)
                .unwrap_or(HostError::AdapterCrashed { status: None }));
        }
        rx.await
            .unwrap_or(Err(HostError::AdapterCrashed { status: None }))
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
        value: Value,
        received_at: Rfc3339,
    ) -> Result<(), ProtocolViolationKind> {
        let reply: Reply<Value> = match serde_json::from_value(value) {
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
        match self.deliver(reply.id(), reply, received_at) {
            Ok(_) => Ok(()),
            Err(violation) => Err(violation.into()),
        }
    }
}

/// The outbound pump: pulls queued requests, enforces `max_in_flight` via
/// `semaphore`, purges anything that expired while queued, assigns an id
/// and writes the frame, then races a per-request deadline against the
/// reply in its own task so the pump keeps moving.
async fn pump(
    mux: Arc<Mux>,
    mut rx: mpsc::Receiver<QueuedRequest>,
    mut stdin: ChildStdin,
    semaphore: Arc<Semaphore>,
) {
    while let Some(req) = rx.recv().await {
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
            let _ = req
                .responder
                .send(Err(HostError::AdapterCrashed { status: None }));
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

        let (id, delivery_rx) = mux.issue();
        let line = match build_frame(id, &req.op, &req.params) {
            Ok(line) => line,
            Err(_) => {
                let _ = req
                    .responder
                    .send(Err(HostError::AdapterCrashed { status: None }));
                drop(permit);
                continue;
            }
        };

        if stdin.write_all(line.as_bytes()).await.is_err() {
            // The pipe is broken -- the adapter is dying or dead. The
            // exit-watcher will classify it properly; here we just make
            // sure this one caller doesn't hang forever.
            let _ = req
                .responder
                .send(Err(HostError::AdapterCrashed { status: None }));
            drop(permit);
            continue;
        }

        let deadline = req.deadline;
        let mux2 = mux.clone();
        tokio::spawn(async move {
            let outcome = tokio::time::timeout(deadline, delivery_rx).await;
            let result = match outcome {
                Ok(Ok(Delivery::Reply(reply, received_at))) => Ok((reply, received_at)),
                Ok(Ok(Delivery::Crashed(status))) => Err(HostError::AdapterCrashed { status }),
                Ok(Ok(Delivery::Violation(kind))) => Err(HostError::ProtocolViolation(kind)),
                Ok(Err(_)) => Err(HostError::AdapterCrashed { status: None }),
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
pub async fn read_loop(
    mux: Arc<Mux>,
    mut stdout: tokio::process::ChildStdout,
    kill_tx: mpsc::Sender<Option<ProtocolViolationKind>>,
) {
    use tokio::io::AsyncReadExt;

    let mut decoder = sumer_wire::FrameDecoder::new();
    let mut hello_done = false;
    let mut buf = [0_u8; 8192];
    loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => {
                // EOF or a read error: not itself a protocol violation --
                // `supervise` (racing the same child) will classify this as
                // a crash once it reaps the exit status. Nothing left to
                // read here either way.
                return;
            }
            Ok(n) => n,
        };
        let mut frames = Vec::new();
        let push_result = decoder.push(&buf[..n], &mut frames);
        for frame in frames {
            let received_at = crate::now_rfc3339();
            if let Err(kind) = mux.on_frame(&mut hello_done, frame, received_at) {
                let _ = kill_tx.try_send(Some(kind));
                return;
            }
        }
        if let Err(kind) = push_result {
            let kind = if hello_done {
                kind
            } else {
                ProtocolViolationKind::PreHelloOutput
            };
            let _ = kill_tx.try_send(Some(kind));
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
    async fn queued_request_expires_before_ever_being_sent() {
        // max_in_flight defaults to 1 (the semaphore starts with exactly
        // one permit), so a call that never gets a reply holds that
        // permit forever -- long enough for a second, short-deadline call
        // to sit queued past its own deadline without the pump ever
        // reaching it.
        let (mut child, stdin) = silent_child();
        let mux = Mux::spawn(stdin);

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
