//! The nine-condition gate (`spec/observation.md` §8.1), one case per
//! disqualifier.
//!
//! Every case has the same shape, and the shape is the point:
//!
//! * **run 0** is a clean, qualifying sweep that plants two live records,
//!   `keep` and `drop`.
//! * **run 1** breaks exactly one condition and stops emitting `drop`,
//!   while emitting `keep` with a **changed amount**.
//!
//! Each case then asserts BOTH halves of "partial": zero retractions, and
//! the observations still persisted -- `keep`'s chain grew to two rows. A
//! test that only counted retractions would pass just as happily against a
//! host that threw the whole page away, which is the opposite of what
//! partial means.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use support::{
    chain_len, drained_status, fixture, live_ids, obs, only, page, refresh_run, retractions, store,
    Run, Scratch, ADAPTER_ID,
};

use sumer_store::sweep::SweepOptions;

/// run 0: two records, one clean drained page.
fn plant() -> Run {
    Run::new(vec![page(
        None,
        vec![obs("keep", "10.00"), obs("drop", "20.00")],
        drained_status(None),
    )])
}

/// The check every disqualified case makes.
async fn assert_partial(runs: Vec<Run>, expected_reason: &str) {
    if !support::python3_available() {
        eprintln!("python3 not found on PATH -- skipping");
        return;
    }
    let scratch = Scratch::new("gate");
    let fixture = fixture(&scratch, runs);
    let mut store = store(&scratch);

    let planted = only(
        refresh_run(&mut store, &fixture, 0, SweepOptions::default())
            .await
            .0,
    );
    assert!(planted.complete, "the priming sweep must qualify");
    assert_eq!(planted.new, 2);

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );

    assert!(
        !report.complete,
        "this sweep breaks a gate condition and must not be complete"
    );
    let reason = report
        .disqualified_reason
        .as_deref()
        .expect("a disqualified sweep records why");
    assert!(
        reason.contains(expected_reason),
        "expected the gate to fail on {expected_reason:?}, got {reason:?}"
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "a partial sweep retracts NOTHING"
    );
    assert!(
        live_ids(&store).contains(&"drop".to_owned()),
        "the record the partial sweep did not carry stays live"
    );
    assert_eq!(
        chain_len(&store, "keep"),
        2,
        "the partial sweep's observations still persisted -- they are evidence"
    );
}

// ---------------------------------------------------------------------
// The control: all nine hold
// ---------------------------------------------------------------------

/// **L1**: a qualifying sweep omitting a live `local_id` writes exactly one
/// retraction, and `history` stops listing it.
#[tokio::test]
async fn a_qualifying_sweep_retracts_exactly_what_it_did_not_carry() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("qualify");
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                drained_status(None),
            )]),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(report.complete, "{:?}", report.disqualified_reason);
    assert_eq!(report.retracted, 1);
    assert_eq!(
        retractions(&store),
        vec![("drop".to_owned(), "absent_from_complete_sweep".to_owned())],
        "a record that simply stopped being reported names the PROVIDER, not a software change"
    );
    assert_eq!(
        live_ids(&store),
        vec!["keep".to_owned()],
        "history stops listing a retracted record"
    );
    assert_eq!(chain_len(&store, "drop"), 1, "nothing was deleted");
}

/// **L2**: the next qualifying sweep re-emitting it revives it. No special
/// case anywhere -- revision 2 simply exceeds the retraction at revision 1.
#[tokio::test]
async fn a_later_sweep_reviving_a_record_needs_no_special_case() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("revive");
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                drained_status(None),
            )]),
            Run::new(vec![page(
                None,
                vec![obs("keep", "12.00"), obs("drop", "20.00")],
                drained_status(None),
            )]),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;
    assert!(!live_ids(&store).contains(&"drop".to_owned()));

    let report = only(
        refresh_run(&mut store, &fixture, 2, SweepOptions::default())
            .await
            .0,
    );
    assert!(report.complete);
    assert_eq!(report.retracted, 0);
    assert!(
        live_ids(&store).contains(&"drop".to_owned()),
        "a re-emitted record is live again: its head revision 2 exceeds the retraction at 1"
    );
    assert_eq!(
        retractions(&store).len(),
        1,
        "the retraction is not deleted -- the table is append-only and explains the gap"
    );
}

/// **L3**: two refreshes against an unchanged provider leave the live set
/// identical, append no revision, and retract nothing. This is what
/// `content_hash` buys; without it a 5000-record wallet grows by 5000 rows
/// on every refresh.
#[tokio::test]
async fn an_unchanged_provider_appends_nothing() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("idempotent");
    let fixture = fixture(&scratch, vec![plant(), plant()]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    let before = live_ids(&store);

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(report.complete);
    assert_eq!(report.new, 0);
    assert_eq!(report.revised, 0, "no revision appended for unchanged data");
    assert_eq!(report.unchanged, 2);
    assert_eq!(report.retracted, 0);
    assert_eq!(live_ids(&store), before);
    assert_eq!(chain_len(&store, "keep"), 1);
    assert_eq!(chain_len(&store, "drop"), 1);
}

// ---------------------------------------------------------------------
// One case per disqualifier
// ---------------------------------------------------------------------

/// (4) A page that did not report `fetched`.
#[tokio::test]
async fn a_page_that_did_not_report_fetched_disqualifies() {
    assert_partial(
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                json!({
                    "resource_id": support::RESOURCE_ID,
                    "outcome": {"rate_limited": {"retry_after_ms": 1000}},
                    "page": {"cursor_resumable": "exact", "next": null}
                }),
            )]),
        ],
        "rate_limited on page 1",
    )
    .await;
}

/// (5) An ANONYMOUS degrade: the host does not know which record went
/// missing, so it cannot tell "dropped for size" from "gone".
#[tokio::test]
async fn an_anonymous_degrade_disqualifies() {
    assert_partial(
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                json!({
                    "resource_id": support::RESOURCE_ID,
                    "degraded": [{"bytes": 99999}],
                    "page": {"cursor_resumable": "exact", "next": null}
                }),
            )]),
        ],
        "anonymous degraded record",
    )
    .await;
}

/// (3) A sweep that never drained: every page said `fetched`, but no page
/// ever answered `next: null`. A read that did not happen claims no resume
/// point, and a missing `page` object is not a drained sweep.
#[tokio::test]
async fn a_sweep_that_never_drained_disqualifies() {
    assert_partial(
        vec![
            plant(),
            Run::new(vec![
                page(
                    None,
                    vec![obs("keep", "11.00")],
                    drained_status(Some(json!({"kind": "cursor", "cursor": "p2"}))),
                ),
                page(
                    Some(json!({"kind": "cursor", "cursor": "p2"})),
                    vec![],
                    json!({
                        "resource_id": support::RESOURCE_ID,
                        "outcome": {"fetched": {"page_empty": true}}
                    }),
                ),
            ]),
        ],
        "did not drain",
    )
    .await;
}

/// (6) `batch_restart` is not `exact`: its durable point never moves, so a
/// resume resends the batch start and the host cannot say what it has and
/// has not seen.
#[tokio::test]
async fn a_batch_restart_cursor_disqualifies() {
    assert_partial(
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                json!({
                    "resource_id": support::RESOURCE_ID,
                    "page": {"cursor_resumable": "batch_restart", "next": null}
                }),
            )]),
        ],
        "not exact-resumable",
    )
    .await;
}

/// (2) The adapter process died mid-sweep. The crawl stays open, page 1's
/// observations stay, and nothing is retracted.
#[tokio::test]
async fn an_adapter_that_dies_mid_sweep_disqualifies() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("died");
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![
                page(
                    None,
                    vec![obs("keep", "11.00")],
                    drained_status(Some(json!({"kind": "cursor", "cursor": "p2"}))),
                ),
                json!({
                    "when": {"resources": [{"resource_id": support::RESOURCE_ID,
                                            "page": {"kind": "cursor", "cursor": "p2"}}]},
                    "do": [{"op": "exit", "code": 1}]
                }),
            ]),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(!report.complete);
    assert!(
        report.error.is_some(),
        "the failure is reported, not hidden"
    );
    assert_eq!(retractions(&store), Vec::<(String, String)>::new());
    assert!(live_ids(&store).contains(&"drop".to_owned()));
    assert_eq!(
        chain_len(&store, "keep"),
        2,
        "page 1 committed on its own -- a crash after it must not lose it"
    );
}

/// (5), the other half: a degrade **naming** a `local_id` must QUALIFY and
/// exempt only that id.
///
/// Treating a named degrade as a veto would be the worse bug: record size
/// is provider-influenced, so anyone who can push bytes into one record
/// could disable retraction for that resource permanently.
#[tokio::test]
async fn a_named_degrade_qualifies_and_exempts_only_that_id() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("named-degrade");
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(vec![page(
                None,
                vec![
                    obs("keep", "10.00"),
                    obs("drop", "20.00"),
                    obs("huge", "30.00"),
                ],
                drained_status(None),
            )]),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                json!({
                    "resource_id": support::RESOURCE_ID,
                    "degraded": [{"local_id": "huge", "bytes": 99999}],
                    "page": {"cursor_resumable": "exact", "next": null}
                }),
            )]),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(
        report.complete,
        "a NAMED degrade must not disqualify: {:?}",
        report.disqualified_reason
    );
    assert_eq!(
        retractions(&store),
        vec![("drop".to_owned(), "absent_from_complete_sweep".to_owned())],
        "only the genuinely absent record is retracted"
    );
    assert!(
        live_ids(&store).contains(&"huge".to_owned()),
        "the named-degraded record is EXEMPT, not retracted"
    );
}

/// (8) An observation whose `provenance.adapter_id` is not this
/// connection's.
///
/// `sumer_host::fold` keys chains by that field and this store keys rows by
/// the adapter it connected to. When they disagree the host is holding two
/// notions of one chain, and a host cannot conclude an absence from a set
/// it cannot key.
///
/// This is the one disqualifier that does NOT persist its whole page, and
/// the case is written out rather than folded into `assert_partial`
/// because that difference is the point: the honest record beside it still
/// lands, and the record that denies its own origin is refused -- there is
/// nowhere honest to put it. Storing it under this adapter's key would
/// record as fact a provenance the record itself denies; storing it under
/// the adapter it names would let one adapter append to another's history,
/// which is why ids are per-adapter at all (`spec/wire.md` §10).
#[tokio::test]
async fn an_observation_claiming_another_adapter_disqualifies_and_is_refused() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("foreign");
    let mut intruder = obs("intruder", "99.00");
    intruder["provenance"] = json!({"adapter_id": "some-other-adapter"});
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00"), intruder],
                drained_status(None),
            )]),
        ],
    );
    let mut store = store(&scratch);
    let planted = only(
        refresh_run(&mut store, &fixture, 0, SweepOptions::default())
            .await
            .0,
    );
    assert!(planted.complete);

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(!report.complete);
    assert!(
        report
            .disqualified_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("named another adapter")),
        "got {:?}",
        report.disqualified_reason
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "a sweep the host cannot key retracts NOTHING"
    );
    assert!(
        live_ids(&store).contains(&"drop".to_owned()),
        "and takes nothing away"
    );
    assert_eq!(
        chain_len(&store, "keep"),
        2,
        "the honest record on the same page still persisted"
    );
    assert_eq!(
        chain_len(&store, "intruder"),
        0,
        "the record denying its own origin was refused -- not filed under this \
         adapter's key, and certainly not under the one it named"
    );
}

/// **Condition (8) is about the CONNECTION'S id, not the stored one.**
///
/// Connect adapter `A`; later, the process behind `A`'s stored argv
/// announces HELLO `B` and hands back an observation whose provenance says
/// `A`. Comparing that provenance against the id we REMEMBERED accepts it,
/// counts `keep` as reported, and retracts `drop` -- on the testimony of a
/// connection that never claimed to be this adapter at all. Condition (8)
/// exists precisely so a host holding two notions of one chain cannot
/// conclude an absence; validating against the identity we remembered
/// rather than the one that just announced itself IS the confusion it was
/// written to catch.
///
/// The refusal is the whole refresh, not one observation at a time: a
/// connection whose HELLO disagrees with the record we opened it for is
/// not that adapter, and nothing it says can be attributed to a record it
/// does not claim.
#[tokio::test]
async fn a_connection_announcing_another_adapter_fails_the_whole_refresh() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("impostor");
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            // Announces `impostor`, and every observation on the page
            // carries the STORED id `fake-adapter` in its provenance.
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                drained_status(None),
            )])
            .announces("impostor"),
        ],
    );
    let mut store = store(&scratch);
    let planted = only(
        refresh_run(&mut store, &fixture, 0, SweepOptions::default())
            .await
            .0,
    );
    assert!(planted.complete);

    let failed = support::refresh_run_result(&mut store, &fixture, 1).await;
    let message = failed
        .expect_err("a connection that is not this adapter is refused")
        .to_string();
    assert!(
        message.contains("impostor") && message.contains(support::ADAPTER_ID),
        "the error names both identities the host was holding: {message}"
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "nothing a connection we cannot identify says may retract anything"
    );
    assert!(live_ids(&store).contains(&"drop".to_owned()));
    assert_eq!(
        chain_len(&store, "keep"),
        1,
        "and nothing it said was stored under the id it did not announce"
    );
}

/// **Condition (8) is judged before the resource filter, not after.**
///
/// §8.1 (8) is "EVERY observation's `provenance.adapter_id` was the
/// connection's own" -- every observation on the page, not merely the ones
/// addressed to the resource being swept. Filtering by `resource_id` first
/// let a page carry contrary evidence past the gate: the honest `keep`
/// completed the sweep, the foreign record under some other resource was
/// `continue`d before anyone looked at its provenance, and `drop` was
/// retracted by a host that had just been handed proof it was holding two
/// notions of one chain.
#[tokio::test]
async fn a_foreign_observation_under_another_resource_still_disqualifies() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("foreign-other-resource");
    let mut intruder = obs("intruder", "99.00");
    intruder["resource_id"] = json!("other");
    intruder["provenance"] = json!({"adapter_id": "some-other-adapter"});
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00"), intruder],
                drained_status(None),
            )]),
        ],
    );
    let mut store = store(&scratch);
    let planted = only(
        refresh_run(&mut store, &fixture, 0, SweepOptions::default())
            .await
            .0,
    );
    assert!(planted.complete);

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert_eq!(
        report.disqualified_reason.as_deref(),
        Some("an observation's provenance named another adapter"),
        "the resource an intruder addresses does not change what it is"
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "a page carrying contrary evidence is never judged complete"
    );
    assert!(live_ids(&store).contains(&"drop".to_owned()));
    assert_eq!(
        chain_len(&store, "intruder"),
        0,
        "and the intruder is still refused, not stored"
    );
}

// ---------------------------------------------------------------------
// (1) A cursor-started sweep -- which is also L5
// ---------------------------------------------------------------------

/// **L5** and gate condition (1) in one scenario, because they are the same
/// scenario.
///
/// Run 1 is interrupted after page 1, leaving a cursor on an open crawl.
/// Run 2 is `refresh --resume`: it MUST put that stored cursor on the wire
/// (asserted on the transcript, not on a log line), and even though it
/// drains cleanly it MUST retract nothing -- it never saw the history below
/// the cursor, so it cannot testify to an absence there.
#[tokio::test]
async fn a_resumed_sweep_carries_the_stored_cursor_and_retracts_nothing() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("resume");
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            // Interrupted: page 1 lands with a cursor, then the adapter dies.
            Run::new(vec![
                page(
                    None,
                    vec![obs("keep", "11.00")],
                    drained_status(Some(json!({"kind": "cursor", "cursor": "p2"}))),
                ),
                json!({
                    "when": {"resources": [{"resource_id": support::RESOURCE_ID,
                                            "page": {"kind": "cursor", "cursor": "p2"}}]},
                    "do": [{"op": "exit", "code": 1}]
                }),
            ]),
            // The resume: this rule fires ONLY for a request carrying the
            // stored cursor.
            Run::new(vec![page(
                Some(json!({"kind": "cursor", "cursor": "p2"})),
                vec![],
                drained_status(None),
            )]),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    let (reports, transcript) = refresh_run(
        &mut store,
        &fixture,
        2,
        SweepOptions {
            resume: true,
            confirm_empty: false,
        },
    )
    .await;

    let history_requests: Vec<String> = transcript
        .iter()
        .filter(|exchange| exchange.op == "history.read")
        .map(|exchange| exchange.params.to_string())
        .collect();
    assert_eq!(
        history_requests.len(),
        1,
        "the resumed sweep issues one page, from the cursor"
    );
    assert!(
        history_requests[0].contains(r#""cursor":"p2""#),
        "the resumed page must carry the STORED cursor on the wire; got {}",
        history_requests[0]
    );

    let report = only(reports);
    assert!(
        !report.complete,
        "a resumed sweep did not begin at the start of history and cannot retract"
    );
    assert_eq!(report.retracted, 0);
    assert_eq!(retractions(&store), Vec::<(String, String)>::new());
    assert!(live_ids(&store).contains(&"drop".to_owned()));
}

// ---------------------------------------------------------------------
// (9) A connection that broke the contract earlier in this refresh
// ---------------------------------------------------------------------

/// **A protocol violation taints the whole refresh, not just the read that
/// carried it.**
///
/// The other eight conditions are facts about the sweep's own pages, or
/// about the connection *while it was sweeping*. This one reaches back to a
/// read that is not the sweep's at all: a `balances.read` answering for a
/// resource nobody asked about is refused where it is decoded, the error is
/// reported -- and the refresh then walked into the sweeps, drained one,
/// called it complete and retracted a live record on its evidence. The
/// violation and the retraction were on the same connection, seconds apart.
///
/// An adapter that has just answered a question nobody asked has
/// demonstrated it is not answering the protocol, and absence is evidence
/// only when the host is confident it looked properly. The conservative
/// direction costs a stale row; the permissive one destroys a record.
#[tokio::test]
async fn a_contract_violation_earlier_in_the_refresh_disqualifies_the_sweep() {
    // A balance for a resource this refresh never listed and never asked
    // about: `invalid_request`, refused whole at the decode.
    let volunteered = json!({
        "resource_id": "ghost",
        "category": "available",
        "amount": {"asset": "usd", "amount": "999.00"},
        "provenance": {"adapter_id": ADAPTER_ID, "provider_id": "p1", "surface": "s",
                       "observed_at": "2026-01-01T00:00:00Z", "completeness": "complete"}
    });
    assert_partial(
        vec![
            plant(),
            // A flawless sweep -- drained, fetched, exact -- on a connection
            // that has already broken the contract.
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                drained_status(None),
            )])
            .balances(vec![volunteered]),
        ],
        "balances.read broke the wire contract",
    )
    .await;
}

/// The control for (9): the same drained sweep on a connection whose
/// `balances.read` failed **honestly** still retracts.
///
/// Condition (9) is about a broken contract, not about a failed read. An
/// adapter answering with an `err` envelope is using the vocabulary the
/// contract gives it, and is saying nothing about the history it goes on to
/// serve. Widening (9) to any error would let one rate-limited balances call
/// switch off retraction for the resource -- and every read fails sometimes.
#[tokio::test]
async fn an_honest_balances_failure_leaves_the_sweep_qualifying() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("honest-balance-failure");
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                drained_status(None),
            )])
            .balances_read_err(),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(
        report.complete,
        "an honest failure is not a violation: {:?}",
        report.disqualified_reason
    );
    assert_eq!(
        retractions(&store),
        vec![(
            "drop".to_owned(),
            sumer_store::sweep::REASON_ABSENT.to_owned()
        )],
        "and the record genuinely absent from a complete sweep still retracts"
    );
}

/// **And a violation inside one resource's sweep taints the next
/// resource's.**
///
/// The same rule, on the read that is easiest to overlook: an adapter with
/// two resources whose FIRST `history.read` breaks the contract, and whose
/// second is drained, fetched and exact. The second sweep is the one that
/// would have retracted, and the connection that serves it has already
/// proved it is not answering the protocol. Withholding the taint here --
/// covering only the `balances.read` case, which happens to be the one that
/// was reported -- is the enumerate-the-failure-paths mistake
/// `adr/0006` decision 8 exists to record.
///
/// The taint is forward-only. `other` is swept first and disqualifies
/// itself; `acct` is swept after and inherits it.
#[tokio::test]
async fn a_violation_in_one_sweep_disqualifies_the_next() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("tainted-sibling");
    let both = json!([{"resource_id": "other"}, {"resource_id": support::RESOURCE_ID}]);
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![
                // `other` is swept first: a reply whose `statuses` do not
                // cover the resource it was asked about -- `invalid_request`.
                json!({"when": {}, "do": [{"op": "reply_ok", "body": {
                    "observations": [], "statuses": []
                }}]}),
                // `acct` is swept second, and its own page is flawless.
                page(None, vec![obs("keep", "11.00")], drained_status(None)),
            ])
            .lists(&["other", support::RESOURCE_ID])
            .statuses(both.clone())
            .balance_statuses(both),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let reports = refresh_run(&mut store, &fixture, 1, SweepOptions::default())
        .await
        .0;
    assert_eq!(reports.len(), 2, "two resources, two sweeps");
    let acct = reports
        .iter()
        .find(|r| r.resource_id == support::RESOURCE_ID)
        .expect("the second resource was swept");
    assert!(
        !acct.complete,
        "a flawless sweep on an already-broken connection is still partial"
    );
    assert!(
        acct.disqualified_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("history.read broke the wire contract")),
        "and it says which violation: {:?}",
        acct.disqualified_reason
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "nothing is retracted on the evidence of that connection"
    );
    assert!(live_ids(&store).contains(&"drop".to_owned()));
    assert_eq!(
        chain_len(&store, "keep"),
        2,
        "the evidence it did read still persists"
    );
}

/// **A violation that arrives BEHIND a successful reply still disqualifies
/// the sweep it arrives during.**
///
/// The other (9) cases all reach the gate as an `Err` some call returned.
/// This one cannot: the adapter answers the final page perfectly --
/// drained, `fetched`, `exact` -- and then, in the same breath, sends a
/// duplicate reply. The host detects it and kills the connection, but a
/// delivered reply is delivered: it cannot be retracted out of the
/// caller's hands, and `core/host/tests/supervisor.rs`'s
/// `duplicate_reply_is_a_fatal_violation` pins that as correct transport
/// behaviour. So the sweep is holding `Ok(read)` and no error arm ever
/// runs.
///
/// The bug that shape found: condition (9) was judged against the value
/// the sweep was handed on the way IN, so it committed the retraction of
/// `drop` **after the host had already established that the connection
/// broke the protocol**. Detection, then commitment -- and the gate never
/// asked the second time.
///
/// # Why this is a schedule, not a race
///
/// `reply_ok_raw` writes `{"id":N,"ok":<body_json>}\n` to stdout
/// **verbatim, in one write**. Closing that envelope early inside
/// `body_json` and appending a second frame therefore puts BOTH lines in
/// that single write, so the host's next `read` returns both, the decoder
/// yields both, and the reader loop judges them in one synchronous pass:
/// it delivers the page, then hits the duplicate and publishes the
/// violation, all before the runtime can poll the task waiting on that
/// reply. Two separate writes would leave it to the scheduler which of the
/// two the host had seen by the time the sweep committed -- and a test
/// that asserts a fix on a coin flip is not a test.
///
/// The duplicate names id `0` -- the hello, the one id every connection
/// has already answered, and the only one whose value the fixture can know
/// without guessing at the host's counter.
#[tokio::test]
async fn a_violation_behind_the_final_reply_disqualifies_before_the_commit() {
    if !support::python3_available() {
        eprintln!("python3 not found on PATH -- skipping");
        return;
    }
    let scratch = Scratch::new("violation-behind-the-reply");

    // The final page, spelled out: `reply_ok_raw` goes on the wire
    // verbatim, so nothing here gets the fixture defaults filled in.
    let body = json!({
        "observations": [{
            "resource_id": support::RESOURCE_ID,
            "local_id": "keep",
            "state": "active",
            "surface": "s",
            "posting": "posted",
            "amount": {"asset": "usd", "amount": "11.00"},
            "raw_sign": "provider_positive",
            "description": "keep",
            "provenance": {
                "adapter_id": ADAPTER_ID,
                "provider_id": "p1",
                "surface": "s",
                "observed_at": "2026-01-01T00:00:00Z",
                "completeness": "complete"
            }
        }],
        "statuses": [{
            "resource_id": support::RESOURCE_ID,
            "outcome": {"fetched": {"page_empty": false}},
            "page": {"cursor_resumable": "exact", "next": null}
        }]
    });
    // `<body>}\n{"id":0,"ok":{}` -- the adapter supplies the leading
    // `{"id":N,"ok":` and the trailing `}\n`, which closes the second frame.
    let smuggled = format!("{body}}}\n{{\"id\":0,\"ok\":{{}}");

    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            Run::new(vec![json!({
                "when": {},
                "do": [{"op": "reply_ok_raw", "body_json": smuggled}]
            })]),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );

    // The reply LANDED and was ingested -- this is what makes the case the
    // one it claims to be. Without it the test would pass just as happily
    // against a run where the duplicate arrived first and the page was
    // never delivered at all, which is the ordinary error arm and proves
    // nothing.
    assert_eq!(
        report.error, None,
        "this sweep's own reads all SUCCEEDED: the violation never reached it as an error"
    );
    assert_eq!(
        chain_len(&store, "keep"),
        2,
        "the final page was delivered and ingested -- a partial sweep still persists its evidence"
    );

    assert!(
        !report.complete,
        "a sweep must not complete on a connection the host has already caught violating"
    );
    let reason = report
        .disqualified_reason
        .as_deref()
        .expect("a disqualified sweep records why");
    assert!(
        reason.contains("the connection broke the wire contract"),
        "and it says which violation: {reason:?}"
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "NOTHING is retracted on the evidence of that connection"
    );
    assert!(
        live_ids(&store).contains(&"drop".to_owned()),
        "the record the page omitted stays live"
    );
}

// ---------------------------------------------------------------------
// (9)'s companion: the violation NO sweep could ever have seen
// ---------------------------------------------------------------------

/// **A terminal violation must reach the refresh report.**
///
/// This is the one violation condition (9) structurally cannot act on: the
/// adapter answers every read cleanly and breaks the protocol on the way
/// out, so by the time it is knowable at all -- `close()` is what makes it
/// knowable, and closing is the last thing a refresh does to a connection
/// -- every sweep on that connection has already committed. Nothing is
/// left to disqualify, and reaching back to un-commit them is precisely
/// what `adr/0006` forbids.
///
/// So it is not a taint. It is a REPORT, and the thing being fixed here is
/// that `refresh` threw it away: `let _ = handle.close().await`. A refresh
/// that discards it finishes quietly and **exits 0** while the last thing
/// the connection did was break the wire contract. `RefreshReport::failed`
/// is the cron job's exit code, and this is the assertion that keeps it
/// honest.
///
/// The adapter here writes a frame with no terminating newline and exits:
/// nothing will ever terminate it, it is not the host's to discard, and it
/// is only detectable at end of stream -- after every reply this refresh
/// asked for had already been delivered and judged.
#[tokio::test]
async fn a_violation_on_the_way_out_reaches_the_refresh_report() {
    if !support::python3_available() {
        eprintln!("python3 not found on PATH -- skipping");
        return;
    }
    let scratch = Scratch::new("terminal-violation");
    let store = store(&scratch);
    // `refresh` spawns the adapter itself, from the argv on file, and
    // forwards no environment -- so this one cannot be the fixture-driven
    // fake adapter, which is configured entirely through `SUMER_FIXTURE`.
    sumer_store::store::upsert_adapter(
        store.conn(),
        ADAPTER_ID,
        &[
            "python3".to_owned(),
            "-c".to_owned(),
            HONEST_UNTIL_THE_END.to_owned(),
        ],
    )
    .unwrap();
    let mut store = store;

    let report = sumer_store::refresh::refresh(&mut store, &Default::default())
        .await
        .expect("the refresh itself completes -- every read was answered");

    assert_eq!(
        report.sweeps.len(),
        1,
        "the adapter answered everything it was asked: {:?}",
        report.sweeps
    );
    assert_eq!(
        report
            .adapter_errors
            .iter()
            .map(|(_, message)| message.as_str())
            .collect::<Vec<_>>(),
        vec!["the connection ended in a wire-contract violation: UnterminatedFrame"],
        "how the connection ENDED is part of the report"
    );
    assert!(
        report.failed(),
        "and a refresh whose adapter broke the protocol on the way out does not exit 0"
    );
}

/// An adapter that answers every read of one refresh correctly and then,
/// on its way out, writes a frame it never terminates.
const HONEST_UNTIL_THE_END: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n")
    sys.stdout.flush()
fetched = {"fetched": {"page_empty": False}}
while True:
    line = sys.stdin.readline()
    if not line:
        break
    req = json.loads(line)
    i, op = req["id"], req["op"]
    if op == "hello":
        send({"id": i, "ok": {"protocol": "1", "adapter_id": "fake-adapter",
              "adapter_version": "0.1.0", "local_id_derivation": "fixture-literal@1",
              "capabilities": ["resources.list", "balances.read", "history.read", "status.read"],
              "max_in_flight": 1}})
    elif op == "resources.list":
        send({"id": i, "ok": {"resources": [{"resource_id": "acct", "provider_id": "p1",
              "kind": "bank_checking", "label": "Checking"}]}})
    elif op == "status.read":
        send({"id": i, "ok": {"statuses": [{"resource_id": "acct", "outcome": fetched}]}})
    elif op == "balances.read":
        send({"id": i, "ok": {"observations": [], "statuses": [
              {"resource_id": "acct", "outcome": {"fetched": {"page_empty": True}}}]}})
    elif op == "history.read":
        send({"id": i, "ok": {"observations": [{"resource_id": "acct", "local_id": "keep",
              "state": "active", "surface": "s", "posting": "posted", "raw_sign": "provider_positive",
              "amount": {"asset": "usd", "amount": "10.00"}, "description": "keep",
              "provenance": {"adapter_id": "fake-adapter", "provider_id": "p1", "surface": "s",
                             "observed_at": "2026-01-01T00:00:00Z", "completeness": "complete"}}],
              "statuses": [{"resource_id": "acct", "outcome": fetched,
                            "page": {"cursor_resumable": "exact", "next": None}}]}})
        # No newline: a frame nothing will ever terminate.
        sys.stdout.write('{"id":99,"ok":')
        sys.stdout.flush()
        sys.exit(0)
"#;
