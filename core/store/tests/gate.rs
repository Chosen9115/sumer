//! The eight-condition gate (`spec/observation.md` §8.1), one case per
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
    Run, Scratch,
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
// The control: all eight hold
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
