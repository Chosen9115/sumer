//! `sumer-host`: the supervisor that owns one adapter subprocess and
//! multiplexes the four read capabilities over it. [`AdapterHandle`] is the
//! only public entry point into that supervisor; [`paging`] and [`fold`]
//! are separate, reusable pieces of pure logic shared with the conformance
//! suite (frozen contract, section (g)) rather than baked privately into
//! `AdapterHandle` itself.
//!
//! **Honest boundary** (spec/wire.md §9, restated here because it governs
//! everything below): an adapter process is a *crash* boundary, not a
//! security one. It runs as the same uid, on the same filesystem, with
//! inherited file descriptors. What this crate buys is that an adapter
//! panic, memory corruption, or hang cannot corrupt host state or wedge the
//! host process -- not isolation from a hostile adapter reading host memory,
//! `ptrace`-ing the host, or reaching the network. The env allowlist in
//! [`process`] is hygiene, not a defense.

#![forbid(unsafe_code)]

pub mod fold;
mod mux;
pub mod paging;
mod process;
pub mod time;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mux::Mux;
pub use mux::{Exchange, Terminal};
use sumer_wire::{
    Balance, Degraded, ErrorBody, HelloParams, HelloReply, Observation, ProtocolViolationKind,
    ReadOutcome, Reply, ResourceQuery, ResourceStatus, Rfc3339, Staleness, WireErrorCode,
    MAX_OBSERVATION_BYTES, OP_BALANCES_READ, OP_HELLO, OP_HISTORY_READ, OP_RESOURCES_LIST,
    OP_STATUS_READ,
};

/// Default per-request deadline (frozen contract): 30 seconds.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

/// The protocol versions this host offers in its hello (spec/wire.md §4).
/// The adapter must pick one of exactly these; anything else is not a
/// negotiated version, it is an adapter answering a question nobody asked.
pub const OFFERED_PROTOCOLS: &[&str] = &["1"];

/// Every way a call into an adapter can fail. Mirrors spec/wire.md §8: a
/// wire-level `err` (including a reply the host itself could not make
/// sense of -- see the module docs on [`AdapterHandle::call_typed`]) versus
/// the host-side outcomes that never appear on the wire at all.
#[derive(Debug, Clone)]
pub enum HostError {
    /// The adapter answered with an envelope `err`, *or* the host could
    /// not parse a reply/provenance into the shape the contract requires
    /// (treated as `invalid_request`, since a reply the host cannot make
    /// sense of is not meaningfully different from a request it could not
    /// process).
    Wire(ErrorBody),
    /// The host stopped waiting; the id was tombstoned. The connection
    /// survives.
    Timeout,
    /// The adapter process exited while this call was in flight (or before
    /// it could be sent at all).
    AdapterCrashed { status: Option<i32> },
    /// A fatal, connection-ending protocol violation. No resync: the
    /// process has already been killed by the time this is returned.
    ProtocolViolation(ProtocolViolationKind),
    /// The adapter process itself could not be started.
    Spawn(String),
    /// The connection's monotonic id counter is exhausted. Ids are never
    /// reused (spec/wire.md §6), so this connection can issue no further
    /// requests; a new connection starts a new counter.
    IdsExhausted,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Wire(err) => write!(f, "adapter error {:?}: {}", err.code, err.message),
            HostError::Timeout => write!(f, "request timed out"),
            HostError::AdapterCrashed { status } => {
                write!(f, "adapter crashed (status {status:?})")
            }
            HostError::ProtocolViolation(kind) => write!(f, "protocol violation: {kind:?}"),
            HostError::Spawn(msg) => write!(f, "could not spawn adapter: {msg}"),
            HostError::IdsExhausted => write!(f, "adapter connection ran out of request ids"),
        }
    }
}

impl std::error::Error for HostError {}

/// A `balances.read` reply with provenance host-stamped (spec/observation.md
/// §1): `received_at` and `staleness` filled in by the host, never by the
/// adapter.
#[derive(Debug, Clone)]
pub struct BalancesRead {
    pub observations: Vec<Balance>,
    pub statuses: Vec<ResourceStatus>,
}

/// A `history.read` reply with provenance host-stamped. Revision assignment
/// and the live-set fold are a separate step -- see [`fold::Fold`] -- run by
/// whoever is accumulating pages across calls, not by this read itself.
#[derive(Debug, Clone)]
pub struct HistoryRead {
    pub observations: Vec<Observation>,
    pub statuses: Vec<ResourceStatus>,
}

/// A live handle to one running adapter process. The only public API this
/// crate exposes for talking to an adapter -- [`process`] and [`mux`] are
/// private implementation.
pub struct AdapterHandle {
    mux: Arc<Mux>,
    hello: HelloReply,
    default_deadline: Duration,
    kill_tx: tokio::sync::mpsc::Sender<Option<ProtocolViolationKind>>,
    /// `None` unless this handle was built with [`AdapterHandle::spawn_recorded`]
    /// (or its `_with_deadline` sibling) -- see [`AdapterHandle::transcript`].
    transcript: Option<Arc<mux::Transcript>>,
    /// [`process::supervise`]'s task: the one place a terminal reason is
    /// ever latched. `None` once [`AdapterHandle::close`] has taken it.
    supervisor: Option<tokio::task::JoinHandle<()>>,
    /// Set by [`process::supervise`] the instant the child is reaped.
    /// Evidence about the PROCESS only -- never about the stream, which can
    /// outlive it (see [`AdapterHandle::close`]).
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for AdapterHandle {
    fn drop(&mut self) {
        // Best-effort: ask `process::supervise` (the sole owner of the
        // `Child`) to tear the process down. If it has already exited (or
        // the channel is full because a shutdown is already in flight),
        // there is nothing more to do.
        let _ = self.kill_tx.try_send(None);
    }
}

impl AdapterHandle {
    /// Spawns `argv[0]` (with `argv[1..]` as arguments) as an adapter
    /// process and completes the hello handshake, using
    /// [`DEFAULT_DEADLINE`] for every request including hello itself.
    /// `extra_env` is forwarded to the child on top of the host's fixed env
    /// allowlist (`process::ENV_ALLOWLIST`) -- e.g. a conformance runner
    /// setting `SUMER_FIXTURE`/`SUMER_FIXTURE_RUN` for a fake adapter.
    ///
    /// # Errors
    /// [`HostError::Spawn`] if the process itself could not be started;
    /// [`HostError::Wire`] if the adapter rejected the protocol version (or
    /// its hello reply could not be parsed); [`HostError::Timeout`] /
    /// [`HostError::AdapterCrashed`] / [`HostError::ProtocolViolation`] if
    /// the handshake never completed for one of those reasons.
    pub async fn spawn(
        argv: Vec<String>,
        extra_env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<AdapterHandle, HostError> {
        AdapterHandle::spawn_with_deadline(argv, extra_env, DEFAULT_DEADLINE).await
    }

    /// As [`AdapterHandle::spawn`], with an explicit default deadline
    /// instead of [`DEFAULT_DEADLINE`] -- e.g. a conformance case using
    /// `conformance_hints.deadline_ms` to force a host-side timeout without
    /// paying the full default wall-clock cost (spec/wire.md §11).
    pub async fn spawn_with_deadline(
        argv: Vec<String>,
        extra_env: impl IntoIterator<Item = (String, String)>,
        default_deadline: Duration,
    ) -> Result<AdapterHandle, HostError> {
        AdapterHandle::spawn_impl(argv, extra_env, default_deadline, false).await
    }

    /// As [`AdapterHandle::spawn`], additionally recording every
    /// request/reply that crosses this connection so the conformance
    /// suite can derive raw wire evidence from the same execution that
    /// produces the typed reads below -- see [`AdapterHandle::transcript`]
    /// and [`mux::Transcript`]. Production adapters never call this: it
    /// exists only for the suite.
    pub async fn spawn_recorded(
        argv: Vec<String>,
        extra_env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<AdapterHandle, HostError> {
        AdapterHandle::spawn_recorded_with_deadline(argv, extra_env, DEFAULT_DEADLINE).await
    }

    /// As [`AdapterHandle::spawn_recorded`], with an explicit default
    /// deadline instead of [`DEFAULT_DEADLINE`] -- see
    /// [`AdapterHandle::spawn_with_deadline`].
    pub async fn spawn_recorded_with_deadline(
        argv: Vec<String>,
        extra_env: impl IntoIterator<Item = (String, String)>,
        default_deadline: Duration,
    ) -> Result<AdapterHandle, HostError> {
        AdapterHandle::spawn_impl(argv, extra_env, default_deadline, true).await
    }

    async fn spawn_impl(
        argv: Vec<String>,
        extra_env: impl IntoIterator<Item = (String, String)>,
        default_deadline: Duration,
        recorded: bool,
    ) -> Result<AdapterHandle, HostError> {
        let spawned =
            process::spawn(&argv, extra_env).map_err(|e| HostError::Spawn(e.to_string()))?;

        let (kill_tx, kill_rx) = tokio::sync::mpsc::channel(1);
        let (mux, transcript) = if recorded {
            let (mux, transcript) = Mux::spawn_recorded(spawned.stdin, kill_tx.clone());
            (mux, Some(transcript))
        } else {
            (Mux::spawn(spawned.stdin, kill_tx.clone()), None)
        };
        let reader = tokio::spawn(mux::read_loop(mux.clone(), spawned.stdout, kill_tx.clone()));
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let supervisor = tokio::spawn(process::supervise(
            spawned.child,
            kill_rx,
            mux.clone(),
            reader,
            exited.clone(),
        ));

        let offered = HelloParams {
            protocol: OFFERED_PROTOCOLS.iter().map(|v| (*v).to_owned()).collect(),
        };
        let hello_params = serde_json::to_value(&offered).map_err(|e| {
            HostError::Wire(ErrorBody::new(
                WireErrorCode::Internal,
                format!("could not serialize hello params: {e}"),
            ))
        })?;
        let hello_result = mux
            .call(OP_HELLO.to_owned(), hello_params, default_deadline)
            .await;
        let (frame, _received_at) = match hello_result {
            Ok(pair) => pair,
            Err(e) => {
                let _ = kill_tx.try_send(None);
                return Err(e);
            }
        };

        // Decoded from the frame's own bytes, like every other reply.
        let hello = match serde_json::from_str::<Reply<HelloReply>>(&frame) {
            Ok(Reply::Ok { ok, .. }) => ok,
            Ok(Reply::Err { err, .. }) => {
                let _ = kill_tx.try_send(None);
                return Err(HostError::Wire(err));
            }
            Err(e) => {
                let _ = kill_tx.try_send(None);
                return Err(HostError::Wire(ErrorBody::new(
                    WireErrorCode::InvalidRequest,
                    format!("malformed hello reply: {e}"),
                )));
            }
        };

        // The adapter must select one of the versions it was offered.
        // Accepting anything else means running an unnegotiated protocol:
        // the host would go on to speak "1" at an adapter that just said it
        // speaks something else, and every later reply would be interpreted
        // under a contract neither side agreed to.
        if !OFFERED_PROTOCOLS.contains(&hello.protocol.as_str()) {
            let _ = kill_tx.try_send(None);
            return Err(HostError::Wire(
                ErrorBody::new(
                    WireErrorCode::UnsupportedProtocol,
                    format!(
                        "adapter selected protocol {:?}, which was not offered",
                        hello.protocol
                    ),
                )
                .with_detail(serde_json::json!({"offered": OFFERED_PROTOCOLS})),
            ));
        }

        mux.raise_concurrency(hello.max_in_flight.max(1));

        Ok(AdapterHandle {
            mux,
            hello,
            default_deadline,
            kill_tx,
            transcript,
            supervisor: Some(supervisor),
            exited,
        })
    }

    #[must_use]
    pub fn hello(&self) -> &HelloReply {
        &self.hello
    }

    /// A snapshot of every request/reply recorded on this connection so
    /// far, in dispatch order -- empty unless this handle was built with
    /// [`AdapterHandle::spawn_recorded`] (or its `_with_deadline`
    /// sibling), which is the only case that ever populates it.
    #[must_use]
    pub fn transcript(&self) -> Vec<Exchange> {
        self.transcript
            .as_ref()
            .map(|t| t.snapshot())
            .unwrap_or_default()
    }

    /// Whether this connection has been **established** to have broken the
    /// wire contract, as of the instant this is called.
    ///
    /// A live query, not a value handed out earlier: the whole reason it
    /// exists is that a violation can arrive *behind* a reply this
    /// connection already delivered successfully. The reply is delivered
    /// -- a delivered reply is delivered, and the host does not retract one
    /// -- but the caller about to act on it can still ask whether anything
    /// has since disqualified the connection that served it. That is
    /// `spec/observation.md` §8.1 condition (9)'s "at the moment of
    /// commitment", and `sumer_store::sweep` asks it inside the same
    /// transaction that commits a retraction.
    ///
    /// Honest about what it answers: what the host has JUDGED by now, not
    /// what the adapter has written. Bytes still in the pipe are nobody's
    /// violation yet. What it does guarantee is that everything the reader
    /// decoded before it handed over the reply you are holding has already
    /// been judged -- the reader publishes a violation before it stops.
    ///
    /// `None` on a connection the host has caught doing nothing wrong,
    /// including one that merely failed, timed out, or died: those are
    /// failures in the vocabulary the contract provides, not violations of
    /// it.
    #[must_use]
    pub fn contract_violation(&self) -> Option<ProtocolViolationKind> {
        self.mux.violation()
    }

    /// Ends this connection and reports **how it ended**.
    ///
    /// Closing the child's stdin (see [`mux::Mux::begin_close`]) makes a
    /// well-behaved adapter exit; its exit closes stdout; the reader loop
    /// decodes every remaining byte and finalizes the framing at that end
    /// of stream; and [`process::supervise`] then latches the terminal
    /// reason.
    ///
    /// This is what makes "the adapter answered everything I asked and
    /// then broke the protocol" observable at all. Without a close there is
    /// no last moment to look at: the final reply is delivered before the
    /// reader has even parsed what follows it, so a caller that judges and
    /// walks away never learns the connection died of a violation.
    ///
    /// # What this boundary does and does not establish
    ///
    /// Awaiting the supervisor is **not** by itself proof that the stream
    /// ended -- a process that inherited the adapter's stdout write end
    /// keeps it open after the adapter is reaped, and a bounded wait for a
    /// stream that never ends is a wait that gives up. So three distinct
    /// facts are kept apart here, and each has its own outcome:
    ///
    /// * **The process exited and the reader loop returned.** The terminal
    ///   reason is whatever the bytes it did decode produced --
    ///   `Terminal::Crashed(status)` for a cooperative exit, a
    ///   `Terminal::Violation` if the last bytes broke the protocol.
    ///
    ///   What this does *not* establish is that every byte the adapter
    ///   ever wrote was decoded and judged. The reader loop also returns
    ///   normally on a **read error** -- the pipe failed, whatever was
    ///   buffered may have been truncated by the failure rather than by
    ///   the adapter, and the host declines to hold that against it -- and
    ///   `supervise` reads the reader task finishing as drainage either
    ///   way. So the honest claim is "the reader ran to completion and
    ///   nothing it decoded was a violation", not "every byte was judged".
    /// * **The process exited, the reader did not finish** (in
    ///   `process::READER_DRAIN`, or by the time this deadline expires):
    ///   [`ProtocolViolationKind::StdoutHeldOpen`]. The host cannot say it
    ///   read everything, and saying the connection ended cleanly would be
    ///   certifying a drain it did not perform. It equally cannot say
    ///   *what* kept the stream open: a forked writer and a reader task
    ///   the runtime did not get back to inside the bound look the same
    ///   from here (see `process::READER_DRAIN`).
    /// * **The process is still running** when `default_deadline` expires:
    ///   [`ProtocolViolationKind::StdinEofIgnored`] -- spec/wire.md §7,
    ///   **an adapter MUST exit when its stdin reaches EOF**. This kind is
    ///   about the PROCESS, and it is assigned only on evidence about the
    ///   process (`exited`), never on the supervisor merely taking too
    ///   long: an adapter that exited promptly and left a slow drain
    ///   behind it would otherwise be blamed for a rule it kept.
    ///
    /// `Drop` (below) kills whatever is left on the way out.
    ///
    /// `None` keeps the meaning it always had -- no terminal reason was
    /// ever latched, i.e. a deliberate non-violation shutdown. A
    /// cooperative exit is not `None`: it is `Terminal::Crashed(status)`
    /// carrying the process's own exit code, which this boundary reads as
    /// an ordinary end rather than a violation.
    pub async fn close(mut self) -> Option<Terminal> {
        self.mux.begin_close();
        if let Some(supervisor) = self.supervisor.take() {
            if tokio::time::timeout(self.default_deadline, supervisor)
                .await
                .is_err()
            {
                // The supervisor did not finish. Two very different things
                // look like that from here, and `exited` is what tells them
                // apart -- see the doc comment. `finish` is first-wins, so
                // either can only ever be the reason when nothing truer was
                // latched.
                let kind = if self.exited.load(std::sync::atomic::Ordering::SeqCst) {
                    ProtocolViolationKind::StdoutHeldOpen
                } else {
                    ProtocolViolationKind::StdinEofIgnored
                };
                self.mux.finish(Terminal::Violation(kind));
            }
        }
        self.mux.terminal()
    }

    /// `resources.list`: no observations, no per-resource status (nothing
    /// was requested before discovery -- spec/observation.md, Contract
    /// Amendment 1 Ruling A4).
    pub async fn resources_list(&self) -> Result<sumer_wire::ResourcesListReply, HostError> {
        self.call_typed(OP_RESOURCES_LIST, sumer_wire::ResourcesListParams {})
            .await
    }

    /// `status.read`: reachability/credential state per resource. No
    /// observations, no provenance to stamp.
    pub async fn status_read(
        &self,
        resource_ids: Vec<String>,
    ) -> Result<sumer_wire::StatusReadReply, HostError> {
        let params = sumer_wire::StatusReadParams { resource_ids };
        let reply: sumer_wire::StatusReadReply = self.call_typed(OP_STATUS_READ, &params).await?;
        check_status_coverage(
            OP_STATUS_READ,
            params.resource_ids.iter().map(String::as_str),
            &reply.statuses,
        )?;
        Ok(reply)
    }

    /// `balances.read`: batched, not paginated (Contract Amendment 1 Ruling
    /// A3). Provenance is host-stamped on the way out -- see the docs on
    /// [`AdapterHandle::call_typed_with_receipt`].
    pub async fn balances_read(
        &self,
        resource_ids: Vec<String>,
    ) -> Result<BalancesRead, HostError> {
        let params = sumer_wire::BalancesReadParams { resource_ids };
        let (raw, received_at): (sumer_wire::BalancesReadReply, Rfc3339) = self
            .call_typed_with_receipt(OP_BALANCES_READ, &params)
            .await?;
        check_status_coverage(
            OP_BALANCES_READ,
            params.resource_ids.iter().map(String::as_str),
            &raw.statuses,
        )?;
        // **A balances reply answers for this connection's adapter, about
        // the resources this call asked for, and nothing else.** Two shapes
        // are refused, and the whole reply with each of them
        // (`spec/observation.md` §2):
        //
        // - a balance whose `provenance.adapter_id` is not the connection's
        //   own. The contradiction is detectable only here: one layer down
        //   the caller's `adapter_id` is all that is left, and the figure
        //   is filed under it, silently reattributing another provider's
        //   money.
        // - a balance for a resource this call did not ask about. The
        //   request bounds what the reply may answer -- nothing in this
        //   refresh listed that resource, so nothing established that it
        //   still exists. Stored anyway it lands against the CURRENT read
        //   and, with no status entry the host asked for behind it, on the
        //   `Live` staleness and the synthesized outcome a missing entry
        //   defaults to: a figure rendered `live` on a §6 outcome nobody
        //   ever gave.
        //
        // Whole rather than line by line, for the reason §2 gives: a
        // history page still faces a gate that can disqualify a sweep and
        // keep what was honest, and a balances reply faces nothing. Safe,
        // because freshness is derived -- a refused read writes no row, so
        // what is on screen goes stale rather than staying `live`.
        if let Some(foreign) = raw
            .observations
            .iter()
            .find(|b| b.provenance.adapter_id != self.hello.adapter_id)
        {
            return Err(HostError::Wire(ErrorBody::new(
                WireErrorCode::InvalidRequest,
                format!(
                    "malformed {OP_BALANCES_READ} reply: a balance for resource {:?} names \
                     adapter_id {:?} on the connection that announced {:?}",
                    foreign.resource_id, foreign.provenance.adapter_id, self.hello.adapter_id
                ),
            )));
        }
        let asked: HashSet<&str> = params.resource_ids.iter().map(String::as_str).collect();
        if let Some(unasked) = raw
            .observations
            .iter()
            .find(|b| !asked.contains(b.resource_id.as_str()))
        {
            return Err(HostError::Wire(ErrorBody::new(
                WireErrorCode::InvalidRequest,
                format!(
                    "malformed {OP_BALANCES_READ} reply: a balance for resource {:?}, which \
                     this call did not request",
                    unasked.resource_id
                ),
            )));
        }
        let staleness = staleness_by_resource(&raw.statuses);
        let mut statuses = raw.statuses;
        let observations = drop_oversized(
            raw.observations,
            &mut statuses,
            |_| true,
            |b| (b.resource_id.clone(), None),
        )
        .into_iter()
        .map(|wire| {
            let stale = staleness_for(&staleness, &wire.resource_id);
            Balance::stamp(wire, received_at.clone(), stale)
        })
        .collect();
        Ok(BalancesRead {
            observations,
            statuses,
        })
    }

    /// `history.read`: batched, per-resource cursor (Contract Amendment 1
    /// Ruling A3). Provenance is host-stamped; revision assignment and the
    /// live-set fold are a separate step -- feed the returned observations
    /// into [`Fold::ingest`].
    pub async fn history_read(
        &self,
        resources: Vec<ResourceQuery>,
    ) -> Result<HistoryRead, HostError> {
        let params = sumer_wire::HistoryReadParams { resources };
        let (raw, received_at): (sumer_wire::HistoryReadReply, Rfc3339) = self
            .call_typed_with_receipt(OP_HISTORY_READ, &params)
            .await?;
        check_status_coverage(
            OP_HISTORY_READ,
            params.resources.iter().map(|r| r.resource_id.as_str()),
            &raw.statuses,
        )?;
        let staleness = staleness_by_resource(&raw.statuses);
        let mut statuses = raw.statuses;
        // An observation naming ANOTHER adapter is not measured and not
        // dropped here: `spec/observation.md` §8.1 condition (8) refuses it
        // outright and disqualifies the sweep, and that ruling is stronger
        // than §6's degrade. Dropping it here instead would file it as a
        // NAMED degrade -- which merely EXEMPTS its `local_id` from
        // retraction -- so a foreign record would suppress a retraction
        // rather than block one, exactly the inversion §8.1 forbids ("a
        // refused observation ... can never suppress a retraction"). It is
        // passed through to the one place that can tell the difference,
        // which refuses it and stores nothing.
        let own = self.hello.adapter_id.clone();
        let observations = drop_oversized(
            raw.observations,
            &mut statuses,
            |o: &sumer_wire::ObservationWire| o.provenance.adapter_id == own,
            |o| (o.resource_id.clone(), Some(o.local_id.clone())),
        )
        .into_iter()
        .map(|wire| {
            let stale = staleness_for(&staleness, &wire.resource_id);
            Observation::stamp(wire, received_at.clone(), stale)
        })
        .collect();
        Ok(HistoryRead {
            observations,
            statuses,
        })
    }

    /// Sends one request with an arbitrary op string and raw params, and
    /// returns the envelope verbatim -- `err` included, undecoded.
    ///
    /// The four typed reads above cover every op this milestone defines, so
    /// this exists for the one thing they cannot express: probing an op the
    /// adapter never declared (`spec/wire.md` §5's `unsupported` path), which
    /// the conformance suite has to do on a real connection. It goes through
    /// the same `process::spawn` (env allowlist) and the same
    /// `FrameDecoder` (frame cap, UTF-8, id lifecycle) as every other call,
    /// which a second, hand-rolled client would not.
    ///
    /// # Errors
    /// The same host-side outcomes as any other call. An envelope `err` is
    /// returned as `Ok(Reply::Err {..})`, not `Err`: probing for `err` is
    /// the point.
    pub async fn call_raw(
        &self,
        op: &str,
        params: serde_json::Value,
    ) -> Result<Reply<serde_json::Value>, HostError> {
        let (frame, _received_at) = self
            .mux
            .call(op.to_owned(), params, self.default_deadline)
            .await?;
        serde_json::from_str(&frame).map_err(|e| {
            HostError::Wire(ErrorBody::new(
                WireErrorCode::InvalidRequest,
                format!("malformed {op} reply: {e}"),
            ))
        })
    }

    /// Sends one call and decodes its `ok` payload as `R`, discarding the
    /// frame's host-stamped receipt time -- for the two ops
    /// ([`sumer_wire::ResourcesListReply`], [`sumer_wire::StatusReadReply`])
    /// that carry no provenance to stamp with it.
    async fn call_typed<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        params: P,
    ) -> Result<R, HostError> {
        let (value, _received_at) = self.call_typed_with_receipt(op, params).await?;
        Ok(value)
    }

    /// As [`AdapterHandle::call_typed`], also returning the host-stamped
    /// receipt time of the reply frame -- captured by the reader loop the
    /// instant the frame was decoded (see `mux::Mux::deliver`), not
    /// recomputed here after crossing a channel and being rescheduled.
    /// This is `received_at`: the timestamp spec/observation.md §1 requires
    /// the host to stamp, and the one an inbound `Provenance` is forbidden
    /// from carrying itself (enforced structurally by
    /// [`sumer_wire::ProvenanceWire`] -- an adapter that sends it fails to
    /// deserialize, which surfaces here as an ordinary malformed-reply
    /// `invalid_request`, the same path taken by any other shape mismatch).
    ///
    /// Staleness is stamped alongside it, derived per resource from the
    /// same reply's `statuses` -- see [`staleness_by_resource`]. This
    /// function returns the undecorated payload; the two reads that carry
    /// provenance ([`AdapterHandle::balances_read`],
    /// [`AdapterHandle::history_read`]) do the stamping.
    async fn call_typed_with_receipt<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        params: P,
    ) -> Result<(R, Rfc3339), HostError> {
        let params_value = serde_json::to_value(params).map_err(|e| {
            HostError::Wire(ErrorBody::new(
                WireErrorCode::Internal,
                format!("could not serialize {op} params: {e}"),
            ))
        })?;
        let (frame, received_at) = self
            .mux
            .call(op.to_owned(), params_value, self.default_deadline)
            .await?;
        // Straight from the frame's bytes to `R`: no `serde_json::Value`
        // in between, which would collapse a duplicate key inside `ok`
        // before `R`'s own deserializer ever saw it.
        let reply = serde_json::from_str::<Reply<R>>(&frame).map_err(|e| {
            HostError::Wire(ErrorBody::new(
                WireErrorCode::InvalidRequest,
                format!("malformed {op} reply: {e}"),
            ))
        })?;
        match reply {
            Reply::Err { err, .. } => Err(HostError::Wire(err)),
            Reply::Ok { ok, .. } => Ok((ok, received_at)),
        }
    }
}

// ---------------------------------------------------------------------
// Staleness (spec/observation.md §1)
// ---------------------------------------------------------------------

/// Staleness per `resource_id`, derived from that resource's own outcome in
/// the same reply.
///
/// The host stamps this; the adapter cannot send it (there is no field for
/// it on [`sumer_wire::ProvenanceWire`]). But "host-stamped" never meant
/// "host-invented": an adapter that answers `stale { as_of }` has said, in
/// the vocabulary the contract gives it, that what it is handing over is
/// not current -- stamping `Live` over that would be the host overruling
/// evidence it asked for. Everything else is a live read: these
/// observations came off the wire moments ago, and there is no cache layer
/// yet that could make them anything else.
fn staleness_by_resource(statuses: &[ResourceStatus]) -> HashMap<String, Staleness> {
    statuses
        .iter()
        .map(|status| {
            let staleness = match status.outcome {
                ReadOutcome::Stale { .. } => Staleness::Cached,
                // Nothing current exists for this resource at all --
                // distinct from "old but real", which is `Cached`.
                ReadOutcome::Unavailable | ReadOutcome::Gone => Staleness::Unavailable,
                // Listed rather than wildcarded so a new outcome has to
                // choose: an outcome nobody classified would silently
                // become `Live`, which is the one value that must never be
                // a default nobody thought about.
                ReadOutcome::Fetched { .. }
                | ReadOutcome::NotFetched
                | ReadOutcome::RateLimited { .. }
                | ReadOutcome::ReauthRequired
                | ReadOutcome::Revoked
                | ReadOutcome::ScaRequired => Staleness::Live,
            };
            (status.resource_id.clone(), staleness)
        })
        .collect()
}

/// The staleness this observation's resource reported. The fallback is
/// reachable only for an observation naming a resource this call did not
/// request -- refused outright on a `balances.read` (spec/observation.md
/// §2), and not part of any swept resource's page on a `history.read`. It
/// is `Unavailable` rather than `Live` because a default is a claim, and
/// `Live` is the one claim nothing here has the evidence to make.
fn staleness_for(by_resource: &HashMap<String, Staleness>, resource_id: &str) -> Staleness {
    by_resource
        .get(resource_id)
        .copied()
        .unwrap_or(Staleness::Unavailable)
}

/// **Every requested `resource_id` appears in `statuses` exactly once**
/// (spec/observation.md §6) -- never twice, never zero times. Refused here,
/// where the reply is decoded, rather than defended against in each of the
/// readers downstream: staleness stamping above, the retraction gate in
/// `sumer-store`, the balance outcome label. Every one of them reaches for
/// one entry per resource and takes the first match, and every one of them
/// spells an absent entry as its own permissive default. §6 carries the
/// reasoning, including which default is the dangerous one.
fn check_status_coverage<'a>(
    op: &str,
    requested: impl IntoIterator<Item = &'a str>,
    statuses: &[ResourceStatus],
) -> Result<(), HostError> {
    let malformed = |detail: String| {
        HostError::Wire(ErrorBody::new(
            WireErrorCode::InvalidRequest,
            format!(
                "malformed {op} reply: {detail}; every requested resource_id appears in \
                 `statuses` exactly once"
            ),
        ))
    };
    let mut seen: HashSet<&str> = HashSet::new();
    for status in statuses {
        if !seen.insert(status.resource_id.as_str()) {
            return Err(malformed(format!(
                "resource {:?} appears in `statuses` more than once",
                status.resource_id
            )));
        }
    }
    for resource_id in requested {
        if !seen.contains(resource_id) {
            return Err(malformed(format!(
                "resource {resource_id:?} was requested and does not appear in `statuses` at all"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// MAX_OBSERVATION_BYTES enforcement (spec/observation.md §6)
// ---------------------------------------------------------------------

/// Drops any observation whose serialized size exceeds
/// [`MAX_OBSERVATION_BYTES`] and reports it in its resource's `degraded`
/// field, continuing the page with everything that fits.
///
/// The adapter is supposed to have done this itself (truncate
/// `provider_extra`, then omit the record and report it). The host repeats
/// the omit half because "the adapter promised" is not enforcement: an
/// adapter that skips the degrade otherwise lands an unbounded record in
/// host memory and in every consumer downstream of it. The degrade stays a
/// degrade -- **a resource is never bricked by one large event** -- so the
/// rest of the page is delivered untouched.
///
/// Size is measured on the host's own re-serialization of the decoded
/// observation, which is the only copy the host can vouch for; it differs
/// from the adapter's bytes only by JSON whitespace and key order.
fn drop_oversized<T: serde::Serialize>(
    observations: Vec<T>,
    statuses: &mut [ResourceStatus],
    measure: impl Fn(&T) -> bool,
    key: impl Fn(&T) -> (String, Option<String>),
) -> Vec<T> {
    observations
        .into_iter()
        .filter(|observation| {
            if !measure(observation) {
                return true;
            }
            let bytes = serde_json::to_vec(observation).map_or(usize::MAX, |v| v.len());
            if bytes <= MAX_OBSERVATION_BYTES {
                return true;
            }
            let (resource_id, local_id) = key(observation);
            report_oversized(statuses, resource_id, local_id, bytes);
            false
        })
        .collect()
}

/// Records the dropped record on one resource's status. It **appends to**
/// that entry's `degraded` list rather than adding a second status entry --
/// **every requested `resource_id` appears in `statuses` exactly once**
/// (spec/observation.md §6) -- and rather than replacing its `outcome`,
/// which carries a different fact: how fresh what this resource *did*
/// deliver is. Overwriting `stale { as_of }` here would mis-stamp a
/// perfectly good cached sibling as `Live`.
///
/// **Appends, and never overwrites.** This runs once per dropped record, so
/// a single slot lost every drop but the last: two oversized records on one
/// page left the first unexplained, and an unexplained absence is retracted.
/// Worse, an *anonymous* degrade the adapter itself reported -- which
/// disqualifies the sweep (§8.1 condition 5) -- was overwritten by this
/// host-authored NAMED one, which merely exempts one id. That silently
/// converted a disqualifying signal into an exempting one and retracted a
/// live record. Nothing the host writes here can weaken what the adapter
/// said.
///
/// **And nothing the host writes here can INVENT what the adapter did not
/// say.** `statuses` is a slice, not a `Vec`, so this cannot grow it -- an
/// oversized record for a resource with no status entry of its own is
/// dropped and nothing is recorded against it. It used to push a
/// host-authored `ResourceStatus { outcome: fetched }` for that resource,
/// which is the most permissive outcome §6 has and one no adapter ever
/// reported. That entry was inert only because every reader downstream
/// happens to look statuses up by the resource it asked about; "inert
/// because nobody currently reads it" is not a property a host-authored
/// outcome is allowed to rest on. The arm is now unrepresentable rather
/// than unused, and the case it covered is a reply that was already
/// off-contract: only an observation for a resource this call did not
/// request can reach it (`check_status_coverage` gives every requested one
/// a status entry), and such an observation is refused outright on a
/// balances reply and dropped unstored by the sweep on a history one.
fn report_oversized(
    statuses: &mut [ResourceStatus],
    resource_id: String,
    local_id: Option<String>,
    bytes: usize,
) {
    if let Some(existing) = statuses
        .iter_mut()
        .find(|status| status.resource_id == resource_id)
    {
        existing.degraded.push(Degraded {
            local_id,
            bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
        });
    }
}
