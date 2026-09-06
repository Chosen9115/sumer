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

use std::sync::Arc;
use std::time::Duration;

use mux::Mux;
use sumer_wire::{
    Balance, ErrorBody, HelloReply, Observation, ProtocolViolationKind, Reply, ResourceQuery,
    ResourceStatus, Rfc3339, Staleness, WireErrorCode, OP_BALANCES_READ, OP_HELLO, OP_HISTORY_READ,
    OP_RESOURCES_LIST, OP_STATUS_READ,
};

/// Default per-request deadline (frozen contract): 30 seconds.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

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
    stderr_tail: Arc<std::sync::Mutex<Vec<u8>>>,
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
        let spawned =
            process::spawn(&argv, extra_env).map_err(|e| HostError::Spawn(e.to_string()))?;
        let stderr_tail = spawned.stderr_tail.clone();

        let mux = Mux::spawn(spawned.stdin);
        let (kill_tx, kill_rx) = tokio::sync::mpsc::channel(1);
        tokio::spawn(mux::read_loop(mux.clone(), spawned.stdout, kill_tx.clone()));
        tokio::spawn(process::supervise(spawned.child, kill_rx, mux.clone()));

        let hello_params = serde_json::json!({ "protocol": ["1"] });
        let hello_result = mux
            .call(OP_HELLO.to_owned(), hello_params, default_deadline)
            .await;
        let (reply, _received_at) = match hello_result {
            Ok(pair) => pair,
            Err(e) => {
                let _ = kill_tx.try_send(None);
                return Err(e);
            }
        };

        let ok_value = match reply {
            Reply::Err { err, .. } => {
                let _ = kill_tx.try_send(None);
                return Err(HostError::Wire(err));
            }
            Reply::Ok { ok, .. } => ok,
        };
        let hello: HelloReply = match serde_json::from_value(ok_value) {
            Ok(h) => h,
            Err(e) => {
                let _ = kill_tx.try_send(None);
                return Err(HostError::Wire(ErrorBody::new(
                    WireErrorCode::InvalidRequest,
                    format!("malformed hello reply: {e}"),
                )));
            }
        };

        mux.raise_concurrency(hello.max_in_flight.max(1));

        Ok(AdapterHandle {
            mux,
            hello,
            default_deadline,
            kill_tx,
            stderr_tail,
        })
    }

    #[must_use]
    pub fn hello(&self) -> &HelloReply {
        &self.hello
    }

    /// The adapter's stderr, up to the last `process`-module cap, drained
    /// in the background for the life of the process. Free-form, never
    /// parsed (spec/wire.md §3) -- for diagnostics only, e.g. after an
    /// [`HostError::AdapterCrashed`].
    #[must_use]
    pub fn stderr_tail(&self) -> Vec<u8> {
        self.stderr_tail
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
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
        self.call_typed(
            OP_STATUS_READ,
            sumer_wire::StatusReadParams { resource_ids },
        )
        .await
    }

    /// `balances.read`: batched, not paginated (Contract Amendment 1 Ruling
    /// A3). Provenance is host-stamped on the way out -- see the docs on
    /// [`AdapterHandle::call_typed_with_receipt`].
    pub async fn balances_read(
        &self,
        resource_ids: Vec<String>,
    ) -> Result<BalancesRead, HostError> {
        let (raw, received_at): (sumer_wire::BalancesReadReply, Rfc3339) = self
            .call_typed_with_receipt(
                OP_BALANCES_READ,
                sumer_wire::BalancesReadParams { resource_ids },
            )
            .await?;
        let observations = raw
            .observations
            .into_iter()
            .map(|wire| Balance::stamp(wire, received_at.clone(), Staleness::Live))
            .collect();
        Ok(BalancesRead {
            observations,
            statuses: raw.statuses,
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
        let (raw, received_at): (sumer_wire::HistoryReadReply, Rfc3339) = self
            .call_typed_with_receipt(OP_HISTORY_READ, sumer_wire::HistoryReadParams { resources })
            .await?;
        let observations = raw
            .observations
            .into_iter()
            .map(|wire| Observation::stamp(wire, received_at.clone(), Staleness::Live))
            .collect();
        Ok(HistoryRead {
            observations,
            statuses: raw.statuses,
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
    /// **Staleness in this milestone is always [`Staleness::Live`]**: the
    /// contract requires the host to compute it from `received_at`, but
    /// gives no threshold, and explicitly defers "clock-skew policy beyond
    /// stamping `received_at`" (frozen contract, "Deliberately NOT
    /// building"). There is no cache layer yet for `Cached`/`Unavailable`
    /// to describe -- every observation returned here was *just* read live
    /// off the wire.
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
        let (reply, received_at) = self
            .mux
            .call(op.to_owned(), params_value, self.default_deadline)
            .await?;
        match reply {
            Reply::Err { err, .. } => Err(HostError::Wire(err)),
            Reply::Ok { ok, .. } => {
                let parsed = serde_json::from_value(ok).map_err(|e| {
                    HostError::Wire(ErrorBody::new(
                        WireErrorCode::InvalidRequest,
                        format!("malformed {op} reply: {e}"),
                    ))
                })?;
                Ok((parsed, received_at))
            }
        }
    }
}

// ---------------------------------------------------------------------
// Host clock: RFC 3339 "now", stdlib-only.
// ---------------------------------------------------------------------

/// The host's own receipt-time stamp, formatted to second precision as
/// `YYYY-MM-DDTHH:MM:SSZ` -- exactly the shape [`Rfc3339::new`] validates.
/// No date/time dependency: a calendar-correct proleptic Gregorian
/// conversion from a Unix timestamp is a well-known, self-contained
/// algorithm (Hinnant's `civil_from_days`), and nothing here needs time
/// zones, locales, or calendar arithmetic beyond that.
pub(crate) fn now_rfc3339() -> Rfc3339 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX);
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    let text = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z");
    Rfc3339::new(text).unwrap_or_else(|e| {
        // Unreachable except if `Rfc3339`'s own validation rules change
        // shape: `year`/`month`/`day`/`hour`/`minute`/`second` above are
        // all in-range by construction (the civil-calendar algorithm and
        // the `div_euclid`/`rem_euclid` splits guarantee it), formatted
        // into exactly the 20-byte shape the validator requires.
        unreachable!("now_rfc3339 built a timestamp its own crate rejects: {e}")
    })
}

/// Civil (proleptic Gregorian) date from a day count since the Unix epoch.
/// Howard Hinnant's `civil_from_days`
/// (<http://howardhinnant.github.io/date_algorithms.html>), valid for any
/// `days >= 0` (all we need: [`now_rfc3339`] never sees a pre-1970 value).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod lib_tests {
    use super::*;

    #[test]
    fn now_rfc3339_is_well_formed_and_recent() {
        let ts = now_rfc3339();
        assert!(
            ts.as_str().starts_with("20"),
            "expected a 21st-century date, got {ts:?}"
        );
        assert!(ts.as_str().ends_with('Z'));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 1970-01-01 is day 0 by definition.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01 is a well-known anchor for this algorithm.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        // 2026-09-06, comfortably inside this milestone's timeframe.
        assert_eq!(civil_from_days(20_702), (2026, 9, 6));
    }
}
