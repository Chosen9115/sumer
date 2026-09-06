//! Drives one conformance case (a `conformance/cases/*.json` fixture)
//! against a black-box adapter and reports every failed assertion.
//!
//! This file is **drivers only**. One execution -- spawn, crawl, judge --
//! lives in [`crate::exec`], and the evidence model it judges against lives
//! in [`crate::ledger`]. What is left here is the part no type can enforce:
//! which executions a case needs, and what has to hold *between* them.
//!
//! Every case is driven through the real [`sumer_host::AdapterHandle`] --
//! the same supervisor a production host uses -- so id lifecycle, protocol
//! violation detection, and provenance stamping are exercised for real, not
//! reimplemented here. There is exactly one client in this crate.
//!
//! Resumption ([`sumer_host::paging::ResumeState`]) and revision assignment
//! / the live-set fold ([`sumer_host::fold::Fold`]) are never reimplemented
//! here -- the frozen contract requires calling into `sumer-host` for both,
//! so a real host and this suite's expectations are checked against one
//! implementation, not two that could silently diverge.
//!
//! **Nothing in this file may be specific to the reference Python adapter.**
//! A read starts with an *absent* `page` (spec/observation.md section 5 and
//! Ruling A8: absent means "from the start of available history"), and every
//! subsequent page request is a cursor the adapter itself returned. An
//! adapter that only honours its own opaque cursors -- which is exactly what
//! the contract permits -- must survive this crawl unchanged.

use crate::assert::{content_diff, expect_array, expect_u64, Failure};
use crate::exec::{run_crawl, JudgedExecution, Mode, Resumption};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use sumer_host::{AdapterHandle, HostError};
use sumer_wire::{ProtocolViolationKind, ResourceQuery};

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
        "interrupted_pagination" => {
            case_interrupted_pagination(argv, path, &fixture, &mut failures).await;
        }
        "protocol_violations" => {
            case_protocol_violations(argv, path, &fixture, &mut failures).await;
        }
        // Every other case is the standard crawl. What varies between them
        // -- an undeclared-op probe, an envelope error, how balances are
        // batched -- is named in the fixture's `expect` block and read by
        // the one crawl, rather than by a driver per case.
        _ => case_crawl(argv, path, &fixture, &mut failures).await,
    }

    CaseOutcome { case, failures }
}

/// The request deadline every connection this fixture opens is built with:
/// `conformance_hints.deadline_ms` when the fixture names one, else the
/// frozen contract's 30-second default (spec/wire.md §7).
///
/// The hint is advisory and non-normative (spec/wire.md §11) -- a runner
/// may ignore it entirely and stay conformant. This one honours it,
/// because the scenarios that need it are the ones whose whole point is a
/// deadline *expiring*: a tombstoned reply that must be discarded, and an
/// adapter that must be caught not exiting at stdin EOF. Neither is
/// observable before the deadline passes, and at the default that is 30
/// seconds of wall clock per execution to learn one bit.
fn deadline_of(fixture: &Value) -> Duration {
    Duration::from_millis(
        fixture
            .pointer("/conformance_hints/deadline_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000),
    )
}

// ---------------------------------------------------------------------
// The standard case: two judged executions of the same crawl.
// ---------------------------------------------------------------------

/// Runs the crawl twice and holds the pair to A9.
///
/// Both executions are **fully judged** on their own (sequences, live set,
/// A10 sizes, statuses), which is what stops A9 from being made vacuous by
/// an adapter that simply empties both sides: two identical empty runs
/// agree with each other perfectly and fail their own ledger check.
async fn case_crawl(argv: &[String], path: &Path, fixture: &Value, failures: &mut Vec<Failure>) {
    let expect = fixture.get("expect").cloned().unwrap_or(Value::Null);
    let deadline = deadline_of(fixture);
    let first = run_crawl(argv, path, 0, &expect, Mode::Complete, deadline, failures).await;
    let second = run_crawl(argv, path, 0, &expect, Mode::Complete, deadline, failures).await;
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
///   touches.
///
/// **The request shapes are gated first.** If the two executions asked
/// different questions, a content difference says nothing about purity, so
/// the divergence is reported as itself. Cursor bytes are excluded from
/// that comparison -- see `exec::request_shape` for why comparing them
/// would fail a conforming adapter.
///
/// This assertion is **discipline, not structure**: nothing forces a driver
/// to call it. See the honest-limit note at the top of `exec.rs`.
fn assert_local_id_purity(
    first: &JudgedExecution,
    second: &JudgedExecution,
    failures: &mut Vec<Failure>,
) {
    let (a, b) = (first.request_shapes(), second.request_shapes());
    if a != b {
        let at = a
            .iter()
            .zip(b)
            .position(|(x, y)| x != y)
            .unwrap_or_else(|| a.len().min(b.len()));
        failures.push(Failure::new(
            "A9",
            format!(
                "{}: the two executions dispatched different requests, so their contents cannot \
                 be compared for purity -- they diverge at request {at}: {:?} vs {:?} ({} vs {} \
                 requests in total; cursor bytes are already excluded from this comparison)",
                first.label(),
                a.get(at),
                b.get(at),
                a.len(),
                b.len()
            ),
        ));
        return;
    }
    for line in content_diff(&second.history_by_local_id(), &first.history_by_local_id()) {
        failures.push(Failure::new(
            "A9",
            format!(
                "{}: the local_id -> provider-record association differs between two independent \
                 process invocations of the same fixture, across the full observation history \
                 (not just the final live set): {line}",
                first.label()
            ),
        ));
    }
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
    let deadline = deadline_of(fixture);
    for family_name in ["exact", "batch_restart"] {
        let Some(family) = expect.pointer(&format!("/families/{family_name}")).cloned() else {
            failures.push(Failure::new(
                "setup",
                format!("interrupted_pagination: expect.families.{family_name} missing"),
            ));
            continue;
        };
        run_pagination_family(argv, path, family_name, &family, deadline, failures).await;
    }
}

/// Reads a `families.<f>.<key>.index` run index out of the fixture.
fn run_index(family: &Value, key: &str) -> Option<u64> {
    family
        .pointer(&format!("/{key}/index"))
        .and_then(Value::as_u64)
}

/// The only four `Mode::Truncated` call sites in this crate are in here.
///
/// A family's declared ledger is the **whole** sequence; three executions
/// read different amounts of it, and only the runner -- never the fixture
/// -- knows which:
///
/// * the uninterrupted run must emit all of it (`Complete`);
/// * the killed run emits a **prefix** of it, because it dies mid-read;
/// * the resumed run emits a **suffix** (`exact`, which resumes from the
///   last page's cursor) or the **whole thing** (`batch_restart`, which
///   resends from the batch's start -- spec/observation.md section 5).
///
/// All three of those are contiguous slices, which is exactly what
/// `Truncated` accepts. It relaxes nothing else: wrong money inside the
/// slice, an observation nobody declared, and a record that was supposed to
/// be omitted still fail.
async fn run_pagination_family(
    argv: &[String],
    path: &Path,
    family_name: &str,
    family: &Value,
    deadline: Duration,
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
            format!(
                "{family_name}: missing one of uninterrupted/interrupted_before/interrupted_after \
                 run index"
            ),
        ));
        return;
    };

    // 1. The uninterrupted run: pagination driven purely by the wire's own
    // `next`, no resumption question asked.
    let full = run_crawl(
        argv,
        path,
        full_idx,
        family,
        Mode::Complete,
        deadline,
        failures,
    )
    .await;

    // 2 + 3. The killed run and the run that resumes it, sharing one
    // `ResumeState` (which decides what to resend) and one `Fold` (which
    // spans the resume boundary). Both are host code; neither is
    // reimplemented here.
    let mut across = Resumption::new();
    let killed = run_crawl(
        argv,
        path,
        before_idx,
        family,
        Mode::Truncated {
            across: &mut across,
            killed: true,
        },
        deadline,
        failures,
    )
    .await;
    if !killed.crashed() {
        failures.push(Failure::new(
            "A5",
            format!(
                "{family_name}: the interrupted-before run completed without ever crashing -- the \
                 fixture expected a mid-batch kill, so this run proved nothing about resumption"
            ),
        ));
    }
    let resumed = run_crawl(
        argv,
        path,
        after_idx,
        family,
        Mode::Truncated {
            across: &mut across,
            killed: false,
        },
        deadline,
        failures,
    )
    .await;
    if resumed.crashed() {
        failures.push(Failure::new(
            "A5",
            format!("{family_name}: the resuming run died part-way through its own read"),
        ));
    }

    // The bracket: neither "re-emit everything" nor "emit nothing" passes
    // both halves -- and the comparison is over the observations' full
    // financial content, not their identities. An adapter that returns the
    // right `local_id`s carrying different amounts after a resume is
    // exactly the mutation a key-set comparison could not see.
    //
    // Discipline, not structure: nothing forces this call. See the
    // honest-limit note at the top of `exec.rs`.
    for line in content_diff(&across.live_content(), &full.live_content()) {
        failures.push(Failure::new(
            "A5",
            format!(
                "{family_name}: the live set folded across the kill and the resume differs from \
                 the uninterrupted one: {line}"
            ),
        ));
    }

    let emitted = resumed.emitted_history_ids(&resource_id);
    if let Some(forbidden) = family.get("must_not_reemit_before_resume_cursor") {
        if let Some(forbidden) = expect_array(
            forbidden,
            &format!("{family_name}: must_not_reemit_before_resume_cursor"),
            failures,
        ) {
            for f in forbidden.iter().filter_map(Value::as_str) {
                if emitted.contains(&f) {
                    failures.push(Failure::new(
                        "A5",
                        format!(
                            "{family_name}: {f:?} was re-emitted after resume, but it lies \
                             strictly before the resume cursor"
                        ),
                    ));
                }
            }
        }
    }
    if let Some(redelivered) = family.get("redelivered_on_resume") {
        if let Some(redelivered) = expect_array(
            redelivered,
            &format!("{family_name}: redelivered_on_resume"),
            failures,
        ) {
            for r in redelivered.iter().filter_map(Value::as_str) {
                if !emitted.contains(&r) {
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
    }
    if let Some(max_frames) = family.get("max_post_resume_frame_count") {
        if let Some(max_frames) = expect_u64(
            max_frames,
            &format!("{family_name}: max_post_resume_frame_count"),
            failures,
        ) {
            let actual = resumed.history_frames(&resource_id);
            if actual > max_frames {
                failures.push(Failure::new(
                    "A5",
                    format!("{family_name}: {actual} post-resume frames, expected <= {max_frames}"),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------
// protocol_violations: A4 / A11. Six independent process runs.
//
// This case does NOT go through `run_crawl`, and must not.
//
// Run 1 dispatches two `history.read` calls with `tokio::join!` -- on
// purpose: that run's hello declares `max_in_flight: 2`, and the whole
// point is that the host correlates replies by id rather than by arrival
// order. Its legs are therefore nondeterministically ordered, and every
// sequence `run_crawl` judges is an ordering claim. Feeding a concurrently
// dispatched run through the ledger would turn those claims into coin
// flips. So this case keeps its bespoke, order-free assertions, and the
// other five runs are here with it because they end in a killed
// connection rather than a completed crawl.
// ---------------------------------------------------------------------

async fn case_protocol_violations(
    argv: &[String],
    path: &Path,
    fixture: &Value,
    failures: &mut Vec<Failure>,
) {
    let expect = fixture.get("expect").cloned().unwrap_or(Value::Null);
    let deadline = deadline_of(fixture);

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
            0 => match spawn_run(argv, path, run_idx, deadline).await {
                Err(HostError::ProtocolViolation(kind)) => {
                    assert_violation_kind(kind, expected_kind.as_deref(), &label, failures);
                }
                Err(e) => failures.push(Failure::new(
                    "A11",
                    format!(
                        "{label}: expected spawn to fail with a ProtocolViolation, got a \
                         different error: {e}"
                    ),
                )),
                Ok(_) => failures.push(Failure::new(
                    "A11",
                    format!(
                        "{label}: expected spawn to fail with a ProtocolViolation, but it \
                         succeeded"
                    ),
                )),
            },
            1 => {
                check_survivable_then_kill(
                    argv,
                    path,
                    run_idx,
                    &label,
                    expected_kind.as_deref(),
                    deadline,
                    failures,
                )
                .await;
            }
            _ => {
                let expectation = RunExpectation {
                    label: &label,
                    kind: expected_kind.as_deref(),
                };
                check_violation_after_resources_list(
                    argv,
                    path,
                    run_idx,
                    &expectation,
                    deadline,
                    failures,
                    run_idx == 2,
                )
                .await;
            }
        }
    }
}

fn env_for(path: &Path, run: u64) -> Vec<(String, String)> {
    vec![
        (
            "SUMER_FIXTURE".to_owned(),
            path.to_string_lossy().into_owned(),
        ),
        ("SUMER_FIXTURE_RUN".to_owned(), run.to_string()),
    ]
}

/// The one spawn outside `run_crawl`: `protocol_violations` needs
/// connections that never complete a crawl (and, for run 0, one that never
/// completes a handshake). It is deliberately *not* recorded -- there is no
/// transcript to judge when the connection dies mid-hello.
async fn spawn_run(
    argv: &[String],
    path: &Path,
    run: u64,
    deadline: Duration,
) -> Result<AdapterHandle, HostError> {
    AdapterHandle::spawn_with_deadline(argv.to_vec(), env_for(path, run), deadline).await
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
            format!(
                "{label}: got ProtocolViolation::{actual_name}, but the fixture named no expected \
                 kind"
            ),
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
                assert_violation_kind(kind, expectation.kind, label, failures);
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
/// `statuses` exactly once (spec/observation.md section 6), so for a single-
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
///
/// **This is the run that cannot go through `run_crawl`.** Its two
/// `history.read` legs are dispatched with `tokio::join!`, so their replies
/// are nondeterministically ordered by design -- which is the whole point,
/// since the host must correlate by id and not by arrival. Every sequence
/// the ledger judges is an ordering claim, so this run is asserted here,
/// order-free, instead.
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
    // (spec/wire.md section 7), so dispatching both at once would risk res-b
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
            format!(
                "{label}: balances.read(res-a) expected Timeout (tombstoned/discarded), got \
                 {other:?}"
            ),
        )),
    }

    // history.read(res-a) is deferred; history.read(res-b) is answered
    // first and triggers the deferred res-a reply -- concurrently, so the
    // host must correlate by id, not arrival order. This run's hello
    // declares `max_in_flight: 2`, which is what makes concurrent dispatch
    // legal here, and what makes this run unrepresentable as an ordered
    // ledger.
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
    // the envelope has (spec/wire.md section 6: no `op`/`params` echo), so a
    // host that matched replies by arrival order would hand res-a's caller
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
        Err(HostError::ProtocolViolation(kind)) => {
            assert_violation_kind(kind, expected_kind, label, failures);
        }
        other => failures.push(Failure::new(
            "A11",
            format!(
                "{label}: expected the final status.read to trigger a ProtocolViolation, got \
                 {other:?}"
            ),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A9's regression: an adapter that mis-derives the `local_id` of an
    /// observation later superseded or tombstoned still lands on an
    /// identical final live set. Comparing the live set alone saw nothing.
    #[test]
    fn a9_compares_the_whole_history_not_just_the_final_state() {
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
        assert_eq!(
            content_diff(&second, &first).len(),
            2,
            "expected the short history and the stray id"
        );
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
}
