//! Drives one conformance case (a `conformance/cases/*.json` fixture)
//! against a black-box adapter and reports every failed assertion.
//!
//! Every case is driven through the real [`sumer_host::AdapterHandle`] --
//! the same supervisor a production host uses -- so id lifecycle, protocol
//! violation detection, and provenance stamping are exercised for real, not
//! reimplemented here -- including `unsupported_op`'s probe and every
//! [`wire_pass`], which reach the wire through `AdapterHandle::call_raw`.
//! There is exactly one client in this crate.
//!
//! A host is also a *remediator*, though: it omits every observation over
//! `MAX_OBSERVATION_BYTES` at decode and stamps fields onto what it keeps.
//! Two assertions judge the adapter behaviour that hides behind that, so
//! they read raw reply envelopes on their own fresh processes -- see
//! [`wire_pass`].
//!
//! Resumption ([`sumer_host::paging::ResumeState`]) and revision assignment
//! / the live-set fold ([`sumer_host::fold::Fold`]) are never reimplemented
//! here -- the frozen contract requires calling into `sumer-host` for both,
//! so a real host and this suite's expectations are checked against one
//! implementation, not two that could silently diverge.
//!
//! **Nothing in this file may be specific to the reference Python adapter.**
//! A read starts with an *absent* `page` (spec/observation.md §5 and Ruling
//! A8: absent means "from the start of available history"), and every
//! subsequent page request is a cursor the adapter itself returned. An
//! adapter that only honours its own opaque cursors -- which is exactly what
//! the contract permits -- must survive this crawl unchanged.

use crate::assert::{assert_status_coverage, json_subset_diff, Failure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;
use sumer_host::fold::{Fold, RevisionedObservation};
use sumer_host::paging::ResumeState;
use sumer_host::{AdapterHandle, HostError, DEFAULT_DEADLINE};
use sumer_wire::{
    BalancesReadParams, CursorResumable, HistoryReadParams, PageRequest, ProtocolViolationKind,
    Reply, ResourceQuery, MAX_OBSERVATION_BYTES, OP_BALANCES_READ, OP_HISTORY_READ,
};

/// Every fixture with more than this many pages for one resource is
/// treated as hung rather than followed forever -- a defensive cap, not
/// part of the wire contract.
const MAX_PAGES: u32 = 500;

/// The result of running one case: which case, and every assertion that
/// failed (empty means the case passed).
pub struct CaseOutcome {
    pub case: String,
    pub failures: Vec<Failure>,
}

/// Loads `path`, dispatches to the case-specific driver named by its
/// `"case"` field, and returns every failure collected.
pub async fn run_case(argv: &[String], path: &Path) -> CaseOutcome {
    let mut failures = Vec::new();
    let fixture: Value = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                failures.push(Failure::new(
                    "setup",
                    format!("{}: invalid JSON: {e}", path.display()),
                ));
                return CaseOutcome {
                    case: path.display().to_string(),
                    failures,
                };
            }
        },
        Err(e) => {
            failures.push(Failure::new(
                "setup",
                format!("could not read {}: {e}", path.display()),
            ));
            return CaseOutcome {
                case: path.display().to_string(),
                failures,
            };
        }
    };
    let case = fixture
        .get("case")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>")
        .to_owned();

    match case.as_str() {
        "large_amounts"
        | "null_category"
        | "pending_to_posted"
        | "reorg_vanish"
        | "duplicate_events"
        | "oversized_observation"
        | "provider_json_number"
        | "fdx_lossless" => generic_crawl(argv, path, &fixture, &mut failures).await,
        "stale_balance" => case_stale_balance(argv, path, &fixture, &mut failures).await,
        "unsupported_op" => case_unsupported_op(argv, path, &fixture, &mut failures).await,
        "interrupted_pagination" => {
            case_interrupted_pagination(argv, path, &fixture, &mut failures).await;
        }
        "protocol_violations" => {
            case_protocol_violations(argv, path, &fixture, &mut failures).await
        }
        other => failures.push(Failure::new(
            "setup",
            format!("no runner logic registered for case {other:?}"),
        )),
    }

    CaseOutcome { case, failures }
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

fn env_for(path: &Path, run: u64) -> Vec<(String, String)> {
    vec![
        (
            "SUMER_FIXTURE".to_owned(),
            path.to_string_lossy().into_owned(),
        ),
        ("SUMER_FIXTURE_RUN".to_owned(), run.to_string()),
    ]
}

async fn spawn_run(
    argv: &[String],
    path: &Path,
    run: u64,
    deadline: Duration,
) -> Result<AdapterHandle, HostError> {
    AdapterHandle::spawn_with_deadline(argv.to_vec(), env_for(path, run), deadline).await
}

/// A fixture that names an `expect` key in the wrong shape must fail loudly.
/// Quietly returning instead is how an assertion stops being able to fail --
/// four of this suite's assertions had already got there by other routes.
fn expect_object<'a>(
    value: &'a Value,
    what: &str,
    failures: &mut Vec<Failure>,
) -> Option<&'a serde_json::Map<String, Value>> {
    value.as_object().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!("{what} must be an object, got {}", brief(value)),
        ));
        None
    })
}

fn expect_array<'a>(
    value: &'a Value,
    what: &str,
    failures: &mut Vec<Failure>,
) -> Option<&'a Vec<Value>> {
    value.as_array().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!("{what} must be an array, got {}", brief(value)),
        ));
        None
    })
}

fn expect_str<'a>(value: &'a Value, what: &str, failures: &mut Vec<Failure>) -> Option<&'a str> {
    value.as_str().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!("{what} must be a string, got {}", brief(value)),
        ));
        None
    })
}

fn expect_u64(value: &Value, what: &str, failures: &mut Vec<Failure>) -> Option<u64> {
    value.as_u64().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!(
                "{what} must be a non-negative integer, got {}",
                brief(value)
            ),
        ));
        None
    })
}

/// Caps a JSON value's rendering so a failure message stays readable when
/// the offending value is a leaked 70 KB blob.
fn brief(value: &Value) -> String {
    let rendered = value.to_string();
    if rendered.len() <= 240 {
        return rendered;
    }
    let head: String = rendered.chars().take(240).collect();
    format!("{head}... [{} bytes total]", rendered.len())
}

/// Serializes an [`sumer_wire::Observation`] for comparison against
/// `expect`.
fn observation_json(observation: &sumer_wire::Observation) -> Value {
    serde_json::to_value(observation).unwrap_or(Value::Null)
}

/// The same observation with the two fields the *host* stamps per receipt
/// removed: `received_at` (a fresh clock reading every run) and the
/// `staleness` computed from it. What remains is exactly what the adapter
/// put on the wire, which is the only thing two runs can be held to.
fn adapter_view(observation: &sumer_wire::Observation) -> Value {
    let mut json = observation_json(observation);
    if let Some(prov) = json
        .pointer_mut("/provenance")
        .and_then(Value::as_object_mut)
    {
        prov.remove("received_at");
        prov.remove("staleness");
    }
    json
}

/// The live set as *financial content*, keyed by `local_id`: amount, state,
/// posting, surface, provider_extra -- everything that constitutes the
/// picture, not just the identities.
///
/// Comparing key sets alone is what A5 and A9 used to do, and it is
/// vacuous: an adapter can return the expected `local_id`s carrying
/// entirely different money and pass. This is the comparison both
/// assertions actually need.
fn live_content(fold: &Fold) -> BTreeMap<String, Value> {
    // Keyed by bare `local_id`: `Fold` keys `(adapter_id, local_id)` because
    // a real host runs several adapters at once, but this suite drives
    // exactly one per case, and the fixtures name bare local_ids. The
    // adapter_id is still inside each value (`provenance.adapter_id`), so a
    // run that changed it would show up as a content difference.
    fold.live_set()
        .into_iter()
        .map(|((_, local_id), r)| (local_id.to_owned(), adapter_view(&r.observation)))
        .collect()
}

/// The chain for a bare `local_id`, under whichever adapter emitted it.
fn chain_for<'a>(fold: &'a Fold, local_id: &str) -> Vec<&'a RevisionedObservation> {
    let adapters: Vec<String> = fold
        .keys()
        .filter(|(_, l)| *l == local_id)
        .map(|(a, _)| a.to_owned())
        .collect();
    adapters
        .iter()
        .flat_map(|a| fold.chain(a, local_id))
        .collect()
}

/// How two values for one `local_id` disagree. An observation *chain* is an
/// array, and rendering two whole chains side by side just truncates into
/// noise -- so a length mismatch says so, and a content mismatch names the
/// first index that differs.
fn disagreement(actual: &Value, expected: &Value) -> String {
    if let (Value::Array(a), Value::Array(e)) = (actual, expected) {
        if a.len() != e.len() {
            return format!(
                "{} observation(s) in this run, {} in the other",
                a.len(),
                e.len()
            );
        }
        if let Some((i, (a_i, e_i))) = a.iter().zip(e).enumerate().find(|(_, (x, y))| x != y) {
            return format!("observation {i} differs: {} != {}", brief(a_i), brief(e_i));
        }
    }
    format!("{} != {}", brief(actual), brief(expected))
}

/// Every way two live sets can disagree, rendered one line per `local_id`.
fn content_diff(
    actual: &BTreeMap<String, Value>,
    expected: &BTreeMap<String, Value>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (local_id, expected_obs) in expected {
        match actual.get(local_id) {
            None => out.push(format!("{local_id:?}: missing")),
            Some(actual_obs) if actual_obs != expected_obs => {
                out.push(format!(
                    "{local_id:?}: {}",
                    disagreement(actual_obs, expected_obs)
                ));
            }
            Some(_) => {}
        }
    }
    for local_id in actual.keys().filter(|k| !expected.contains_key(*k)) {
        out.push(format!("{local_id:?}: unexpected, not in the other run"));
    }
    out
}

/// A10: an observation **the adapter put on the wire** that exceeds
/// `MAX_OBSERVATION_BYTES` means the two-step degrade of
/// spec/observation.md §6 was not performed.
///
/// `raw` must be an observation exactly as it arrived in the reply frame --
/// see [`wire_pass`]. Measuring anything else cannot work, in either
/// direction:
///
/// * The host **omits** every over-cap observation at decode
///   (spec/observation.md §6, "the host enforces the cap too"). By the time
///   a decoded observation is visible to this suite it is under the cap by
///   construction, so a check there can never see the violation A10 exists
///   to catch -- an adapter that skips the degrade entirely is silently
///   remediated into looking conformant.
/// * The host's decoded form is not the wire form. `Observation`
///   serializes its absent optionals as explicit nulls that
///   `ObservationWire` omits, so re-serializing it charges an adapter for
///   bytes it never sent -- enough to reject a legitimate record sitting
///   exactly at the cap.
///
/// The count is `serde_json`'s encoding of the same JSON value the adapter
/// sent, which differs from the adapter's own bytes only in whitespace and
/// key order -- the difference spec/observation.md §6 explicitly permits
/// when it says the same thing about the host's measurement.
fn assert_wire_size(raw: &Value, label: &str, failures: &mut Vec<Failure>) {
    let Ok(bytes) = serde_json::to_vec(raw).map(|v| v.len()) else {
        failures.push(Failure::new(
            "A10",
            format!("{label}: could not re-serialize the observation to measure it"),
        ));
        return;
    };
    if bytes > MAX_OBSERVATION_BYTES {
        failures.push(Failure::new(
            "A10",
            format!(
                "{label}: the adapter put a {bytes}-byte observation on the wire, over \
                 MAX_OBSERVATION_BYTES ({MAX_OBSERVATION_BYTES}) -- spec/observation.md §6's \
                 two-step degrade was not applied before it went out. The host omits it \
                 afterwards; that is remediation, not conformance"
            ),
        ));
    }
}

/// Compares `expected` against `actual` as a subset -- with one exception:
/// `provider_extra` must match **exactly**.
///
/// A subset comparison cannot express "and nothing else", and
/// `provider_extra` is the one field where that is the whole assertion: A10
/// step 1 says the field is *replaced* by `{"_truncated": true,
/// "_original_bytes": N}`, and `fdx_lossless` says every FDX field lands
/// there verbatim and nothing else does. An adversarial review proved the
/// subset form accepts 70 KB of leaked payload sitting beside the
/// truncation marker.
fn diff_observation(actual: &Value, expected: &Value, path: &str, diffs: &mut Vec<String>) {
    let mut expected = expected.clone();
    if let Value::Object(map) = &mut expected {
        if let Some(expected_extra) = map.remove("provider_extra") {
            let actual_extra = actual.get("provider_extra").cloned().unwrap_or(Value::Null);
            if actual_extra != expected_extra {
                diffs.push(format!(
                    "{path}.provider_extra must be EXACTLY {}, got {}",
                    brief(&expected_extra),
                    brief(&actual_extra)
                ));
            }
        }
    }
    json_subset_diff(actual, &expected, path, diffs);
}

/// The one history-pagination loop in this crate. Every caller reads a
/// different part of the outcome ([`Drain`]), which is why there used to be
/// six near-identical copies of it.
struct Drain {
    /// Page replies received.
    frames: u64,
    /// Every `local_id` emitted, in arrival order, duplicates kept.
    local_ids: Vec<String>,
    /// Every `statuses` entry, flattened across pages.
    statuses: Vec<Value>,
    /// The adapter died mid-drain (the interruption `interrupted_pagination`
    /// deliberately provokes).
    crashed: bool,
}

/// What a drain is for: which assertion owns its failures, and whether an
/// adapter crash mid-read is the point of the exercise or a fault. Exactly
/// one leg of `interrupted_pagination` expects a crash; everywhere else one
/// is a failure, and used to be swallowed in silence.
#[derive(Clone, Copy)]
enum DrainFor {
    Assertion(&'static str),
    ExpectedCrash(&'static str),
}

impl DrainFor {
    fn assertion(self) -> &'static str {
        match self {
            DrainFor::Assertion(a) | DrainFor::ExpectedCrash(a) => a,
        }
    }

    fn crash_is_a_failure(self) -> bool {
        matches!(self, DrainFor::Assertion(_))
    }
}

/// Drains one resource's history to exhaustion, folding as it goes.
///
/// `start` is what the first request carries in `page`: `None` means an
/// absent `page` field, i.e. "from the start of available history"
/// (spec/observation.md §5, Ruling A8). Every page after the first uses the
/// cursor the adapter itself returned -- this crawl never invents one.
async fn drain_history(
    handle: &AdapterHandle,
    resource_id: &str,
    start: Option<PageRequest>,
    fold: &mut Fold,
    mut resume: Option<&mut ResumeState>,
    drain_for: DrainFor,
    failures: &mut Vec<Failure>,
) -> Drain {
    let mut drain = Drain {
        frames: 0,
        local_ids: Vec::new(),
        statuses: Vec::new(),
        crashed: false,
    };
    let requested = [resource_id.to_owned()];
    let mut page = start;
    let mut pages = 0_u32;
    loop {
        pages += 1;
        if pages > MAX_PAGES {
            failures.push(Failure::new(
                "setup",
                format!(
                    "history.read({resource_id}) exceeded {MAX_PAGES} pages -- treating as hung"
                ),
            ));
            return drain;
        }
        let reply = match handle
            .history_read(vec![ResourceQuery {
                resource_id: resource_id.to_owned(),
                page: page.clone(),
            }])
            .await
        {
            Ok(r) => r,
            Err(HostError::AdapterCrashed { status }) => {
                drain.crashed = true;
                if drain_for.crash_is_a_failure() {
                    failures.push(Failure::new(
                        drain_for.assertion(),
                        format!(
                            "history.read({resource_id}): the adapter died (exit {status:?}) \
                             part-way through the read"
                        ),
                    ));
                }
                return drain;
            }
            Err(e) => {
                failures.push(Failure::new(
                    drain_for.assertion(),
                    format!("history.read({resource_id}) failed: {e}"),
                ));
                return drain;
            }
        };
        drain.frames += 1;

        let statuses_json: Vec<Value> = reply
            .statuses
            .iter()
            .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
            .collect();
        assert_status_coverage(&requested, &statuses_json, failures);
        drain.statuses.extend(statuses_json);

        // No size check here, deliberately: the host omits every over-cap
        // observation at decode, so nothing that reaches this loop can be
        // one. A10 is measured where the violation is still visible -- at
        // the wire, in [`wire_pass`].
        for obs in reply.observations {
            drain.local_ids.push(obs.local_id.clone());
            fold.ingest(obs);
        }

        let (resumable, next) = reply
            .statuses
            .into_iter()
            .next()
            .and_then(|s| s.page)
            .map_or((CursorResumable::None, None), |p| {
                (p.cursor_resumable, p.next)
            });
        if let Some(state) = resume.as_deref_mut() {
            state.record(resumable, next.clone());
        }
        match next {
            Some(n) => page = Some(n),
            None => return drain,
        }
    }
}

// ---------------------------------------------------------------------
// The wire pass: the same reads, on a fresh process, read as raw frames.
// ---------------------------------------------------------------------

/// One adapter lifetime's reads, observed **at the wire**: the raw JSON the
/// adapter emitted, before the host decoded it, dropped anything over the
/// cap, or stamped a single field onto it.
///
/// This is not a second client -- it is the same [`AdapterHandle`], the same
/// spawn, the same frame decoder and the same id lifecycle as every other
/// call in this suite. The only difference is
/// [`AdapterHandle::call_raw`], which hands back the envelope verbatim
/// instead of the host's decoded, remediated view of it. Two assertions
/// need that and cannot be written without it:
///
/// * **A10** (see [`assert_wire_size`]): the host omits an over-cap
///   observation before any decoded reply exists, so the only place the
///   violation is still visible is the frame it arrived in.
/// * **A9**: `local_id` purity is a claim about what the *adapter* derives.
///   Comparing the host's decoded observations compares the host's
///   serialization of them; comparing raw frames compares the adapter's own
///   bytes, with nothing host-added in the way.
///
/// Requests are built from the same `sumer_wire` param types the typed
/// reads use, so the bytes on the adapter's stdin are identical to a normal
/// read's -- a fixture cannot tell a wire pass apart from the crawl it
/// mirrors.
///
/// Returns every history observation seen, keyed by `local_id`, as a JSON
/// array in arrival order: the `local_id` -> provider-record association,
/// including records later superseded or tombstoned.
async fn wire_pass(
    argv: &[String],
    path: &Path,
    run: u64,
    resource_ids: &[String],
    label: &str,
    failures: &mut Vec<Failure>,
) -> BTreeMap<String, Value> {
    let mut by_local_id: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let handle = match spawn_run(argv, path, run, DEFAULT_DEADLINE).await {
        Ok(h) => h,
        Err(e) => {
            failures.push(Failure::new(
                "A10",
                format!("{label}: failed to spawn: {e}"),
            ));
            return BTreeMap::new();
        }
    };

    // balances.read: measured, not collected -- a balance line has no
    // `local_id` to associate anything with, so it is A10 evidence only.
    let params = BalancesReadParams {
        resource_ids: resource_ids.to_vec(),
    };
    match raw_call(&handle, OP_BALANCES_READ, &params, label, failures).await {
        Some(ok) => {
            for (i, raw) in raw_observations(&ok, label, failures).iter().enumerate() {
                assert_wire_size(
                    raw,
                    &format!("{label}: balances.read observation {i}"),
                    failures,
                );
            }
        }
        None => return by_local_id.into_iter().map(array_value).collect(),
    }

    for resource_id in resource_ids {
        let mut page: Option<PageRequest> = None;
        let mut pages = 0_u32;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                failures.push(Failure::new(
                    "setup",
                    format!("{label}: history.read({resource_id}) exceeded {MAX_PAGES} pages"),
                ));
                break;
            }
            let params = HistoryReadParams {
                resources: vec![ResourceQuery {
                    resource_id: resource_id.clone(),
                    page: page.clone(),
                }],
            };
            let Some(ok) = raw_call(&handle, OP_HISTORY_READ, &params, label, failures).await
            else {
                break;
            };
            for raw in raw_observations(&ok, label, failures) {
                let Some(local_id) = raw.get("local_id").and_then(Value::as_str) else {
                    failures.push(Failure::new(
                        "A2",
                        format!(
                            "{label}: an observation arrived with no local_id: {}",
                            brief(raw)
                        ),
                    ));
                    continue;
                };
                assert_wire_size(
                    raw,
                    &format!("{label}: history.read({resource_id}) observation {local_id:?}"),
                    failures,
                );
                by_local_id
                    .entry(local_id.to_owned())
                    .or_default()
                    .push(raw.clone());
            }
            match next_page(&ok, label, failures) {
                Some(next) => page = Some(next),
                None => break,
            }
        }
    }
    by_local_id.into_iter().map(array_value).collect()
}

fn array_value((local_id, observations): (String, Vec<Value>)) -> (String, Value) {
    (local_id, Value::Array(observations))
}

/// One raw call, reporting anything that is not a success envelope. A wire
/// pass mirrors reads the host crawl already made successfully, so an `err`
/// here is a real divergence, not a scripted outcome to tolerate.
async fn raw_call<P: serde::Serialize>(
    handle: &AdapterHandle,
    op: &str,
    params: &P,
    label: &str,
    failures: &mut Vec<Failure>,
) -> Option<Value> {
    let params = match serde_json::to_value(params) {
        Ok(v) => v,
        Err(e) => {
            failures.push(Failure::new(
                "setup",
                format!("{label}: could not serialize {op} params: {e}"),
            ));
            return None;
        }
    };
    match handle.call_raw(op, params).await {
        Ok(Reply::Ok { ok, .. }) => Some(ok),
        other => {
            failures.push(Failure::new(
                "A10",
                format!("{label}: {op} did not return a success envelope: {other:?}"),
            ));
            None
        }
    }
}

/// A reply body's `observations`, verbatim. Absent or non-array is a
/// malformed reply -- the typed path rejects it too, but this pass has to
/// say so itself rather than measure nothing in silence.
fn raw_observations<'a>(ok: &'a Value, label: &str, failures: &mut Vec<Failure>) -> &'a [Value] {
    match ok.get("observations") {
        Some(Value::Array(items)) => items,
        other => {
            failures.push(Failure::new(
                "A2",
                format!(
                    "{label}: reply body has no `observations` array (got {})",
                    other.map_or("nothing".to_owned(), brief)
                ),
            ));
            &[]
        }
    }
}

/// The next page request out of a raw reply body, read from the first
/// status's `page.next` exactly as [`drain_history`] reads it off the
/// decoded one.
///
/// This deserializes from a [`Value`], which -- unlike the host's typed
/// path, which now runs against the frame's original text -- cannot see a
/// duplicate JSON key, because `Value` collapsed it during parsing. That is
/// not a gap in coverage: `call_raw` is the only thing here that yields a
/// `Value` at all, and every case running a wire pass also drives the same
/// frames through the typed crawl, where a duplicate key is rejected.
fn next_page(ok: &Value, label: &str, failures: &mut Vec<Failure>) -> Option<PageRequest> {
    let next = ok.pointer("/statuses/0/page/next")?;
    if next.is_null() {
        return None;
    }
    match serde_json::from_value::<PageRequest>(next.clone()) {
        Ok(page) => Some(page),
        Err(e) => {
            failures.push(Failure::new(
                "A5",
                format!(
                    "{label}: `page.next` is not a valid PageRequest: {e} ({})",
                    brief(next)
                ),
            ));
            None
        }
    }
}

// ---------------------------------------------------------------------
// The generic crawl: resources.list -> balances.read -> history.read
// (paginated) -> status.read, checked against whatever `expect` names.
// Covers every case whose fixture asks for exactly this shape.
// ---------------------------------------------------------------------

async fn generic_crawl(argv: &[String], path: &Path, fixture: &Value, failures: &mut Vec<Failure>) {
    let expect = fixture.get("expect").cloned().unwrap_or(Value::Null);
    let handle = match spawn_run(argv, path, 0, DEFAULT_DEADLINE).await {
        Ok(h) => h,
        Err(e) => {
            failures.push(Failure::new("setup", format!("spawn failed: {e}")));
            return;
        }
    };

    // 1. resources.list
    let resources_reply = match handle.resources_list().await {
        Ok(r) => r,
        Err(e) => {
            failures.push(Failure::new("setup", format!("resources.list failed: {e}")));
            return;
        }
    };
    let actual_resources_json: Vec<Value> = resources_reply
        .resources
        .iter()
        .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
        .collect();
    if let Some(expected_list) = expect.get("resources").and_then(Value::as_array) {
        assert_resources(&actual_resources_json, expected_list, failures);
    }
    let resource_ids: Vec<String> = resources_reply
        .resources
        .iter()
        .map(|d| d.resource_id.clone())
        .collect();
    if resource_ids.is_empty() {
        failures.push(Failure::new(
            "setup",
            "resources.list returned no resources".to_owned(),
        ));
        return;
    }
    // Every `provider_extra` key the adapter emitted anywhere -- one half of
    // `expect.fdx_field_map`'s "nothing silently dropped, nothing
    // undocumented" claim.
    let mut emitted_provider_extra: BTreeSet<String> = actual_resources_json
        .iter()
        .filter_map(|r| r.get("provider_extra"))
        .filter_map(Value::as_object)
        .flat_map(|m| m.keys().cloned())
        .collect();

    let mut all_statuses_by_resource: HashMap<String, Vec<Value>> = HashMap::new();

    // 2. balances.read: every resource, one batched call.
    match handle.balances_read(resource_ids.clone()).await {
        Ok(reply) => {
            let statuses_json: Vec<Value> = reply
                .statuses
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                .collect();
            assert_status_coverage(&resource_ids, &statuses_json, failures);
            for s in &statuses_json {
                if let Some(rid) = s.get("resource_id").and_then(Value::as_str) {
                    all_statuses_by_resource
                        .entry(rid.to_owned())
                        .or_default()
                        .push(s.clone());
                }
            }
            if let Some(expected_balances) = expect.get("balances") {
                assert_balances(&reply.observations, expected_balances, failures);
            }
            if let Some(expected_grammar) = expect.get("grammar_check") {
                assert_grammar_check(&reply.observations, expected_grammar, failures);
            }
            if let Some(expected_prov) = expect.get("provenance") {
                assert_provenance(&reply.observations, expected_prov, failures);
            }
        }
        Err(e) => failures.push(Failure::new("A2", format!("balances.read failed: {e}"))),
    }

    // 3. history.read: one resource at a time, paginated to completion,
    // folded through sumer_host::fold::Fold (never reimplemented here).
    let mut fold = Fold::new();
    for rid in &resource_ids {
        let drain = drain_history(
            &handle,
            rid,
            None,
            &mut fold,
            None,
            DrainFor::Assertion("A2"),
            failures,
        )
        .await;
        all_statuses_by_resource
            .entry(rid.clone())
            .or_default()
            .extend(drain.statuses);
    }
    // Every observation ever ingested, not just the live ones: a
    // provider_extra key that arrived on a record later superseded or
    // tombstoned is still a key that arrived undocumented.
    let all_observed: Vec<(String, String)> = fold
        .keys()
        .map(|(a, l)| (a.to_owned(), l.to_owned()))
        .collect();
    for obs in all_observed
        .iter()
        .flat_map(|(a, l)| fold.chain(a, l).into_iter())
    {
        if let Some(extra) = observation_json(&obs.observation)
            .get("provider_extra")
            .and_then(Value::as_object)
        {
            emitted_provider_extra.extend(extra.keys().cloned());
        }
    }

    if let Some(expected_live) = expect.get("history_live_set") {
        assert_history_live_set(&fold, expected_live, failures);
    }
    if let Some(expected_sizes) = expect.get("live_set_size") {
        assert_live_set_size(&fold, expected_sizes, failures);
    }
    if let Some(expected_chains) = expect.get("chains") {
        assert_chains(&fold, expected_chains, failures);
    }
    if let Some(expected_omitted) = expect.get("omitted_observations") {
        assert_omitted(&fold, expected_omitted, failures);
    }
    if let Some(expected_statuses) = expect.get("statuses") {
        assert_expected_statuses(
            &all_statuses_by_resource,
            expected_statuses,
            "expect.statuses",
            failures,
        );
    }
    if let Some(field_map) = expect.get("fdx_field_map") {
        assert_fdx_field_map(field_map, &emitted_provider_extra, failures);
    }

    // 4. status.read: every resource, one batched call. Its reply is the
    // only one that populates `credential_expires_at` /
    // `strong_auth_expires_at` / `history_start` (spec/observation.md §7),
    // so `expect.status_read` is checked against these entries alone --
    // folding them in with the balances/history statuses would let a
    // fixture's claim about the clocks be satisfied by a reply that never
    // carried them.
    match handle.status_read(resource_ids.clone()).await {
        Ok(reply) => {
            let statuses_json: Vec<Value> = reply
                .statuses
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                .collect();
            assert_status_coverage(&resource_ids, &statuses_json, failures);
            assert_status_read(&statuses_json, &expect, failures);
        }
        Err(e) => failures.push(Failure::new("A7", format!("status.read failed: {e}"))),
    }

    // 5. Any envelope-level rejection the fixture demands, on this same
    // connection -- and the connection has to survive it.
    if let Some(expected_errors) = expect.get("envelope_errors") {
        assert_envelope_errors(&handle, expected_errors, failures).await;
    }

    // A9 + A10, both measured at the wire, on two further independent
    // process invocations of the same fixture.
    drop(handle);
    let first = wire_pass(argv, path, 0, &resource_ids, "wire pass 1", failures).await;
    let second = wire_pass(argv, path, 0, &resource_ids, "wire pass 2", failures).await;
    assert_local_id_purity(&first, &second, failures);
}

/// A9: `local_id` is a pure function of provider data, checked across two
/// independent process launches -- as the **whole historical association**,
/// not a set of ids and not the final live set.
///
/// Two weaker comparisons this deliberately is not:
///
/// * The set of ids alone is satisfied by an adapter that hands out the
///   same ids on the second launch attached to *different records*: swap
///   two observations' ids and the set is identical while every id now
///   names the wrong thing.
/// * The final live set alone ignores every record that was later
///   superseded or tombstoned. An adapter can mis-derive the `local_id` of
///   an intermediate observation -- the tombstone in a reorg chain, the
///   pending row a posted one supersedes -- and still land on an identical
///   live set. Purity is a claim about *every* record the derivation
///   touches, so the comparison is per `local_id` over the full ordered
///   list of observations carrying it.
///
/// Both sides are raw wire frames ([`wire_pass`]), so what is compared is
/// the adapter's own bytes with nothing host-stamped in the way.
fn assert_local_id_purity(
    first: &BTreeMap<String, Value>,
    second: &BTreeMap<String, Value>,
    failures: &mut Vec<Failure>,
) {
    for line in content_diff(second, first) {
        failures.push(Failure::new(
            "A9",
            format!(
                "the local_id -> provider-record association differs between two independent \
                 process invocations of the same fixture, across the full observation history \
                 (not just the final live set): {line}"
            ),
        ));
    }
}

/// `expect.envelope_errors`, keyed `"<resource_id>_<op>"` (`balances_read`
/// or `history_read`): that call must be rejected at the envelope level with
/// the named `err.code`, and -- since the whole point is that an envelope
/// error is not a connection error -- the connection must still be usable
/// afterwards.
async fn assert_envelope_errors(
    handle: &AdapterHandle,
    expected: &Value,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(expected, "expect.envelope_errors", failures) else {
        return;
    };
    for (key, expected_body) in map {
        let (resource_id, result) = if let Some(rid) = key.strip_suffix("_balances_read") {
            (rid, handle.balances_read(vec![rid.to_owned()]).await.err())
        } else if let Some(rid) = key.strip_suffix("_history_read") {
            (
                rid,
                handle
                    .history_read(vec![ResourceQuery {
                        resource_id: rid.to_owned(),
                        page: None,
                    }])
                    .await
                    .err(),
            )
        } else {
            failures.push(Failure::new(
                "setup",
                format!(
                    "envelope_errors key {key:?} names no op -- expected \
                     \"<resource_id>_balances_read\" or \"<resource_id>_history_read\""
                ),
            ));
            continue;
        };
        let expected_code = expected_body.get("code").and_then(Value::as_str);
        match result {
            Some(HostError::Wire(err)) => {
                let actual_code = serde_json::to_value(err.code).unwrap_or(Value::Null);
                if actual_code.as_str() != expected_code {
                    failures.push(Failure::new(
                        "A4",
                        format!(
                            "{resource_id}: expected envelope err.code={expected_code:?}, got \
                             {actual_code} ({})",
                            err.message
                        ),
                    ));
                }
            }
            other => failures.push(Failure::new(
                "A4",
                format!(
                    "{key}: expected an envelope err with code {expected_code:?}, got {other:?}"
                ),
            )),
        }
    }
    // A4's pairing half: an envelope error is scoped to the one request it
    // answers and must never take the connection with it.
    if let Err(e) = handle.resources_list().await {
        failures.push(Failure::new(
            "A4",
            format!("the connection did not survive the envelope error: {e}"),
        ));
    }
}

// ---------------------------------------------------------------------
// Assertion composition (wire-type-aware; the generic JSON diffing lives
// in `assert.rs`).
// ---------------------------------------------------------------------

/// `expect.resources` names every resource the adapter may list, and only
/// those. The count matters for the same reason it does on balances: an
/// invented resource contradicts nothing on its own, so a
/// present-and-correct check alone certifies an adapter that reports an
/// account the provider never had.
fn assert_resources(actual_json: &[Value], expected_list: &[Value], failures: &mut Vec<Failure>) {
    if actual_json.len() != expected_list.len() {
        failures.push(Failure::new(
            "setup",
            format!(
                "resources.list returned {} resource(s), expected {} -- actual ids: {:?}",
                actual_json.len(),
                expected_list.len(),
                actual_json
                    .iter()
                    .map(|r| r.get("resource_id").cloned().unwrap_or(Value::Null))
                    .collect::<Vec<_>>()
            ),
        ));
    }
    for expected in expected_list {
        let Some(resource_id) = expected
            .get("resource_id")
            .and_then(|v| expect_str(v, "expect.resources[].resource_id", failures))
        else {
            continue;
        };
        match actual_json
            .iter()
            .find(|a| a.get("resource_id").and_then(Value::as_str) == Some(resource_id))
        {
            None => failures.push(Failure::new(
                "setup",
                format!("resources.list did not return resource_id {resource_id:?}"),
            )),
            Some(actual) => {
                let mut diffs = Vec::new();
                diff_observation(
                    actual,
                    expected,
                    &format!("resources[{resource_id}]"),
                    &mut diffs,
                );
                for d in diffs {
                    failures.push(Failure::new("setup", d));
                }
            }
        }
    }
}

/// A1/A3: every balance line a fixture names arrived with the value it
/// names -- **and the resource reported no line the fixture did not name**.
///
/// The count is half the assertion, not bookkeeping. A balances reply is a
/// list of provider-named categories (spec/observation.md §2), and nothing
/// downstream can tell a category the provider actually reported from one
/// an adapter invented: there is no schema to violate, no other field to
/// contradict. Checking only that the expected categories are present lets
/// an adapter append a fabricated line -- any name, any amount -- and pass.
fn assert_balances(actual: &[sumer_wire::Balance], expected: &Value, failures: &mut Vec<Failure>) {
    let Some(map) = expect_object(expected, "expect.balances", failures) else {
        return;
    };
    for (resource_id, entries) in map {
        let Some(entries) =
            expect_array(entries, &format!("expect.balances.{resource_id}"), failures)
        else {
            continue;
        };
        let actual_for_resource: Vec<&sumer_wire::Balance> = actual
            .iter()
            .filter(|b| &b.resource_id == resource_id)
            .collect();
        if actual_for_resource.len() != entries.len() {
            failures.push(Failure::new(
                "A1",
                format!(
                    "resource {resource_id:?}: {} balance line(s), expected {} -- actual \
                     categories: {:?}",
                    actual_for_resource.len(),
                    entries.len(),
                    actual_for_resource
                        .iter()
                        .map(|b| &b.category)
                        .collect::<Vec<_>>()
                ),
            ));
        }
        for expected_entry in entries {
            let Some(category) = expected_entry
                .get("category")
                .and_then(|v| expect_str(v, "expect.balances[].category", failures))
            else {
                failures.push(Failure::new(
                    "setup",
                    format!("expect.balances.{resource_id} has an entry with no category"),
                ));
                continue;
            };
            match actual_for_resource.iter().find(|b| b.category == category) {
                None => failures.push(Failure::new(
                    "A1",
                    format!(
                        "resource {resource_id:?}: no balance observation with category \
                         {category:?}; actual categories: {:?}",
                        actual_for_resource
                            .iter()
                            .map(|b| &b.category)
                            .collect::<Vec<_>>()
                    ),
                )),
                Some(actual_entry) => {
                    let actual_json = serde_json::to_value(actual_entry).unwrap_or(Value::Null);
                    let mut diffs = Vec::new();
                    json_subset_diff(
                        &actual_json,
                        expected_entry,
                        &format!("balances.{resource_id}[{category}]"),
                        &mut diffs,
                    );
                    for d in diffs {
                        failures.push(Failure::new("A1", d));
                    }
                }
            }
        }
    }
}

/// `expect.grammar_check`: extra digit-count / scale evidence beyond
/// `cmp_same_asset`, keyed `"<category>_digit_count"` / `"<category>_scale"`
/// (only `large_amounts.json` uses this).
fn assert_grammar_check(
    actual: &[sumer_wire::Balance],
    grammar: &Value,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(grammar, "expect.grammar_check", failures) else {
        return;
    };
    for (key, expected_val) in map {
        // `note` is the one key that carries prose rather than an
        // expectation. Anything else that matches neither suffix is a typo
        // that would otherwise be skipped in silence -- a grammar_check key
        // no one reads is a grammar_check that cannot fail.
        if key == "note" {
            continue;
        }
        let (category, is_scale) = if let Some(c) = key.strip_suffix("_digit_count") {
            (c, false)
        } else if let Some(c) = key.strip_suffix("_scale") {
            (c, true)
        } else {
            failures.push(Failure::new(
                "setup",
                format!(
                    "expect.grammar_check key {key:?} ends in neither \"_digit_count\" nor \
                     \"_scale\", so nothing reads it"
                ),
            ));
            continue;
        };
        let Some(balance) = actual.iter().find(|b| b.category == category) else {
            failures.push(Failure::new(
                "A1",
                format!("grammar_check names category {category:?}, which no balance reported"),
            ));
            continue;
        };
        let Some(amount) = &balance.amount else {
            failures.push(Failure::new(
                "A1",
                format!(
                    "grammar_check names category {category:?}, but its amount is null \
                     (unknown) -- there are no digits to count"
                ),
            ));
            continue;
        };
        if is_scale {
            {
                let Some(expected_scale) = expect_u64(expected_val, key, failures) else {
                    continue;
                };
                let actual_scale = u64::from(amount.scale());
                if actual_scale != expected_scale {
                    failures.push(Failure::new(
                        "A1",
                        format!(
                            "grammar_check: category {category:?} scale = {actual_scale}, \
                             expected {expected_scale}"
                        ),
                    ));
                }
            }
        } else {
            {
                let Some(expected_digits) = expect_u64(expected_val, key, failures) else {
                    continue;
                };
                let rendered = amount.to_string();
                let actual_digits =
                    u64::try_from(rendered.bytes().filter(u8::is_ascii_digit).count()).unwrap_or(0);
                if actual_digits != expected_digits {
                    failures.push(Failure::new(
                        "A1",
                        format!(
                            "grammar_check: category {category:?} has {actual_digits} \
                             significant digits (from {rendered:?}), expected {expected_digits}"
                        ),
                    ));
                }
            }
        }
    }
}

/// `expect.provenance`: checked field-by-field via the generic subset
/// comparator -- `staleness` included. It is host-computed and this
/// milestone's host has no cache layer, so `"live"` is the only conforming
/// value; a fixture that claims otherwise is asserting something the
/// contract forbids the adapter from influencing.
fn assert_provenance(
    actual: &[sumer_wire::Balance],
    expected: &Value,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(expected, "expect.provenance", failures) else {
        return;
    };
    for (resource_id, expected_prov) in map {
        let Some(balance) = actual.iter().find(|b| &b.resource_id == resource_id) else {
            failures.push(Failure::new(
                "A3",
                format!("expect.provenance names {resource_id:?}, which returned no balance"),
            ));
            continue;
        };
        let actual_json = serde_json::to_value(&balance.provenance).unwrap_or(Value::Null);
        let mut diffs = Vec::new();
        json_subset_diff(
            &actual_json,
            expected_prov,
            &format!("provenance.{resource_id}"),
            &mut diffs,
        );
        for d in diffs {
            failures.push(Failure::new("A3", d));
        }
    }
}

/// A2 (+ A6 floor): the folded live set matches `expect.history_live_set`
/// exactly (by `local_id`, per resource), and a resource with a non-empty
/// expected live set actually produced a non-empty one -- the floor that
/// kills a "report success with nothing in it" adapter.
fn assert_history_live_set(fold: &Fold, expected: &Value, failures: &mut Vec<Failure>) {
    let Some(map) = expect_object(expected, "expect.history_live_set", failures) else {
        return;
    };
    let live = fold.live_set();
    for (resource_id, expected_entries) in map {
        let Some(expected_entries) = expect_array(
            expected_entries,
            &format!("expect.history_live_set.{resource_id}"),
            failures,
        ) else {
            continue;
        };
        let actual_for_resource: Vec<&RevisionedObservation> = live
            .values()
            .filter(|r| &r.observation.resource_id == resource_id)
            .copied()
            .collect();
        if !expected_entries.is_empty() && actual_for_resource.is_empty() {
            failures.push(Failure::new(
                "A6",
                format!("resource {resource_id:?}: expected a non-empty live set, actual is empty"),
            ));
        }
        for expected_entry in expected_entries {
            let Some(local_id) = expected_entry
                .get("local_id")
                .and_then(|v| expect_str(v, "expect.history_live_set[].local_id", failures))
            else {
                failures.push(Failure::new(
                    "setup",
                    format!("expect.history_live_set.{resource_id} has an entry with no local_id"),
                ));
                continue;
            };
            match actual_for_resource
                .iter()
                .find(|r| r.observation.local_id == local_id)
            {
                None => failures.push(Failure::new(
                    "A2",
                    format!(
                        "resource {resource_id:?}: local_id {local_id:?} missing from the live \
                         set; actual live local_ids: {:?}",
                        actual_for_resource
                            .iter()
                            .map(|r| &r.observation.local_id)
                            .collect::<Vec<_>>()
                    ),
                )),
                Some(actual_entry) => {
                    let actual_json = observation_json(&actual_entry.observation);
                    let mut diffs = Vec::new();
                    diff_observation(
                        &actual_json,
                        expected_entry,
                        &format!("history_live_set.{resource_id}[{local_id}]"),
                        &mut diffs,
                    );
                    for d in diffs {
                        failures.push(Failure::new("A2", d));
                    }
                }
            }
        }
        if actual_for_resource.len() != expected_entries.len() {
            failures.push(Failure::new(
                "A2",
                format!(
                    "resource {resource_id:?}: live set has {} entries, expected {} -- actual \
                     local_ids: {:?}",
                    actual_for_resource.len(),
                    expected_entries.len(),
                    actual_for_resource
                        .iter()
                        .map(|r| &r.observation.local_id)
                        .collect::<Vec<_>>()
                ),
            ));
        }
    }
}

/// `expect.live_set_size`: the live set's cardinality per resource, stated
/// as a number rather than inferred from a list. `duplicate_events.json`'s
/// whole claim is a count -- a host that dedups by `provider_id` alone
/// collapses two unrelated events into one and lands on 1 instead of 2.
fn assert_live_set_size(fold: &Fold, expected: &Value, failures: &mut Vec<Failure>) {
    let Some(map) = expect_object(expected, "expect.live_set_size", failures) else {
        return;
    };
    let live = fold.live_set();
    for (resource_id, expected_size) in map {
        let Some(expected_size) = expect_u64(
            expected_size,
            &format!("expect.live_set_size.{resource_id}"),
            failures,
        ) else {
            continue;
        };
        let actual = live
            .values()
            .filter(|r| &r.observation.resource_id == resource_id)
            .count();
        if u64::try_from(actual).unwrap_or(u64::MAX) != expected_size {
            failures.push(Failure::new(
                "A2",
                format!(
                    "resource {resource_id:?}: live set size is {actual}, expected {expected_size}"
                ),
            ));
        }
    }
}

/// A8: the full ordered chain for each named `local_id` matches, entry by
/// entry, in fold total order -- kills an adapter (or host) that only
/// retains the final state.
fn assert_chains(fold: &Fold, expected: &Value, failures: &mut Vec<Failure>) {
    let Some(map) = expect_object(expected, "expect.chains", failures) else {
        return;
    };
    for (local_id, expected_chain) in map {
        let Some(expected_chain) = expect_array(
            expected_chain,
            &format!("expect.chains.{local_id}"),
            failures,
        ) else {
            continue;
        };
        let actual_chain = chain_for(fold, local_id);
        if actual_chain.len() != expected_chain.len() {
            failures.push(Failure::new(
                "A8",
                format!(
                    "chain {local_id:?}: expected {} entries, got {} -- actual states: {:?}",
                    expected_chain.len(),
                    actual_chain.len(),
                    actual_chain
                        .iter()
                        .map(|r| format!("{:?}/{:?}", r.observation.state, r.observation.posting))
                        .collect::<Vec<_>>()
                ),
            ));
            continue;
        }
        for (i, (actual_entry, expected_entry)) in
            actual_chain.iter().zip(expected_chain).enumerate()
        {
            let actual_json = observation_json(&actual_entry.observation);
            let mut diffs = Vec::new();
            diff_observation(
                &actual_json,
                expected_entry,
                &format!("chains.{local_id}[{i}]"),
                &mut diffs,
            );
            for d in diffs {
                failures.push(Failure::new("A8", d));
            }
        }
    }
}

/// A10 (the "never appears" half): every `local_id` named in
/// `expect.omitted_observations` must be absent from the fold entirely --
/// an adapter that emits it anyway (even truncated) fails.
fn assert_omitted(fold: &Fold, expected: &Value, failures: &mut Vec<Failure>) {
    let Some(map) = expect_object(expected, "expect.omitted_observations", failures) else {
        return;
    };
    for (resource_id, entries) in map {
        let Some(entries) = expect_array(
            entries,
            &format!("expect.omitted_observations.{resource_id}"),
            failures,
        ) else {
            continue;
        };
        for e in entries {
            if let Some(local_id) = e
                .get("local_id")
                .and_then(|v| expect_str(v, "expect.omitted_observations[].local_id", failures))
            {
                if !chain_for(fold, local_id).is_empty() {
                    failures.push(Failure::new(
                        "A10",
                        format!(
                            "local_id {local_id:?} was expected to be fully OMITTED (too large \
                             even after truncating provider_extra) but appeared in observations"
                        ),
                    ));
                }
            }
        }
    }
}

/// `expect.fdx_field_map`: the FDX 6.4 mapping table `fdx_lossless.json`
/// gates, checked in both directions.
///
/// Every `provider_extra.<key>` the map names must actually have arrived on
/// the wire ("nothing is silently dropped"), and every `provider_extra` key
/// that did arrive must be named by the map ("nothing arrives
/// undocumented"). The prose around each mapping is documentation and is not
/// parsed for meaning -- only the `provider_extra.<key>` targets in it are,
/// which is the part a machine can hold anyone to.
fn assert_fdx_field_map(
    field_map: &Value,
    emitted: &BTreeSet<String>,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(field_map, "expect.fdx_field_map", failures) else {
        return;
    };
    let mut named: BTreeSet<String> = BTreeSet::new();
    for value in map.values() {
        if let Some(text) = value.as_str() {
            named.extend(provider_extra_targets(text));
        }
    }
    if named.is_empty() {
        failures.push(Failure::new(
            "A1",
            "expect.fdx_field_map names no provider_extra target at all -- it asserts nothing"
                .to_owned(),
        ));
    }
    for key in named.difference(emitted) {
        failures.push(Failure::new(
            "A1",
            format!(
                "fdx_field_map says an FDX field lands in provider_extra.{key}, but no \
                 observation carried that key -- the mapping claims a field is preserved that \
                 was silently dropped"
            ),
        ));
    }
    for key in emitted.difference(&named) {
        failures.push(Failure::new(
            "A1",
            format!(
                "provider_extra.{key} arrived on the wire but fdx_field_map documents no FDX \
                 field landing there"
            ),
        ));
    }
}

/// Every `provider_extra.<identifier>` token in `text`.
fn provider_extra_targets(text: &str) -> Vec<String> {
    const NEEDLE: &str = "provider_extra.";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(NEEDLE) {
        let tail = &rest[i + NEEDLE.len()..];
        let end = tail
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(tail.len());
        if end > 0 {
            out.push(tail[..end].to_owned());
        }
        rest = &tail[end..];
    }
    out
}

/// Checks that, across every status observed for a resource (balances.read
/// and/or history.read alike -- `expect.statuses` does not say which), at
/// least one **entry** matches the expected shape as a subset. "At least
/// one of the calls that touched this resource produced this" is the right
/// granularity here: `expect.statuses` fixtures each exercise exactly one
/// of the two read paths, and this stays correct either way without the
/// runner having to guess which.
///
/// The match is against the whole status entry, not just its `outcome`,
/// because a status entry now states two independent facts: `outcome` (the
/// freshness one, which §1's staleness table reads) and `degraded` (a
/// record dropped for size). A fixture that could only name the outcome
/// could not assert the thing spec/observation.md §6 is emphatic about --
/// that a degrade leaves the outcome alone -- which is exactly the bug that
/// made a cached sibling observation look live.
fn assert_expected_statuses(
    all_statuses_by_resource: &HashMap<String, Vec<Value>>,
    expected: &Value,
    what: &str,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(expected, what, failures) else {
        return;
    };
    for (resource_id, expected_entry) in map {
        let candidates = all_statuses_by_resource
            .get(resource_id)
            .cloned()
            .unwrap_or_default();
        // The closest candidate's own diffs, not two blobs side by side:
        // "no entry matched {150 bytes} against {400 bytes}" is unreadable
        // at 3am, and every one of these fields is a single scalar that can
        // say exactly how it differs.
        let Some(closest) = candidates
            .iter()
            .map(|status| status_entry_diffs(status, expected_entry))
            .min_by_key(Vec::len)
        else {
            failures.push(Failure::new(
                "A7",
                format!("resource {resource_id:?}: {what} names it, but no status entry for it was ever observed"),
            ));
            continue;
        };
        for diff in closest {
            failures.push(Failure::new(
                "A7",
                format!("resource {resource_id:?} ({what}): {diff}"),
            ));
        }
    }
}

/// `expect.status_read`: the fields only a `status.read` reply carries.
///
/// Two clocks, deliberately independent (spec/observation.md §7): a bearer
/// credential's expiry and a strong-authentication session's expiry are not
/// the same event on any provider that has both -- a Wise personal token
/// does not expire until it is revoked while its SCA re-authentication
/// lapses on a fixed schedule, and the EU/UK SCA window moved from 90 to
/// 180 days while banks kept enforcing 90. Collapse them into one field and
/// the host can no longer tell "reconnect this credential" from "re-run
/// this authentication step". A fixture therefore names values that
/// *differ*, so an implementation that reports one clock for both is caught
/// rather than passing on a coincidence.
fn assert_status_read(statuses_json: &[Value], expect: &Value, failures: &mut Vec<Failure>) {
    let Some(expected) = expect.get("status_read") else {
        return;
    };
    let mut by_resource: HashMap<String, Vec<Value>> = HashMap::new();
    for status in statuses_json {
        if let Some(rid) = status.get("resource_id").and_then(Value::as_str) {
            by_resource
                .entry(rid.to_owned())
                .or_default()
                .push(status.clone());
        }
    }
    assert_expected_statuses(&by_resource, expected, "expect.status_read", failures);
}

/// How one status entry fails to match what a fixture named. Every field
/// the fixture names must be present and equal -- except a field it names
/// as `null`, which must be **absent or null** on the entry.
///
/// `null` is how a fixture spells "the adapter must not claim to know
/// this", and it is load-bearing for the two clocks and `history_start`
/// (spec/observation.md §7). A credential that never expires until it is
/// revoked has no expiry date; a provider that promised nothing about how
/// far back its history goes has no `history_start`. Neither is a zero, an
/// epoch, or a far-future placeholder -- the same rule §2 states for a null
/// balance amount, and one a subset comparison alone cannot express, since
/// a subset can only ever say "this field is there and equal".
fn status_entry_diffs(actual: &Value, expected: &Value) -> Vec<String> {
    let mut diffs = Vec::new();
    let mut expected = expected.clone();
    if let Value::Object(map) = &mut expected {
        let unknown: Vec<String> = map
            .iter()
            .filter(|(_, v)| v.is_null())
            .map(|(k, _)| k.clone())
            .collect();
        for key in unknown {
            map.remove(&key);
            match actual.get(&key) {
                None | Some(Value::Null) => {}
                Some(claimed) => diffs.push(format!(
                    "status.{key} must be absent (unknown), got {} -- unknown is never a date, \
                     and never zero",
                    brief(claimed)
                )),
            }
        }
    }
    json_subset_diff(actual, &expected, "status", &mut diffs);
    diffs
}

// ---------------------------------------------------------------------
// stale_balance: A3 / A4 / A7. Separate calls per resource (not a batch)
// because the point is that checking-3's adversarial reply must not
// disturb checking-2 or the calls that come after it.
// ---------------------------------------------------------------------

async fn case_stale_balance(
    argv: &[String],
    path: &Path,
    fixture: &Value,
    failures: &mut Vec<Failure>,
) {
    let expect = fixture.get("expect").cloned().unwrap_or(Value::Null);
    let handle = match spawn_run(argv, path, 0, DEFAULT_DEADLINE).await {
        Ok(h) => h,
        Err(e) => {
            failures.push(Failure::new("setup", format!("spawn failed: {e}")));
            return;
        }
    };

    let resources_reply = match handle.resources_list().await {
        Ok(r) => r,
        Err(e) => {
            failures.push(Failure::new("setup", format!("resources.list failed: {e}")));
            return;
        }
    };
    let actual_json: Vec<Value> = resources_reply
        .resources
        .iter()
        .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
        .collect();
    if let Some(expected_list) = expect.get("resources").and_then(Value::as_array) {
        assert_resources(&actual_json, expected_list, failures);
    }

    // checking-2: the legitimate stale-cache leg.
    match handle.balances_read(vec!["checking-2".to_owned()]).await {
        Ok(reply) => {
            let statuses_json: Vec<Value> = reply
                .statuses
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                .collect();
            assert_status_coverage(&["checking-2".to_owned()], &statuses_json, failures);
            if let Some(expected_balances) = expect.get("balances") {
                assert_balances(&reply.observations, expected_balances, failures);
            }
            if let Some(expected_prov) = expect.get("provenance") {
                assert_provenance(&reply.observations, expected_prov, failures);
            }
            if let Some(expected_statuses) = expect.get("statuses") {
                let observed: HashMap<String, Vec<Value>> =
                    [("checking-2".to_owned(), statuses_json)].into();
                assert_expected_statuses(&observed, expected_statuses, "expect.statuses", failures);
            }
        }
        Err(e) => failures.push(Failure::new(
            "A3",
            format!("checking-2 balances.read unexpectedly failed: {e}"),
        )),
    }

    // checking-3: the adversarial leg. Its scripted reply's `provenance`
    // literally carries `received_at`, which the wire forbids (Ruling A2).
    // The whole reply must be rejected at the envelope level -- never a
    // `status` outcome, and never a value the caller can see. The shared
    // helper also re-checks that the connection survived it.
    if let Some(expected_errors) = expect.get("envelope_errors") {
        assert_envelope_errors(&handle, expected_errors, failures).await;
    }

    // A4 pairing: the invalid_request on checking-3's balances.read must
    // not kill the connection or block anything requested afterward.
    match handle
        .status_read(vec!["checking-2".to_owned(), "checking-3".to_owned()])
        .await
    {
        Ok(reply) => {
            let statuses_json: Vec<Value> = reply
                .statuses
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
                .collect();
            assert_status_coverage(
                &["checking-2".to_owned(), "checking-3".to_owned()],
                &statuses_json,
                failures,
            );
            assert_status_read(&statuses_json, &expect, failures);
        }
        Err(e) => failures.push(Failure::new(
            "A4",
            format!(
                "status.read(checking-2,checking-3) must still succeed after checking-3's \
                 invalid_request, but failed: {e}"
            ),
        )),
    }

    // A10 at the wire, on a fresh process: checking-2 only, the one leg
    // this case answers with a normal `ok` (checking-3's is scripted to be
    // rejected at the envelope, so it emits no observation to measure).
    drop(handle);
    wire_pass(
        argv,
        path,
        0,
        &["checking-2".to_owned()],
        "wire pass",
        failures,
    )
    .await;
}

// ---------------------------------------------------------------------
// unsupported_op: A4. Sends an op string outside the four capabilities via
// `AdapterHandle::call_raw`, on the same connection as the calls that must
// survive it.
// ---------------------------------------------------------------------

async fn case_unsupported_op(
    argv: &[String],
    path: &Path,
    fixture: &Value,
    failures: &mut Vec<Failure>,
) {
    let expect = fixture.get("expect").cloned().unwrap_or(Value::Null);
    let handle = match spawn_run(argv, path, 0, DEFAULT_DEADLINE).await {
        Ok(h) => h,
        Err(e) => {
            failures.push(Failure::new("setup", format!("spawn failed: {e}")));
            return;
        }
    };

    // Normal crawl, BEFORE the probe.
    let resources_reply = match handle.resources_list().await {
        Ok(r) => r,
        Err(e) => {
            failures.push(Failure::new("A4", format!("resources.list failed: {e}")));
            return;
        }
    };
    let actual_list: Vec<Value> = resources_reply
        .resources
        .iter()
        .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
        .collect();
    if let Some(expected_list) = expect.get("resources").and_then(Value::as_array) {
        assert_resources(&actual_list, expected_list, failures);
    }
    let resource_ids: Vec<String> = resources_reply
        .resources
        .iter()
        .map(|d| d.resource_id.clone())
        .collect();

    // THE PROBE: an op no declared capability names. `call_raw` is the only
    // way to send one, and it goes through the same spawn, the same
    // environment allowlist, and the same frame decoder as every other call
    // in this suite.
    let probe_op = expect
        .pointer("/probe/op")
        .and_then(Value::as_str)
        .unwrap_or("execute");
    let probe_params = expect
        .pointer("/probe/params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match handle.call_raw(probe_op, probe_params).await {
        Ok(Reply::Err { err, .. }) => {
            let code_json = serde_json::to_value(err.code).unwrap_or(Value::Null);
            let expected_code = expect
                .pointer("/probe/expected_err/code")
                .and_then(Value::as_str)
                .unwrap_or("unsupported");
            if code_json.as_str() != Some(expected_code) {
                failures.push(Failure::new(
                    "A4",
                    format!("probe op {probe_op:?}: expected err.code={expected_code:?}, got {code_json}"),
                ));
            }
            if let Some(expected_op) = expect
                .pointer("/probe/expected_err/detail/op")
                .and_then(Value::as_str)
            {
                let actual_op = err
                    .detail
                    .as_ref()
                    .and_then(|d| d.get("op"))
                    .and_then(Value::as_str);
                if actual_op != Some(expected_op) {
                    failures.push(Failure::new(
                        "A4",
                        format!(
                            "probe op {probe_op:?}: expected detail.op={expected_op:?}, got {actual_op:?}"
                        ),
                    ));
                }
            }
        }
        other => failures.push(Failure::new(
            "A4",
            format!(
                "probe op {probe_op:?}: expected an envelope err, got {other:?} -- an adapter \
                 must never close the connection over an op it did not declare"
            ),
        )),
    }

    // Normal crawl, AFTER the probe -- the connection must have survived.
    // Every op verified here is recorded in the same `"<op>"` /
    // `"<op>(<resource_id>)"` spelling `expect.must_still_succeed` uses, so
    // that list is a real obligation on this runner rather than a comment.
    let mut verified: BTreeSet<String> = BTreeSet::new();
    match handle.resources_list().await {
        Ok(_) => {
            verified.insert("resources.list".to_owned());
        }
        Err(e) => failures.push(Failure::new(
            "A4",
            format!("resources.list after the probe failed: {e}"),
        )),
    }
    if let Some(rid) = resource_ids.first() {
        match handle.balances_read(vec![rid.clone()]).await {
            Ok(_) => {
                verified.insert(format!("balances.read({rid})"));
            }
            Err(e) => failures.push(Failure::new(
                "A4",
                format!("balances.read after the probe failed: {e}"),
            )),
        }
        let before = failures.len();
        let mut fold = Fold::new();
        drain_history(
            &handle,
            rid,
            None,
            &mut fold,
            None,
            DrainFor::Assertion("A4"),
            failures,
        )
        .await;
        if failures.len() == before {
            verified.insert(format!("history.read({rid})"));
        }
        match handle.status_read(vec![rid.clone()]).await {
            Ok(_) => {
                verified.insert(format!("status.read({rid})"));
            }
            Err(e) => failures.push(Failure::new(
                "A4",
                format!("status.read after the probe failed: {e}"),
            )),
        }
    }

    if let Some(required) = expect.get("must_still_succeed").and_then(Value::as_array) {
        for op in required.iter().filter_map(Value::as_str) {
            if !verified.contains(op) {
                failures.push(Failure::new(
                    "A4",
                    format!(
                        "expect.must_still_succeed names {op:?}, but this run never verified it \
                         succeeded after the probe (verified: {verified:?})"
                    ),
                ));
            }
        }
    }

    // A10 at the wire, on a fresh process.
    drop(handle);
    wire_pass(argv, path, 0, &resource_ids, "wire pass", failures).await;
}

// ---------------------------------------------------------------------
// interrupted_pagination: A2 / A5. Two cursor families (exact,
// batch_restart), each: uninterrupted full run, then an interrupted-before
// run that crashes mid-page, then a fresh interrupted-after run resumed
// via sumer_host::paging::ResumeState.
// ---------------------------------------------------------------------

async fn case_interrupted_pagination(
    argv: &[String],
    path: &Path,
    fixture: &Value,
    failures: &mut Vec<Failure>,
) {
    let expect = fixture.get("expect").cloned().unwrap_or(Value::Null);
    for family_name in ["exact", "batch_restart"] {
        let Some(family) = expect.pointer(&format!("/families/{family_name}")).cloned() else {
            failures.push(Failure::new(
                "setup",
                format!("interrupted_pagination: expect.families.{family_name} missing"),
            ));
            continue;
        };
        run_pagination_family(argv, path, family_name, &family, failures).await;
    }
}

/// Reads a `families.<f>.<key>.index` run index out of the fixture.
fn run_index(family: &Value, key: &str) -> Option<u64> {
    family
        .pointer(&format!("/{key}/index"))
        .and_then(Value::as_u64)
}

async fn run_pagination_family(
    argv: &[String],
    path: &Path,
    family_name: &str,
    family: &Value,
    failures: &mut Vec<Failure>,
) {
    let Some(resource_id) = family
        .get("resource_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        failures.push(Failure::new(
            "setup",
            format!("{family_name}: missing resource_id"),
        ));
        return;
    };
    let (Some(full_idx), Some(before_idx), Some(after_idx)) = (
        run_index(family, "uninterrupted_run"),
        run_index(family, "interrupted_before_run"),
        run_index(family, "interrupted_after_run"),
    ) else {
        failures.push(Failure::new(
            "setup",
            format!("{family_name}: missing one of uninterrupted/interrupted_before/interrupted_after run index"),
        ));
        return;
    };

    // 1. Uninterrupted run: drive pagination purely by following the
    // wire's own `next` -- no resumption question is being asked here.
    let mut full_fold = Fold::new();
    {
        let handle = match spawn_run(argv, path, full_idx, DEFAULT_DEADLINE).await {
            Ok(h) => h,
            Err(e) => {
                failures.push(Failure::new(
                    "A5",
                    format!("{family_name}: uninterrupted run failed to spawn: {e}"),
                ));
                return;
            }
        };
        drain_history(
            &handle,
            &resource_id,
            None,
            &mut full_fold,
            None,
            DrainFor::Assertion("A5"),
            failures,
        )
        .await;
    }
    let full_live = live_content(&full_fold);
    if let Some(expected_full) = family.get("full_live_set").and_then(Value::as_array) {
        assert_declared_live_set(&full_live, expected_full, family_name, failures);
    }

    // A10 at the wire: the same uninterrupted run, read raw on a fresh
    // process. The two interrupted runs are deliberately cut short, so the
    // full page set only exists here.
    wire_pass(
        argv,
        path,
        full_idx,
        std::slice::from_ref(&resource_id),
        &format!("{family_name} wire pass"),
        failures,
    )
    .await;

    // 2. Interrupted-before run: normal pagination by `next`, feeding
    // sumer_host::paging::ResumeState so we know how to resume once it
    // crashes mid-page.
    let mut combined_fold = Fold::new();
    // `None` == "from the start of available history": the same absent
    // `page` the read began with, and what `batch_restart` correctly resends.
    let mut resume = ResumeState::new(None);
    let crashed = {
        let handle = match spawn_run(argv, path, before_idx, DEFAULT_DEADLINE).await {
            Ok(h) => h,
            Err(e) => {
                failures.push(Failure::new(
                    "A5",
                    format!("{family_name}: interrupted-before run failed to spawn: {e}"),
                ));
                return;
            }
        };
        drain_history(
            &handle,
            &resource_id,
            None,
            &mut combined_fold,
            Some(&mut resume),
            DrainFor::ExpectedCrash("A5"),
            failures,
        )
        .await
        .crashed
    };
    if !crashed {
        failures.push(Failure::new(
            "A5",
            format!(
                "{family_name}: the interrupted-before run completed without ever crashing -- \
                 the fixture expected a mid-batch kill"
            ),
        ));
    }

    // 3. Resume on a FRESH process. `ResumeState::next_request` -- the
    // non-negotiable call -- decides what to resend. Its `None` is not
    // "nothing to do": it is the absent `page` that means "from the start",
    // which is exactly what a `batch_restart` resume has to send.
    let resumed = {
        let handle = match spawn_run(argv, path, after_idx, DEFAULT_DEADLINE).await {
            Ok(h) => h,
            Err(e) => {
                failures.push(Failure::new(
                    "A5",
                    format!("{family_name}: interrupted-after run failed to spawn: {e}"),
                ));
                return;
            }
        };
        drain_history(
            &handle,
            &resource_id,
            resume.next_request(),
            &mut combined_fold,
            None,
            DrainFor::Assertion("A5"),
            failures,
        )
        .await
    };

    // The bracket: neither "re-emit everything" nor "emit nothing" passes
    // both halves -- and the comparison is over the observations' full
    // financial content, not their identities. An adapter that returns the
    // right `local_id`s carrying different amounts after a resume is
    // exactly the mutation a key-set comparison could not see.
    let combined_live = live_content(&combined_fold);
    for line in content_diff(&combined_live, &full_live) {
        failures.push(Failure::new(
            "A5",
            format!(
                "{family_name}: the resumed live set differs from the uninterrupted one: {line}"
            ),
        ));
    }
    if let Some(forbidden) = family
        .get("must_not_reemit_before_resume_cursor")
        .and_then(Value::as_array)
    {
        for f in forbidden.iter().filter_map(Value::as_str) {
            if resumed.local_ids.iter().any(|id| id == f) {
                failures.push(Failure::new(
                    "A5",
                    format!(
                        "{family_name}: {f:?} was re-emitted after resume, but it lies strictly \
                         before the resume cursor"
                    ),
                ));
            }
        }
    }
    if let Some(redelivered) = family
        .get("redelivered_on_resume")
        .and_then(Value::as_array)
    {
        for r in redelivered.iter().filter_map(Value::as_str) {
            if !resumed.local_ids.iter().any(|id| id == r) {
                failures.push(Failure::new(
                    "A5",
                    format!(
                        "{family_name}: {r:?} was expected to be re-delivered by a \
                         batch_restart resume (which resends from the batch's start), but the \
                         resumed run never emitted it"
                    ),
                ));
            }
        }
    }
    if let Some(max_frames) = family
        .get("max_post_resume_frame_count")
        .and_then(Value::as_u64)
    {
        if resumed.frames > max_frames {
            failures.push(Failure::new(
                "A5",
                format!(
                    "{family_name}: {} post-resume frames, expected <= {max_frames}",
                    resumed.frames
                ),
            ));
        }
    }
}

/// `families.<f>.full_live_set`: each entry names a `local_id` and whatever
/// financial fields the fixture pins on it. Checking the ids alone would let
/// both runs agree on the same wrong money.
fn assert_declared_live_set(
    live: &BTreeMap<String, Value>,
    expected: &[Value],
    family_name: &str,
    failures: &mut Vec<Failure>,
) {
    let expected_ids: BTreeSet<&str> = expected
        .iter()
        .filter_map(|e| e.get("local_id").and_then(Value::as_str))
        .collect();
    let actual_ids: BTreeSet<&str> = live.keys().map(String::as_str).collect();
    if expected_ids != actual_ids {
        failures.push(Failure::new(
            "A5",
            format!(
                "{family_name}: uninterrupted live set = {actual_ids:?}, expected {expected_ids:?}"
            ),
        ));
    }
    for entry in expected {
        let Some(local_id) = entry
            .get("local_id")
            .and_then(|v| expect_str(v, "families[].full_live_set[].local_id", failures))
        else {
            continue;
        };
        let Some(actual) = live.get(local_id) else {
            continue;
        };
        let mut diffs = Vec::new();
        diff_observation(
            actual,
            entry,
            &format!("{family_name}.full_live_set[{local_id}]"),
            &mut diffs,
        );
        for d in diffs {
            failures.push(Failure::new("A5", d));
        }
    }
}

// ---------------------------------------------------------------------
// protocol_violations: A4 / A11. Six independent process runs.
// ---------------------------------------------------------------------

async fn case_protocol_violations(
    argv: &[String],
    path: &Path,
    fixture: &Value,
    failures: &mut Vec<Failure>,
) {
    let expect = fixture.get("expect").cloned().unwrap_or(Value::Null);
    let deadline_ms = fixture
        .pointer("/conformance_hints/deadline_ms")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
    let deadline = Duration::from_millis(deadline_ms);

    let Some(runs) = expect.get("runs").and_then(Value::as_array).cloned() else {
        failures.push(Failure::new(
            "setup",
            "protocol_violations: expect.runs missing".to_owned(),
        ));
        return;
    };
    for run_entry in &runs {
        let Some(run_idx) = run_entry
            .get("index")
            .and_then(|v| expect_u64(v, "expect.runs[].index", failures))
        else {
            failures.push(Failure::new(
                "setup",
                "protocol_violations: an expect.runs entry names no run index".to_owned(),
            ));
            continue;
        };
        let label = run_entry
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let expected_kind = run_entry
            .pointer("/outcome/protocol_violation/kind")
            .and_then(Value::as_str)
            .map(str::to_owned);

        match run_idx {
            0 => {
                match spawn_run(argv, path, run_idx, deadline).await {
                    Err(HostError::ProtocolViolation(kind)) => {
                        assert_violation_kind(kind, expected_kind.as_deref(), &label, failures);
                    }
                    Err(e) => failures.push(Failure::new(
                        "A11",
                        format!("{label}: expected spawn to fail with a ProtocolViolation, got a different error: {e}"),
                    )),
                    Ok(_) => failures.push(Failure::new(
                        "A11",
                        format!("{label}: expected spawn to fail with a ProtocolViolation, but it succeeded"),
                    )),
                }
            }
            1 => check_survivable_then_kill(argv, path, run_idx, &label, expected_kind.as_deref(), deadline, failures).await,
            2 => {
                let expectation = RunExpectation {
                    label: &label,
                    kind: expected_kind.as_deref(),
                };
                check_violation_after_resources_list(argv, path, run_idx, &expectation, deadline, failures, true)
                    .await;
            }
            _ => {
                let expectation = RunExpectation {
                    label: &label,
                    kind: expected_kind.as_deref(),
                };
                check_violation_after_resources_list(argv, path, run_idx, &expectation, deadline, failures, false)
                    .await;
            }
        }
    }
}

fn assert_violation_kind(
    actual: ProtocolViolationKind,
    expected: Option<&str>,
    label: &str,
    failures: &mut Vec<Failure>,
) {
    let actual_name = format!("{actual:?}");
    match expected {
        Some(exp) if exp == actual_name => {}
        Some(exp) => failures.push(Failure::new(
            "A11",
            format!("{label}: expected ProtocolViolation::{exp}, got {actual_name}"),
        )),
        None => failures.push(Failure::new(
            "A11",
            format!("{label}: got ProtocolViolation::{actual_name}, but the fixture named no expected kind"),
        )),
    }
}

/// Re-issues a harmless op until the connection reports the violation it is
/// expected to have already suffered -- used for `duplicate_kill`, whose
/// violation is an *unsolicited* extra frame. Nothing the host was awaiting
/// carries it, so the only way to observe it is to ask again; whether the
/// reader loop has decoded that frame by the time the next request is
/// dispatched is a scheduling detail rather than a contract, which is why
/// this is bounded rather than a single call.
///
/// **Every outcome other than "the connection is still healthy" is a verdict
/// here, not a state to wait through.** A crash -- with or without an exit
/// status -- and a timeout both end this immediately as failures: the
/// connection is gone and no violation is coming. `AdapterCrashed { status:
/// None }` in particular is a real, distinguishable outcome (the adapter
/// died and nothing ever established why, which is what death by signal
/// looks like) and is treated as the failure it is. Only a successful reply
/// or an ordinary envelope `err` -- the fixture has no second rule scripted
/// for this op -- means the connection is alive and the frame has not landed
/// yet.
async fn wait_for_violation(
    handle: &AdapterHandle,
    timeout: Duration,
) -> Result<ProtocolViolationKind, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match handle.resources_list().await {
            Err(HostError::ProtocolViolation(kind)) => return Ok(kind),
            Err(HostError::AdapterCrashed { status }) => {
                return Err(format!(
                    "adapter died (exit {status:?}) with no protocol violation ever established"
                ))
            }
            Err(e @ (HostError::Timeout | HostError::Spawn(_) | HostError::IdsExhausted)) => {
                return Err(format!("{e} instead of a protocol violation"))
            }
            // The only two "not settled yet" outcomes: the adapter answered,
            // or it answered with an ordinary envelope error because the
            // fixture scripts no second rule for this op. Both mean the
            // connection is alive and the unsolicited frame has not landed.
            Ok(_) | Err(HostError::Wire(_)) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for the expected protocol violation".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Bundles a run's diagnostic label and its expected `ProtocolViolationKind`
/// (as its Rust variant name, e.g. `"OversizeFrame"`) -- kept together
/// purely to stay under clippy's argument-count lint on the functions below.
struct RunExpectation<'a> {
    label: &'a str,
    kind: Option<&'a str>,
}

async fn check_violation_after_resources_list(
    argv: &[String],
    path: &Path,
    run_idx: u64,
    expectation: &RunExpectation<'_>,
    deadline: Duration,
    failures: &mut Vec<Failure>,
    needs_poll: bool,
) {
    let label = expectation.label;
    let handle = match spawn_run(argv, path, run_idx, deadline).await {
        Ok(h) => h,
        Err(e) => {
            failures.push(Failure::new("A11", format!("{label}: spawn failed: {e}")));
            return;
        }
    };
    if let Err(e) = handle.resources_list().await {
        failures.push(Failure::new(
            "A11",
            format!("{label}: resources.list unexpectedly failed: {e}"),
        ));
        return;
    }
    if needs_poll {
        match wait_for_violation(&handle, Duration::from_secs(3)).await {
            Ok(kind) => assert_violation_kind(kind, expectation.kind, label, failures),
            Err(msg) => failures.push(Failure::new("A11", format!("{label}: {msg}"))),
        }
    } else {
        match handle.balances_read(vec!["res-a".to_owned()]).await {
            Err(HostError::ProtocolViolation(kind)) => {
                assert_violation_kind(kind, expectation.kind, label, failures)
            }
            other => failures.push(Failure::new(
                "A11",
                format!("{label}: expected a ProtocolViolation on balances.read, got {other:?}"),
            )),
        }
    }
}

/// A reply's `statuses` name exactly the one resource its request asked
/// about, and nothing else. Every requested `resource_id` appears in
/// `statuses` exactly once (spec/observation.md §6), so for a single-
/// resource request that array *is* the reply's identity.
fn assert_reply_belongs_to(
    statuses: &[sumer_wire::ResourceStatus],
    resource_id: &str,
    label: &str,
    failures: &mut Vec<Failure>,
) {
    let named: Vec<&str> = statuses.iter().map(|s| s.resource_id.as_str()).collect();
    if named != [resource_id] {
        failures.push(Failure::new(
            "A11",
            format!(
                "{label}: the reply to the request for {resource_id:?} carried statuses for \
                 {named:?} -- a reply is correlated to its own request by id, never to whichever \
                 request happened to be answered first"
            ),
        ));
    }
}

/// Run 1, `survivable_then_kill`: the one run that exercises A4 (must-
/// still-succeed) and A11 (tombstoned-discard survives; the final reply's
/// bogus id kills) together, on one connection.
async fn check_survivable_then_kill(
    argv: &[String],
    path: &Path,
    run_idx: u64,
    label: &str,
    expected_kind: Option<&str>,
    deadline: Duration,
    failures: &mut Vec<Failure>,
) {
    let handle = match spawn_run(argv, path, run_idx, deadline).await {
        Ok(h) => h,
        Err(e) => {
            failures.push(Failure::new("A4", format!("{label}: spawn failed: {e}")));
            return;
        }
    };
    if let Err(e) = handle.resources_list().await {
        failures.push(Failure::new(
            "A4",
            format!("{label}: resources.list failed: {e}"),
        ));
        return;
    }

    // Sent SEQUENTIALLY, res-b first. res-a's reply is deliberately withheld
    // past the (shortened) deadline, and a serial adapter is fully legal
    // (spec/wire.md §7), so dispatching both at once would risk res-b
    // queueing behind res-a on a conforming serial adapter. Sequential
    // dispatch is correct against any declared `max_in_flight`.
    match handle.balances_read(vec!["res-b".to_owned()]).await {
        Ok(reply) => assert_reply_belongs_to(&reply.statuses, "res-b", label, failures),
        Err(e) => failures.push(Failure::new(
            "A4",
            format!("{label}: balances.read(res-b) unexpectedly failed: {e}"),
        )),
    }
    match handle.balances_read(vec!["res-a".to_owned()]).await {
        Err(HostError::Timeout) => {}
        other => failures.push(Failure::new(
            "A11",
            format!("{label}: balances.read(res-a) expected Timeout (tombstoned/discarded), got {other:?}"),
        )),
    }

    // history.read(res-a) is deferred; history.read(res-b) is answered
    // first and triggers the deferred res-a reply -- concurrently, so the
    // host must correlate by id, not arrival order. This run's hello
    // declares `max_in_flight: 2`, which is what makes concurrent dispatch
    // legal here.
    let (hist_a, hist_b) = tokio::join!(
        handle.history_read(vec![ResourceQuery {
            resource_id: "res-a".to_owned(),
            page: None,
        }]),
        handle.history_read(vec![ResourceQuery {
            resource_id: "res-b".to_owned(),
            page: None,
        }])
    );
    // Each reply must carry the content belonging to ITS request. Checking
    // only that both succeeded asserts nothing about ordering -- swap the
    // two payloads and a success-only check still passes, which is the one
    // thing this leg exists to rule out. The `id` is the only correlation
    // the envelope has (spec/wire.md §6: no `op`/`params` echo), so a host
    // that matched replies by arrival order would hand res-a's caller
    // res-b's page here and be caught by exactly this.
    match hist_a {
        Ok(reply) => assert_reply_belongs_to(&reply.statuses, "res-a", label, failures),
        Err(e) => failures.push(Failure::new(
            "A4",
            format!("{label}: history.read(res-a) unexpectedly failed: {e}"),
        )),
    }
    match hist_b {
        Ok(reply) => assert_reply_belongs_to(&reply.statuses, "res-b", label, failures),
        Err(e) => failures.push(Failure::new(
            "A4",
            format!("{label}: history.read(res-b) unexpectedly failed: {e}"),
        )),
    }

    // The final, fatal violation: the reply names an id no counter would
    // ever have issued.
    match handle
        .status_read(vec!["res-a".to_owned(), "res-b".to_owned()])
        .await
    {
        Err(HostError::ProtocolViolation(kind)) => assert_violation_kind(kind, expected_kind, label, failures),
        other => failures.push(Failure::new(
            "A11",
            format!("{label}: expected the final status.read to trigger a ProtocolViolation, got {other:?}"),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn provider_extra_targets_scans_prose() {
        let found = provider_extra_targets(
            "lands in provider_extra.fdx_status (verbatim) AND provider_extra.fdx_amount_unsigned.",
        );
        assert_eq!(found, vec!["fdx_status", "fdx_amount_unsigned"]);
    }

    #[test]
    fn content_diff_catches_same_ids_with_different_money() {
        let a: BTreeMap<String, Value> =
            [("x".to_owned(), serde_json::json!({"amount": "1"}))].into();
        let b: BTreeMap<String, Value> =
            [("x".to_owned(), serde_json::json!({"amount": "999999"}))].into();
        assert_eq!(
            content_diff(&a, &b).len(),
            1,
            "identical key sets, different content"
        );
    }

    /// A wire observation with every optional field absent, padded so its
    /// serialized form is exactly `bytes` long.
    fn wire_observation_of(bytes: usize) -> Value {
        let mut observation = serde_json::json!({
            "resource_id": "r1",
            "local_id": "boundary",
            "state": "active",
            "surface": "onchain",
            "posting": "posted",
            "amount": {"asset": "sat", "amount": "1"},
            "raw_sign": "provider_positive",
            "description": "",
            "provider_extra": {},
            "provenance": {
                "adapter_id": "a",
                "provider_id": "p",
                "surface": "onchain",
                "observed_at": "2026-09-01T10:00:00Z",
                "completeness": "complete"
            }
        });
        let empty = serde_json::to_vec(&observation).unwrap().len();
        observation["description"] = Value::String("x".repeat(bytes - empty));
        assert_eq!(serde_json::to_vec(&observation).unwrap().len(), bytes);
        observation
    }

    #[test]
    fn an_observation_exactly_at_the_cap_is_legal() {
        let mut failures = Vec::new();
        assert_wire_size(
            &wire_observation_of(MAX_OBSERVATION_BYTES),
            "$",
            &mut failures,
        );
        assert!(
            failures.is_empty(),
            "MAX_OBSERVATION_BYTES is a cap, not a limit one below it: {failures:?}"
        );
    }

    #[test]
    fn one_byte_over_the_cap_is_not() {
        let mut failures = Vec::new();
        assert_wire_size(
            &wire_observation_of(MAX_OBSERVATION_BYTES + 1),
            "$",
            &mut failures,
        );
        assert_eq!(failures.len(), 1, "65537 bytes must fail A10");
    }

    /// Why A10 is measured at the wire and not on the host's decoded copy:
    /// the same legal record, once decoded and re-serialized, is over the
    /// cap purely from nulls `ObservationWire` never put on the wire.
    /// Measuring that form rejected conforming adapters.
    #[test]
    fn the_decoded_form_is_not_the_wire_form() {
        let raw = wire_observation_of(MAX_OBSERVATION_BYTES);
        let wire: sumer_wire::ObservationWire = serde_json::from_value(raw).unwrap();
        let stamped = sumer_wire::Observation::stamp(
            wire,
            sumer_wire::Rfc3339::new("2026-09-06T00:00:00Z".to_owned()).unwrap(),
            sumer_wire::Staleness::Live,
        );
        let decoded_bytes = serde_json::to_vec(&adapter_view(&stamped)).unwrap().len();
        assert!(
            decoded_bytes > MAX_OBSERVATION_BYTES,
            "expected the decoded form to be inflated past the cap, got {decoded_bytes}"
        );
    }

    /// A9's regression: an adapter that mis-derives the `local_id` of an
    /// observation later superseded or tombstoned still lands on an
    /// identical final live set. Comparing the live set alone saw nothing.
    #[test]
    fn a9_compares_the_whole_chain_not_just_the_final_state() {
        let first: BTreeMap<String, Value> = [(
            "tx".to_owned(),
            serde_json::json!([
                {"local_id": "tx", "state": "active"},
                {"local_id": "tx", "state": "tombstoned"},
                {"local_id": "tx", "state": "active"},
            ]),
        )]
        .into();
        let second: BTreeMap<String, Value> = [
            (
                "tx".to_owned(),
                serde_json::json!([
                    {"local_id": "tx", "state": "active"},
                    {"local_id": "tx", "state": "active"},
                ]),
            ),
            (
                "wrong-id".to_owned(),
                serde_json::json!([{"local_id": "wrong-id", "state": "tombstoned"}]),
            ),
        ]
        .into();
        let mut failures = Vec::new();
        assert_local_id_purity(&first, &second, &mut failures);
        assert_eq!(
            failures.len(),
            2,
            "expected the short chain and the stray id: {failures:?}"
        );
    }

    /// spec/observation.md §7's two clocks and `history_start`: a fixture
    /// spells "must not claim to know this" as `null`, and an adapter that
    /// answers with a date instead fails. Unknown is never an epoch.
    #[test]
    fn a_null_in_an_expected_status_entry_means_absent_not_any_value() {
        let expected = serde_json::json!({
            "credential_expires_at": null,
            "strong_auth_expires_at": "2026-12-05T00:00:00Z"
        });
        let honest = serde_json::json!({"strong_auth_expires_at": "2026-12-05T00:00:00Z"});
        assert!(status_entry_diffs(&honest, &expected).is_empty());

        let invented = serde_json::json!({
            "credential_expires_at": "1970-01-01T00:00:00Z",
            "strong_auth_expires_at": "2026-12-05T00:00:00Z"
        });
        assert_eq!(status_entry_diffs(&invented, &expected).len(), 1);

        // The two clocks are independent: reporting one for both is the
        // conflation §7 exists to prevent, and must not match.
        let conflated = serde_json::json!({
            "credential_expires_at": "2026-12-05T00:00:00Z",
            "strong_auth_expires_at": "2026-12-05T00:00:00Z"
        });
        assert_eq!(status_entry_diffs(&conflated, &expected).len(), 1);
    }

    #[test]
    fn provider_extra_is_compared_exactly_not_as_a_subset() {
        let actual = serde_json::json!({"provider_extra": {"_truncated": true, "leaked": "x"}});
        let expected = serde_json::json!({"provider_extra": {"_truncated": true}});
        let mut diffs = Vec::new();
        diff_observation(&actual, &expected, "$", &mut diffs);
        assert_eq!(
            diffs.len(),
            1,
            "a leaked extra key must not pass: {diffs:?}"
        );
    }
}
