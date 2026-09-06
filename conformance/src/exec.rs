//! One adapter execution, judged before it can be looked at.
//!
//! # The three properties this module exists to enforce
//!
//! **1. One execution, both views.** A crawl is spawned through
//! [`AdapterHandle::spawn_recorded`], so the typed evidence (the
//! [`Fold`], the decoded observations, the statuses) and the raw evidence
//! (the reply frames A10 is measured on) come from **the same bytes of the
//! same process**. The design this replaces ran a typed crawl and then one
//! or more separate "wire passes" on fresh processes -- and an adapter
//! could tell them apart, because the typed crawl opened with
//! `resources.list` and a wire pass opened with `balances.read`. Returning
//! real data to one and empty arrays to the other passed the suite. There
//! is now no second pass to diverge from: the transcript is a recording of
//! the crawl that already happened.
//!
//! **2. An unjudged execution is unobtainable.** [`JudgedExecution`]'s
//! fields are private, [`assert_execution`] is private, and the only
//! constructor is [`run_crawl`], which judges before it returns. A driver
//! that forgets to assert does not compile, because there is nothing to
//! forget: it never holds an unjudged execution in the first place. "Every
//! driver remembers to call the assertion" is exactly the discipline that
//! failed seven times, and it is not shipped again.
//!
//! **3. Raw measurement lives inside that judgement.** A10 measures every
//! observation in [`JudgedExecution::raw_observations`] -- parsed out of
//! the transcript -- for **every** execution: resumption runs, post-probe
//! reads, all of them. It measures the adapter's own wire bytes and never
//! a re-serialization of the host's decoded form, because the host emits
//! nulls the wire omits. Measuring the decoded form previously produced
//! both a false positive at exactly 65,536 bytes and a missed violation
//! one byte over.
//!
//! # The honest limit
//!
//! What is **structural** here -- impossible to forget, because a type or
//! a constructor enforces it -- is: per-execution sequence equality, the
//! A10 size measurement, status coverage, and the resources list. Every
//! execution gets all four whether its driver asks or not.
//!
//! What is still **discipline** -- cross-execution claims no type in this
//! crate can force -- is: the A5 resume bracket, A9's comparison of two
//! executions, A4's must-still-succeed probes, and A11's violation-kind
//! checks. They live at call sites in `runner.rs` and a driver that
//! omitted one would still compile. Historically *most* of this suite's
//! hollow assertions lived exactly there. A reviewer looking for the next
//! hollow assertion should look there first.
//!
//! # The accepted aperture
//!
//! A discarded, tombstoned reply is never recorded (`Mux::deliver` drops
//! it before it reaches any caller, including the transcript), and that
//! drop is legally not a violation -- spec/wire.md §6, "the connection
//! survives". So an oversized observation riding in on a reply that
//! arrived after its own deadline is invisible to A10's measurement,
//! forever. This is accepted, not fixed: the aperture is bounded by the
//! deadline that caused the tombstone, never unbounded. It is written down
//! so the next reviewer does not rediscover it as a surprise.

use crate::assert::{
    assert_status_coverage, brief, expect_array, expect_object, expect_str, expect_u64,
    json_subset_diff, stable_view, to_json, Failure,
};
use crate::ledger::{compare_sequence, parse_declared, Completeness, Ledger, ObsKind, SequenceRef};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use sumer_host::fold::Fold;
use sumer_host::paging::ResumeState;
use sumer_host::{AdapterHandle, Exchange, HostError};
use sumer_wire::{
    CursorResumable, PageRequest, Reply, ResourceQuery, MAX_OBSERVATION_BYTES, OP_BALANCES_READ,
    OP_HISTORY_READ,
};

/// Every fixture with more than this many pages for one resource is
/// treated as hung rather than followed forever -- a defensive cap, not
/// part of the wire contract.
const MAX_PAGES: u32 = 500;

/// The literal a cursor's bytes are replaced by in a request shape.
const CURSOR_PLACEHOLDER: &str = "<cursor>";

/// State a *pair* of executions shares across the resume boundary: the
/// paging state one run's kill leaves behind for the next run to resume
/// from, and the fold that spans both.
///
/// Both come from `sumer-host` and are never reimplemented here -- a suite
/// carrying its own second implementation of resumption or of revision
/// assignment could not catch a real host and this suite silently
/// disagreeing about either.
pub(crate) struct Resumption {
    resume: ResumeState,
    fold: Fold,
}

impl Resumption {
    /// A resumption pair starting from an absent `page` -- "from the start
    /// of available history" (spec/observation.md §5, Ruling A8).
    pub(crate) fn new() -> Resumption {
        Resumption {
            resume: ResumeState::new(None),
            fold: Fold::new(),
        }
    }

    /// The live set folded across both runs, as financial content keyed by
    /// `local_id` -- what the A5 bracket compares against an uninterrupted
    /// execution.
    pub(crate) fn live_content(&self) -> BTreeMap<String, Value> {
        live_content(&self.fold)
    }
}

/// How much of its declared ledger an execution was obliged to emit.
///
/// **A driver constant. Never a fixture key.** See
/// [`crate::ledger::Completeness`] for why, and
/// `case_interrupted_pagination` for the only four `Truncated` call sites
/// in this crate.
pub(crate) enum Mode<'a> {
    /// Read to exhaustion; the emitted sequence must equal the declared one.
    Complete,
    /// A run that is deliberately cut short, or one resuming after such a
    /// run. `across` carries the paging state and the fold shared with its
    /// partner run; `killed` says whether this run is *expected* to die
    /// mid-read (in which case the death is the exercise, not a failure).
    Truncated {
        across: &'a mut Resumption,
        killed: bool,
    },
}

/// One completed, already-judged adapter execution.
///
/// Every field is private and the only constructor is [`run_crawl`]. The
/// accessors below expose *derived* evidence for the cross-execution
/// assertions in `runner.rs`; there is no way to obtain one of these
/// without its per-execution judgement having already run.
pub(crate) struct JudgedExecution {
    label: String,
    fold: Fold,
    ledger: Ledger,
    /// This execution's own recording: every request/reply that crossed
    /// its connection, taken off the handle before it was dropped. The raw
    /// half of "one execution, both views".
    transcript: Vec<Exchange>,
    request_shapes: Vec<Value>,
    resource_ids: Vec<String>,
    crashed: bool,
}

impl JudgedExecution {
    /// `<case>[run <n>]` -- every failure message this execution produces
    /// starts with it.
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// This execution's live set as financial content keyed by `local_id`:
    /// amount, state, posting, surface, provider_extra -- everything that
    /// constitutes the picture, not just the identities. Comparing key sets
    /// alone is vacuous: an adapter can return the expected `local_id`s
    /// carrying entirely different money.
    pub(crate) fn live_content(&self) -> BTreeMap<String, Value> {
        live_content(&self.fold)
    }

    /// The `local_id` -> emitted-records association over this execution's
    /// whole history, in arrival order. A9 compares this between two
    /// executions.
    pub(crate) fn history_by_local_id(&self) -> BTreeMap<String, Value> {
        self.ledger.history_by_local_id()
    }

    /// Every request this execution dispatched, with cursor bytes replaced
    /// -- see [`request_shape`].
    pub(crate) fn request_shapes(&self) -> &[Value] {
        &self.request_shapes
    }

    /// Whether the adapter died part-way through this execution.
    pub(crate) fn crashed(&self) -> bool {
        self.crashed
    }

    /// Every `local_id` this execution emitted for one resource's history,
    /// in arrival order, duplicates kept.
    pub(crate) fn emitted_history_ids(&self, resource_id: &str) -> Vec<&str> {
        self.ledger
            .sequence(ObsKind::History, resource_id)
            .iter()
            .filter_map(|e| e.local_id.as_deref())
            .collect()
    }

    /// How many `history.read` reply frames this execution drained for one
    /// resource.
    pub(crate) fn history_frames(&self, resource_id: &str) -> u64 {
        self.ledger.frames(ObsKind::History, resource_id)
    }

    /// Every observation the adapter put on the wire during this execution, as
    /// the raw JSON it arrived as -- before the host decoded it, dropped
    /// anything over the cap, or stamped a single field onto it.
    ///
    /// This is not a second pass and not a second client: it is a recording of
    /// the very calls the typed crawl above made, taken off the same
    /// connection. A fixture cannot behave differently for it, because there
    /// is no "it" to detect.
    fn raw_observations(&self, failures: &mut Vec<Failure>) -> Vec<(String, Value)> {
        let label = &self.label;
        let mut out = Vec::new();
        for (i, exchange) in self.transcript.iter().enumerate() {
            if exchange.op != OP_BALANCES_READ && exchange.op != OP_HISTORY_READ {
                continue;
            }
            // No frame: the request was tombstoned, or the connection ended
            // first. See this module's "accepted aperture" note.
            let Some(frame) = &exchange.frame else {
                continue;
            };
            let Ok(reply) = serde_json::from_str::<Value>(frame) else {
                failures.push(Failure::new(
                    "A11",
                    format!("{label}: exchange {i} ({}) is not JSON", exchange.op),
                ));
                continue;
            };
            // An `err` envelope is a legitimate scripted outcome and carries no
            // observations to measure.
            let Some(ok) = reply.get("ok") else {
                continue;
            };
            match ok.get("observations") {
                Some(Value::Array(items)) => {
                    for (j, raw) in items.iter().enumerate() {
                        let named = raw
                            .get("local_id")
                            .and_then(Value::as_str)
                            .map_or_else(|| format!("observation {j}"), |l| format!("{l:?}"));
                        out.push((format!("{label}: {} {named}", exchange.op), raw.clone()));
                    }
                }
                other => failures.push(Failure::new(
                    "A2",
                    format!(
                        "{label}: a {} reply body has no `observations` array (got {})",
                        exchange.op,
                        other.map_or_else(|| "nothing".to_owned(), brief)
                    ),
                )),
            }
        }
        out
    }
}

/// Drives one full read crawl against a fresh adapter process and judges
/// it, returning evidence only after every per-execution assertion has run.
///
/// The crawl is `resources.list` -> (optional undeclared-op probe) ->
/// `balances.read` -> paginated `history.read` -> `status.read` ->
/// (optional envelope-error probes), all on one connection.
///
/// **It awaits SERIALLY, and must keep doing so.** The transcript is
/// *dispatch* order (`sumer_host::Exchange`), and reply CONTENT is ordered
/// the same way only when dispatch is itself serial: an adapter declaring
/// `max_in_flight` above 1 may answer out of order, and two entries'
/// frames would then fill in in a different order than the entries appear.
/// Every sequence this module compares is an ordering claim, so
/// introducing `tokio::join!` (or any other concurrent dispatch) here
/// would silently turn those claims into coin flips. Await each call
/// before issuing the next.
///
/// `expect` is the fixture's `expect` block -- or, for
/// `interrupted_pagination`, one family's sub-block, which carries the
/// same keys.
pub(crate) async fn run_crawl(
    argv: &[String],
    path: &Path,
    run: u64,
    expect: &Value,
    mode: Mode<'_>,
    failures: &mut Vec<Failure>,
) -> JudgedExecution {
    let label = format!(
        "{}[run {run}]",
        path.file_stem().map_or_else(
            || path.display().to_string(),
            |s| s.to_string_lossy().into_owned()
        )
    );
    let mut exec = JudgedExecution {
        label: label.clone(),
        fold: Fold::new(),
        ledger: Ledger::default(),
        transcript: Vec::new(),
        request_shapes: Vec::new(),
        resource_ids: Vec::new(),
        crashed: false,
    };
    let (completeness, mut across, killed) = match mode {
        Mode::Complete => (Completeness::Complete, None, false),
        Mode::Truncated { across, killed } => (Completeness::Truncated, Some(across), killed),
    };

    let handle = match AdapterHandle::spawn_recorded(argv.to_vec(), env_for(path, run)).await {
        Ok(h) => h,
        Err(e) => {
            failures.push(Failure::new("setup", format!("{label}: spawn failed: {e}")));
            return exec;
        }
    };

    let mut statuses_by_resource: HashMap<String, Vec<Value>> = HashMap::new();
    let mut status_read_statuses: Vec<Value> = Vec::new();
    // Every op verified to still work *after* the undeclared-op probe, in
    // the `"<op>"` / `"<op>(<resource_id>)"` spelling
    // `expect.must_still_succeed` uses -- so that list is a real obligation
    // rather than a comment.
    let mut verified: BTreeSet<String> = BTreeSet::new();
    // Every `provider_extra` key seen anywhere: one half of
    // `expect.fdx_field_map`'s "nothing silently dropped, nothing
    // undocumented" claim.
    let mut emitted_provider_extra: BTreeSet<String> = BTreeSet::new();

    // 1. resources.list
    let resources = match handle.resources_list().await {
        Ok(r) => r,
        Err(e) => {
            failures.push(Failure::new(
                "setup",
                format!("{label}: resources.list failed: {e}"),
            ));
            return finish(exec, &handle, expect, completeness, across, failures);
        }
    };
    let resources_json: Vec<Value> = resources.resources.iter().map(to_json).collect();
    if let Some(declared) = expect.get("resources") {
        assert_resources(&label, &resources_json, declared, failures);
    }
    emitted_provider_extra.extend(provider_extra_keys(&resources_json));
    exec.resource_ids = resources
        .resources
        .iter()
        .map(|d| d.resource_id.clone())
        .collect();
    if exec.resource_ids.is_empty() {
        failures.push(Failure::new(
            "setup",
            format!("{label}: resources.list returned no resources"),
        ));
        return finish(exec, &handle, expect, completeness, across, failures);
    }
    if across.is_some() && exec.resource_ids.len() != 1 {
        failures.push(Failure::new(
            "setup",
            format!(
                "{label}: a truncated (resumption) execution reads one resource, but \
                 resources.list named {:?} -- one ResumeState cannot stand for several",
                exec.resource_ids
            ),
        ));
    }

    // 2. The undeclared-op probe, if this fixture has one. It goes here,
    // early, so that EVERY read below is a post-probe read: A4's "the
    // connection survives an op the adapter never declared" is then the
    // rest of this crawl, not a separate afterthought.
    if let Some(probe) = expect.get("probe") {
        assert_probe(&handle, &label, probe, failures).await;
        match handle.resources_list().await {
            Ok(_) => {
                verified.insert("resources.list".to_owned());
            }
            Err(e) => failures.push(Failure::new(
                "A4",
                format!("{label}: resources.list after the probe failed: {e}"),
            )),
        }
    }

    // 3. balances.read. ONE batched call naming every discovered resource,
    // always. Batching is what makes A7's "every requested `resource_id`
    // appears in `statuses` exactly once" worth checking rather than
    // vacuously true of a one-element request, and it is what makes
    // `stale_balance`'s two resources come back `cached` and `live` out of
    // the SAME reply -- a per-resource derivation a blanket stamp cannot
    // fake.
    {
        let batch = exec.resource_ids.clone();
        match handle.balances_read(batch.clone()).await {
            Ok(reply) => {
                let statuses_json: Vec<Value> = reply.statuses.iter().map(to_json).collect();
                assert_status_coverage(&batch, &statuses_json, failures);
                for status in &statuses_json {
                    if let Some(rid) = status.get("resource_id").and_then(Value::as_str) {
                        statuses_by_resource
                            .entry(rid.to_owned())
                            .or_default()
                            .push(status.clone());
                    }
                }
                for balance in &reply.observations {
                    let json = stable_view(&to_json(balance));
                    emitted_provider_extra.extend(provider_extra_keys(std::slice::from_ref(&json)));
                    exec.ledger
                        .push(ObsKind::Balances, &balance.resource_id, None, json);
                }
                for rid in &batch {
                    exec.ledger.count_frame(ObsKind::Balances, rid);
                    verified.insert(format!("balances.read({rid})"));
                }
                if let Some(expected) = expect.get("grammar_check") {
                    assert_grammar_check(&label, &reply.observations, expected, failures);
                }
                if let Some(expected) = expect.get("provenance") {
                    assert_provenance(&label, &reply.observations, expected, failures);
                }
            }
            Err(HostError::AdapterCrashed { status }) => {
                exec.crashed = true;
                if !killed {
                    failures.push(Failure::new(
                        "A2",
                        format!("{label}: the adapter died (exit {status:?}) during balances.read"),
                    ));
                }
                return finish(exec, &handle, expect, completeness, across, failures);
            }
            Err(e) => failures.push(Failure::new(
                "A2",
                format!("{label}: balances.read({batch:?}) failed: {e}"),
            )),
        }
    }

    // 4. history.read, one resource at a time, paginated to completion,
    // folded through `sumer_host::fold::Fold`.
    let resource_ids = exec.resource_ids.clone();
    for rid in &resource_ids {
        let start = across.as_deref().and_then(|a| a.resume.next_request());
        let before = failures.len();
        let statuses = drain_history(
            &handle,
            rid,
            start,
            &mut exec,
            across.as_deref_mut(),
            killed,
            failures,
        )
        .await;
        statuses_by_resource
            .entry(rid.clone())
            .or_default()
            .extend(statuses);
        // "Verified" means the read actually went through clean -- a drain
        // that logged anything is not evidence that anything still works.
        if failures.len() == before {
            verified.insert(format!("history.read({rid})"));
        }
        if exec.crashed {
            break;
        }
    }
    for rid in &resource_ids {
        let keys: Vec<Value> = exec
            .ledger
            .sequence(ObsKind::History, rid)
            .iter()
            .map(|e| e.json.clone())
            .collect();
        emitted_provider_extra.extend(provider_extra_keys(&keys));
    }

    // 5. status.read: the only reply that populates `credential_expires_at`
    // / `strong_auth_expires_at` / `history_start` (spec/observation.md §7),
    // so `expect.status_read` is checked against these entries alone --
    // folding them in with the balances/history statuses would let a
    // fixture's claim about the clocks be satisfied by a reply that never
    // carried them.
    if !exec.crashed {
        match handle.status_read(resource_ids.clone()).await {
            Ok(reply) => {
                status_read_statuses = reply.statuses.iter().map(to_json).collect();
                assert_status_coverage(&resource_ids, &status_read_statuses, failures);
                for rid in &resource_ids {
                    verified.insert(format!("status.read({rid})"));
                }
            }
            Err(e) => failures.push(Failure::new(
                "A7",
                format!("{label}: status.read failed: {e}"),
            )),
        }
    }

    // 6. Any envelope-level rejection the fixture demands, on this same
    // connection -- and the connection has to survive it.
    if let Some(expected) = expect.get("envelope_errors") {
        assert_envelope_errors(&handle, &label, expected, failures).await;
    }

    // 7. The fixture's own list of what had to survive the probe.
    if let Some(required) = expect.get("must_still_succeed") {
        if let Some(required) = expect_array(required, "expect.must_still_succeed", failures) {
            for op in required.iter().filter_map(Value::as_str) {
                if !verified.contains(op) {
                    failures.push(Failure::new(
                        "A4",
                        format!(
                            "{label}: expect.must_still_succeed names {op:?}, but this execution \
                             never verified it succeeded after the probe (verified: {verified:?})"
                        ),
                    ));
                }
            }
        }
    }

    if let Some(expected) = expect.get("statuses") {
        assert_expected_statuses(
            &label,
            &statuses_by_resource,
            expected,
            "expect.statuses",
            failures,
        );
    }
    if let Some(expected) = expect.get("status_read") {
        let mut by_resource: HashMap<String, Vec<Value>> = HashMap::new();
        for status in &status_read_statuses {
            if let Some(rid) = status.get("resource_id").and_then(Value::as_str) {
                by_resource
                    .entry(rid.to_owned())
                    .or_default()
                    .push(status.clone());
            }
        }
        assert_expected_statuses(
            &label,
            &by_resource,
            expected,
            "expect.status_read",
            failures,
        );
    }
    if let Some(field_map) = expect.get("fdx_field_map") {
        assert_fdx_field_map(&label, field_map, &emitted_provider_extra, failures);
    }

    finish(exec, &handle, expect, completeness, across, failures)
}

/// Takes the transcript off the connection, runs the per-execution
/// judgement, and hands back the (now judged) execution. Every early
/// return in [`run_crawl`] goes through here -- an execution that failed
/// half-way is still judged on what it did emit.
fn finish(
    mut exec: JudgedExecution,
    handle: &AdapterHandle,
    expect: &Value,
    completeness: Completeness,
    across: Option<&mut Resumption>,
    failures: &mut Vec<Failure>,
) -> JudgedExecution {
    exec.transcript = handle.transcript();
    exec.request_shapes = exec.transcript.iter().map(request_shape).collect();
    let live_fold = across.map_or(&exec.fold, |a| &a.fold);
    assert_execution(&exec, expect, completeness, live_fold, failures);
    exec
}

/// The per-execution judgement. Private, and called from exactly one place
/// -- [`run_crawl`], before it returns. Nothing outside this module can
/// obtain a [`JudgedExecution`] that has not been through here.
fn assert_execution(
    exec: &JudgedExecution,
    expect: &Value,
    completeness: Completeness,
    live_fold: &Fold,
    failures: &mut Vec<Failure>,
) {
    let label = &exec.label;

    // A10: every observation the adapter put on the wire, measured as the
    // adapter emitted it. Every execution, every op, no exceptions.
    for (what, raw) in exec.raw_observations(failures) {
        assert_wire_size(&raw, &what, failures);
    }

    // R4: per-resource sequence equality, both sequences, both directions.
    let Some(ledger) = expect.get("ledger") else {
        failures.push(Failure::new(
            "setup",
            format!(
                "{label}: the fixture declares no `expect.ledger`, so nothing holds this \
                 execution's emitted observations to anything"
            ),
        ));
        return;
    };
    let declared = parse_declared(ledger, label, failures);
    // The union, not the declared side: **an absent sequence asserts
    // EMPTY.** A fixture that leaves `history` (or `balances`, or a whole
    // resource) off `expect.ledger` is not opting out of the comparison,
    // it is declaring that nothing may arrive there -- anything the
    // adapter emits under an undeclared key is an orphan with a declared
    // length of zero. Absence meaning "unchecked" is the hollow-assertion
    // disease at the format level: it would let an adapter invent a whole
    // history unchallenged, and let a fixture silently drop a check by
    // deleting one key.
    let mut keys: BTreeSet<(String, ObsKind)> = declared.seqs.keys().cloned().collect();
    keys.extend(exec.ledger.keys());
    for (resource_id, kind) in keys {
        let declared_seq = declared
            .seqs
            .get(&(resource_id.clone(), kind))
            .map_or(&[][..], Vec::as_slice);
        compare_sequence(
            &SequenceRef {
                label,
                resource_id: &resource_id,
                kind,
            },
            declared_seq,
            exec.ledger.sequence(kind, &resource_id),
            completeness,
            failures,
        );
    }

    // A record the adapter was required to drop entirely (spec/observation.md
    // §6 step 2) must appear in NO sequence of this execution -- not
    // truncated, not anywhere.
    let emitted_ids = exec.ledger.local_ids();
    for local_id in &declared.omitted {
        if emitted_ids.contains(local_id.as_str()) {
            failures.push(Failure::new(
                "A10",
                format!(
                    "{label}: local_id {local_id:?} was declared fully OMITTED (still too large \
                     after truncating provider_extra), but the adapter emitted it anyway"
                ),
            ));
        }
    }

    assert_live(label, expect, live_fold, completeness, failures);
}

/// `expect.live`: the `local_id`s that must be live once the fold settles,
/// per resource.
///
/// **Declared, deliberately not derived.** Folding the declared ledger to
/// compute the expected live set would make this check tautological -- the
/// same `Fold` on both sides, agreeing with itself. The fixture states the
/// answer independently, so a wrong fold is a difference rather than a
/// coincidence.
fn assert_live(
    label: &str,
    expect: &Value,
    fold: &Fold,
    completeness: Completeness,
    failures: &mut Vec<Failure>,
) {
    let Some(live) = expect.get("live") else {
        failures.push(Failure::new(
            "setup",
            format!("{label}: the fixture declares no `expect.live`"),
        ));
        return;
    };
    let Some(by_resource) = expect_object(live, &format!("{label}: expect.live"), failures) else {
        return;
    };
    let actual = fold.live_set();
    let mut declared_everywhere: BTreeSet<String> = BTreeSet::new();
    for (resource_id, ids) in by_resource {
        let Some(ids) = expect_array(
            ids,
            &format!("{label}: expect.live.{resource_id}"),
            failures,
        ) else {
            continue;
        };
        let declared: BTreeSet<String> = ids
            .iter()
            .filter_map(|v| {
                expect_str(
                    v,
                    &format!("{label}: expect.live.{resource_id}[]"),
                    failures,
                )
            })
            .map(str::to_owned)
            .collect();
        declared_everywhere.extend(declared.iter().cloned());
        let emitted: BTreeSet<String> = actual
            .values()
            .filter(|r| &r.observation.resource_id == resource_id)
            .map(|r| r.observation.local_id.clone())
            .collect();
        if !declared.is_empty() && emitted.is_empty() {
            failures.push(Failure::new(
                "A6",
                format!(
                    "{label}: resource {resource_id:?} declares a non-empty live set, but the \
                     fold produced an empty one -- \"report success\" with nothing behind it"
                ),
            ));
            continue;
        }
        for extra in emitted.difference(&declared) {
            failures.push(Failure::new(
                "A2",
                format!(
                    "{label}: resource {resource_id:?} has {extra:?} live, which expect.live \
                     does not declare"
                ),
            ));
        }
        if completeness == Completeness::Complete {
            for missing in declared.difference(&emitted) {
                failures.push(Failure::new(
                    "A2",
                    format!(
                        "{label}: resource {resource_id:?} declares {missing:?} live, but the \
                         fold does not -- live local_ids: {emitted:?}"
                    ),
                ));
            }
        }
    }
    for ((_, local_id), _) in actual {
        if !declared_everywhere.contains(local_id) {
            failures.push(Failure::new(
                "A2",
                format!(
                    "{label}: {local_id:?} is live, but expect.live declares it under no resource"
                ),
            ));
        }
    }
}

/// A10: an observation **the adapter put on the wire** that exceeds
/// `MAX_OBSERVATION_BYTES` means the two-step degrade of
/// spec/observation.md §6 was not performed.
///
/// `raw` must be an observation exactly as it arrived in the reply frame.
/// Measuring anything else cannot work, in either direction:
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

/// One dispatched request reduced to its **shape**: the op, plus the params
/// with every cursor's bytes replaced by the literal `"<cursor>"`.
///
/// A9 gates on two executions producing the same shapes before it compares
/// their content, so "the second run asked different questions" is
/// reported as itself rather than as a mysterious content difference.
///
/// **Only `resources[].page.cursor` is stripped.** Window bounds, resource
/// ids and page counts all survive, because those are host-chosen and a
/// difference in any of them is a real divergence. A cursor is not: it
/// carries no cross-invocation purity obligation -- spec/observation.md §5
/// calls an intermediate `batch_restart` cursor untrusted, and only
/// `local_id` is required to be a pure function. Comparing raw cursor bytes
/// would FAIL A CONFORMING ADAPTER that mints session-scoped cursors, which
/// is Plaid's model and the reason `batch_restart` exists at all.
fn request_shape(exchange: &Exchange) -> Value {
    let mut params = exchange.params.clone();
    if let Some(resources) = params.get_mut("resources").and_then(Value::as_array_mut) {
        for resource in resources.iter_mut() {
            if let Some(cursor) = resource.pointer_mut("/page/cursor") {
                *cursor = Value::String(CURSOR_PLACEHOLDER.to_owned());
            }
        }
    }
    serde_json::json!({"op": exchange.op, "params": params})
}

// ---------------------------------------------------------------------
// The crawl's steps
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

/// The live set as *financial content*, keyed by bare `local_id`.
///
/// `Fold` keys `(adapter_id, local_id)` because a real host runs several
/// adapters at once; this suite drives exactly one per execution and the
/// fixtures name bare local_ids. The adapter_id is still inside each value
/// (`provenance.adapter_id`), so a run that changed it shows up as a
/// content difference.
fn live_content(fold: &Fold) -> BTreeMap<String, Value> {
    fold.live_set()
        .into_iter()
        .map(|((_, local_id), r)| (local_id.to_owned(), stable_view(&to_json(&r.observation))))
        .collect()
}

/// Drains one resource's history to exhaustion, folding as it goes and
/// appending every observation to the execution's ledger in arrival order.
///
/// `start` is what the first request carries in `page`: `None` means an
/// absent `page` field, i.e. "from the start of available history"
/// (spec/observation.md §5, Ruling A8). Every page after the first uses the
/// cursor the adapter itself returned -- this crawl never invents one, and
/// never asks an adapter for a cursor it did not hand out.
///
/// Pagination follows the **typed decode of the same frame**: the `next`
/// comes off `sumer_wire::PageReply`, not off a re-parsed JSON value. A
/// value-based read collapses duplicate keys before any deserializer sees
/// them, which is precisely the check the typed path exists to keep alive.
async fn drain_history(
    handle: &AdapterHandle,
    resource_id: &str,
    start: Option<PageRequest>,
    exec: &mut JudgedExecution,
    mut across: Option<&mut Resumption>,
    tolerate_crash: bool,
    failures: &mut Vec<Failure>,
) -> Vec<Value> {
    let label = exec.label.clone();
    let requested = [resource_id.to_owned()];
    let mut statuses_out = Vec::new();
    let mut page = start;
    let mut pages = 0_u32;
    loop {
        pages += 1;
        if pages > MAX_PAGES {
            failures.push(Failure::new(
                "setup",
                format!(
                    "{label}: history.read({resource_id}) exceeded {MAX_PAGES} pages -- \
                     treating as hung"
                ),
            ));
            return statuses_out;
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
                exec.crashed = true;
                if !tolerate_crash {
                    failures.push(Failure::new(
                        "A2",
                        format!(
                            "{label}: history.read({resource_id}): the adapter died (exit \
                             {status:?}) part-way through the read"
                        ),
                    ));
                }
                return statuses_out;
            }
            Err(e) => {
                failures.push(Failure::new(
                    "A2",
                    format!("{label}: history.read({resource_id}) failed: {e}"),
                ));
                return statuses_out;
            }
        };
        exec.ledger.count_frame(ObsKind::History, resource_id);

        let statuses_json: Vec<Value> = reply.statuses.iter().map(to_json).collect();
        assert_status_coverage(&requested, &statuses_json, failures);
        statuses_out.extend(statuses_json);

        for observation in reply.observations {
            exec.ledger.push(
                ObsKind::History,
                &observation.resource_id,
                Some(observation.local_id.clone()),
                stable_view(&to_json(&observation)),
            );
            if let Some(a) = across.as_deref_mut() {
                a.fold.ingest(observation.clone());
            }
            exec.fold.ingest(observation);
        }

        let (resumable, next) = reply
            .statuses
            .into_iter()
            .next()
            .and_then(|s| s.page)
            .map_or((CursorResumable::None, None), |p| {
                (p.cursor_resumable, p.next)
            });
        if let Some(a) = across.as_deref_mut() {
            a.resume.record(resumable, next.clone());
        }
        match next {
            Some(n) => page = Some(n),
            None => return statuses_out,
        }
    }
}

/// The undeclared-op probe (`expect.probe`): an op no declared capability
/// names must come back as an envelope `err`, never as a closed
/// connection. `call_raw` is the only way to send one, and it goes through
/// the same spawn, the same environment allowlist, and the same frame
/// decoder as every other call in this suite.
async fn assert_probe(
    handle: &AdapterHandle,
    label: &str,
    probe: &Value,
    failures: &mut Vec<Failure>,
) {
    let op = probe.get("op").and_then(Value::as_str).unwrap_or("execute");
    let params = probe
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    match handle.call_raw(op, params).await {
        Ok(Reply::Err { err, .. }) => {
            let code = to_json(&err.code);
            let expected_code = probe
                .pointer("/expected_err/code")
                .and_then(Value::as_str)
                .unwrap_or("unsupported");
            if code.as_str() != Some(expected_code) {
                failures.push(Failure::new(
                    "A4",
                    format!(
                        "{label}: probe op {op:?}: expected err.code={expected_code:?}, got {code}"
                    ),
                ));
            }
            if let Some(expected_op) = probe
                .pointer("/expected_err/detail/op")
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
                            "{label}: probe op {op:?}: expected detail.op={expected_op:?}, got \
                             {actual_op:?}"
                        ),
                    ));
                }
            }
        }
        other => failures.push(Failure::new(
            "A4",
            format!(
                "{label}: probe op {op:?}: expected an envelope err, got {other:?} -- an adapter \
                 must never close the connection over an op it did not declare"
            ),
        )),
    }
}

/// `expect.envelope_errors`, keyed `"<resource_id>_<op>"` (`balances_read`
/// or `history_read`): that call must be rejected at the envelope level with
/// the named `err.code`, and -- since the whole point is that an envelope
/// error is not a connection error -- the connection must still be usable
/// afterwards.
async fn assert_envelope_errors(
    handle: &AdapterHandle,
    label: &str,
    expected: &Value,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(
        expected,
        &format!("{label}: expect.envelope_errors"),
        failures,
    ) else {
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
                    "{label}: envelope_errors key {key:?} names no op -- expected \
                     \"<resource_id>_balances_read\" or \"<resource_id>_history_read\""
                ),
            ));
            continue;
        };
        let expected_code = expected_body.get("code").and_then(Value::as_str);
        match result {
            Some(HostError::Wire(err)) => {
                let actual_code = to_json(&err.code);
                if actual_code.as_str() != expected_code {
                    failures.push(Failure::new(
                        "A4",
                        format!(
                            "{label}: {resource_id}: expected envelope err.code={expected_code:?}, \
                             got {actual_code} ({})",
                            err.message
                        ),
                    ));
                }
            }
            other => failures.push(Failure::new(
                "A4",
                format!(
                    "{label}: {key}: expected an envelope err with code {expected_code:?}, got \
                     {other:?}"
                ),
            )),
        }
    }
    // A4's pairing half: an envelope error is scoped to the one request it
    // answers and must never take the connection with it.
    if let Err(e) = handle.resources_list().await {
        failures.push(Failure::new(
            "A4",
            format!("{label}: the connection did not survive the envelope error: {e}"),
        ));
    }
}

// ---------------------------------------------------------------------
// The remaining `expect.*` keys, judged inside the crawl that produced them
// ---------------------------------------------------------------------

/// `expect.resources` names every resource the adapter may list, and only
/// those. The count matters for the same reason it does on balances: an
/// invented resource contradicts nothing on its own, so a
/// present-and-correct check alone certifies an adapter that reports an
/// account the provider never had.
fn assert_resources(label: &str, actual: &[Value], expected: &Value, failures: &mut Vec<Failure>) {
    let Some(expected_list) =
        expect_array(expected, &format!("{label}: expect.resources"), failures)
    else {
        return;
    };
    if actual.len() != expected_list.len() {
        // A1, not `setup`: an adapter that returns a resource the provider
        // never had is fabricating an account, and nothing else in the model
        // can contradict it -- the same reason the balances list is compared
        // by count. `setup` means THIS fixture is malformed, which would
        // read an adapter's invention as our own authoring mistake.
        failures.push(Failure::new(
            "A1",
            format!(
                "{label}: resources.list returned {} resource(s), expected {} -- actual ids: {:?}",
                actual.len(),
                expected_list.len(),
                actual
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
        match actual
            .iter()
            .find(|a| a.get("resource_id").and_then(Value::as_str) == Some(resource_id))
        {
            None => failures.push(Failure::new(
                "setup",
                format!("{label}: resources.list did not return resource_id {resource_id:?}"),
            )),
            Some(actual) => {
                let mut diffs = Vec::new();
                json_subset_diff(
                    actual,
                    expected,
                    &format!("{label}: resources[{resource_id}]"),
                    &mut diffs,
                );
                for d in diffs {
                    failures.push(Failure::new("setup", d));
                }
            }
        }
    }
}

/// `expect.grammar_check`: extra digit-count / scale evidence beyond
/// `cmp_same_asset`, keyed `"<category>_digit_count"` / `"<category>_scale"`.
fn assert_grammar_check(
    label: &str,
    actual: &[sumer_wire::Balance],
    grammar: &Value,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(grammar, &format!("{label}: expect.grammar_check"), failures)
    else {
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
                    "{label}: expect.grammar_check key {key:?} ends in neither \"_digit_count\" \
                     nor \"_scale\", so nothing reads it"
                ),
            ));
            continue;
        };
        let Some(balance) = actual.iter().find(|b| b.category == category) else {
            failures.push(Failure::new(
                "A1",
                format!(
                    "{label}: grammar_check names category {category:?}, which no balance reported"
                ),
            ));
            continue;
        };
        let Some(amount) = &balance.amount else {
            failures.push(Failure::new(
                "A1",
                format!(
                    "{label}: grammar_check names category {category:?}, but its amount is null \
                     (unknown) -- there are no digits to count"
                ),
            ));
            continue;
        };
        let Some(expected) = expect_u64(expected_val, key, failures) else {
            continue;
        };
        if is_scale {
            let actual_scale = u64::from(amount.scale());
            if actual_scale != expected {
                failures.push(Failure::new(
                    "A1",
                    format!(
                        "{label}: grammar_check: category {category:?} scale = {actual_scale}, \
                         expected {expected}"
                    ),
                ));
            }
        } else {
            let rendered = amount.to_string();
            let actual_digits =
                u64::try_from(rendered.bytes().filter(u8::is_ascii_digit).count()).unwrap_or(0);
            if actual_digits != expected {
                failures.push(Failure::new(
                    "A1",
                    format!(
                        "{label}: grammar_check: category {category:?} has {actual_digits} \
                         significant digits (from {rendered:?}), expected {expected}"
                    ),
                ));
            }
        }
    }
}

/// `expect.provenance`: checked field-by-field via the generic subset
/// comparator -- `staleness` included. It is host-computed and this
/// milestone's host has no cache layer, so `"live"` is the only conforming
/// value unless the resource's own outcome says otherwise; a fixture that
/// claims something else is asserting what the contract forbids the
/// adapter from influencing.
fn assert_provenance(
    label: &str,
    actual: &[sumer_wire::Balance],
    expected: &Value,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(expected, &format!("{label}: expect.provenance"), failures)
    else {
        return;
    };
    for (resource_id, expected_prov) in map {
        let Some(balance) = actual.iter().find(|b| &b.resource_id == resource_id) else {
            failures.push(Failure::new(
                "A3",
                format!(
                    "{label}: expect.provenance names {resource_id:?}, which returned no balance"
                ),
            ));
            continue;
        };
        let mut diffs = Vec::new();
        json_subset_diff(
            &to_json(&balance.provenance),
            expected_prov,
            &format!("{label}: provenance.{resource_id}"),
            &mut diffs,
        );
        for d in diffs {
            failures.push(Failure::new("A3", d));
        }
    }
}

/// Checks that, across every status observed for a resource, at least one
/// **entry** matches the expected shape as a subset.
///
/// The match is against the whole status entry, not just its `outcome`,
/// because a status entry states two independent facts: `outcome` (the
/// freshness one, which spec/observation.md §1's staleness table reads) and
/// `degraded` (a record dropped for size). A fixture that could only name
/// the outcome could not assert the thing §6 is emphatic about -- that a
/// degrade leaves the outcome alone.
fn assert_expected_statuses(
    label: &str,
    by_resource: &HashMap<String, Vec<Value>>,
    expected: &Value,
    what: &str,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(expected, &format!("{label}: {what}"), failures) else {
        return;
    };
    for (resource_id, expected_entry) in map {
        let candidates = by_resource.get(resource_id).cloned().unwrap_or_default();
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
                format!(
                    "{label}: resource {resource_id:?}: {what} names it, but no status entry for \
                     it was ever observed"
                ),
            ));
            continue;
        };
        for diff in closest {
            failures.push(Failure::new(
                "A7",
                format!("{label}: resource {resource_id:?} ({what}): {diff}"),
            ));
        }
    }
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
/// epoch, or a far-future placeholder.
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

/// `expect.fdx_field_map`: the FDX 6.4 mapping table `fdx_lossless` gates,
/// checked in both directions. Every `provider_extra.<key>` the map names
/// must actually have arrived on the wire ("nothing is silently dropped"),
/// and every `provider_extra` key that did arrive must be named by the map
/// ("nothing arrives undocumented").
fn assert_fdx_field_map(
    label: &str,
    field_map: &Value,
    emitted: &BTreeSet<String>,
    failures: &mut Vec<Failure>,
) {
    let Some(map) = expect_object(
        field_map,
        &format!("{label}: expect.fdx_field_map"),
        failures,
    ) else {
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
            format!(
                "{label}: expect.fdx_field_map names no provider_extra target at all -- it \
                 asserts nothing"
            ),
        ));
    }
    for key in named.difference(emitted) {
        failures.push(Failure::new(
            "A1",
            format!(
                "{label}: fdx_field_map says an FDX field lands in provider_extra.{key}, but no \
                 observation carried that key -- the mapping claims a field is preserved that \
                 was silently dropped"
            ),
        ));
    }
    for key in emitted.difference(&named) {
        failures.push(Failure::new(
            "A1",
            format!(
                "{label}: provider_extra.{key} arrived on the wire but fdx_field_map documents \
                 no FDX field landing there"
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

/// Every `provider_extra` key present on any of these JSON values.
fn provider_extra_keys(values: &[Value]) -> BTreeSet<String> {
    values
        .iter()
        .filter_map(|v| v.get("provider_extra"))
        .filter_map(Value::as_object)
        .flat_map(|m| m.keys().cloned())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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

    /// Why A10 is measured on the adapter's own wire bytes and not on the
    /// host's decoded copy: the same legal record, once decoded and
    /// re-serialized, is over the cap purely from nulls `ObservationWire`
    /// never put on the wire. Measuring that form rejected conforming
    /// adapters.
    #[test]
    fn the_decoded_form_is_not_the_wire_form() {
        let raw = wire_observation_of(MAX_OBSERVATION_BYTES);
        let wire: sumer_wire::ObservationWire = serde_json::from_value(raw).unwrap();
        let stamped = sumer_wire::Observation::stamp(
            wire,
            sumer_wire::Rfc3339::new("2026-09-06T00:00:00Z".to_owned()).unwrap(),
            sumer_wire::Staleness::Live,
        );
        let decoded_bytes = serde_json::to_vec(&stable_view(&to_json(&stamped)))
            .unwrap()
            .len();
        assert!(
            decoded_bytes > MAX_OBSERVATION_BYTES,
            "expected the decoded form to be inflated past the cap, got {decoded_bytes}"
        );
    }

    /// A9's gate compares request *shapes*. A conforming adapter that
    /// mints a fresh, session-scoped cursor on every launch -- Plaid's
    /// model, and the reason `batch_restart` exists -- must not fail it.
    #[test]
    fn request_shape_hides_cursor_bytes_and_nothing_else() {
        let shape = |cursor: &str| {
            request_shape(&Exchange {
                op: OP_HISTORY_READ.to_owned(),
                params: serde_json::json!({
                    "resources": [{
                        "resource_id": "r1",
                        "page": {"kind": "cursor", "cursor": cursor}
                    }]
                }),
                frame: None,
                received_at: None,
            })
        };
        assert_eq!(shape("session-a:7"), shape("session-b:99"));
        assert_eq!(
            shape("x").pointer("/params/resources/0/page/cursor"),
            Some(&Value::String(CURSOR_PLACEHOLDER.to_owned()))
        );
        // The resource id is not a cursor and must still separate two
        // executions that asked different questions.
        let other = request_shape(&Exchange {
            op: OP_HISTORY_READ.to_owned(),
            params: serde_json::json!({
                "resources": [{"resource_id": "r2", "page": {"kind": "cursor", "cursor": "x"}}]
            }),
            frame: None,
            received_at: None,
        });
        assert_ne!(shape("x"), other);
    }

    #[test]
    fn provider_extra_targets_scans_prose() {
        let found = provider_extra_targets(
            "lands in provider_extra.fdx_status (verbatim) AND provider_extra.fdx_amount_unsigned.",
        );
        assert_eq!(found, vec!["fdx_status", "fdx_amount_unsigned"]);
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
}
