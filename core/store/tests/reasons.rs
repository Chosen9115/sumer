//! Why a record was retracted, and the three exemptions that stop one
//! being retracted at all.
//!
//! The reason is chosen by WHY the record is absent, in this order:
//!
//! | the head's ...      | differs from the sweep's | reason |
//! |---------------------|--------------------------|--------|
//! | `derivation`        | yes                      | `derivation_changed` |
//! | `fingerprint`       | yes                      | `resource_definition_changed` |
//! | neither             | --                       | `absent_from_complete_sweep` |
//!
//! A derivation bump and an address change are MIGRATIONS, recorded in the
//! sweep's own transaction. Old ids can never be re-emitted, so they retire
//! under a reason naming the SOFTWARE change; records that still exist come
//! back on that same sweep with a new revision and are never retracted at
//! all. A software change must never retire records under a reason blaming
//! the provider.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::json;
use support::{
    discrepancy_kinds, drained_status, fixture, live_ids, obs, obs_at, only, page, refresh_run,
    retractions, store, Run, Scratch,
};

use sumer_store::sweep::SweepOptions;

/// **L7.** A sweep run AFTER a `local_id_derivation` change must retract
/// every old-derivation live head under `derivation_changed`, must NOT use
/// `absent_from_complete_sweep`, and must leave no pre-upgrade record live
/// beside its new-id twin.
///
/// This is the test that kills BOTH catastrophic readings of gate condition
/// (7). If (7) compared the sweep's hello derivation against the stored
/// observations', or against the `adapter` row, this sweep would be
/// DISQUALIFIED -- zero retractions -- and `old:a` would sit live beside
/// `new:a` for ever, which is what the last assertion here refuses.
#[tokio::test]
async fn a_derivation_change_retires_old_ids_under_the_software_reason() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("derivation");
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(vec![page(
                None,
                vec![obs("old:a", "10.00"), obs("old:b", "20.00")],
                drained_status(None),
            )])
            .derivation("txid@1"),
            Run::new(vec![page(
                None,
                vec![obs("new:a", "10.00"), obs("new:b", "20.00")],
                drained_status(None),
            )])
            .derivation("txid@2"),
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
        "condition (7) gates on the PER-SWEEP HELLO VALUE ONLY -- a derivation change is a \
         REASON, never a disqualifier. Got: {:?}",
        report.disqualified_reason
    );
    assert_eq!(report.retracted, 2);

    let reasons = retractions(&store);
    assert_eq!(
        reasons,
        vec![
            ("old:a".to_owned(), "derivation_changed".to_owned()),
            ("old:b".to_owned(), "derivation_changed".to_owned()),
        ],
        "a software change must never retire records under a reason blaming the provider"
    );
    assert!(
        !reasons
            .iter()
            .any(|(_, reason)| reason == "absent_from_complete_sweep"),
        "absent_from_complete_sweep is the WRONG reason here and must not appear"
    );

    let mut live = live_ids(&store);
    live.sort();
    assert_eq!(
        live,
        vec!["new:a".to_owned(), "new:b".to_owned()],
        "no pre-upgrade record is left live beside its new-id twin"
    );
    assert!(
        discrepancy_kinds(&store).contains(&"derivation_changed".to_owned()),
        "the migration is recorded once per sweep, for `status` to show"
    );
}

/// **L8**, in the general shape the Bitcoin end-to-end test drives for
/// real: a sweep after the resource DEFINITION changed retracts its absent
/// heads under `resource_definition_changed`, and every record still
/// present stays live.
#[tokio::test]
async fn a_definition_change_retires_absent_heads_and_keeps_the_present_ones() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("definition");
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(vec![page(
                None,
                vec![obs("shared", "10.00"), obs("only-second-address", "20.00")],
                drained_status(None),
            )])
            .resource_extra(json!({"address_set_sha256": "aaaa"})),
            Run::new(vec![page(
                None,
                vec![obs("shared", "10.00")],
                drained_status(None),
            )])
            .resource_extra(json!({"address_set_sha256": "bbbb"})),
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
    assert_eq!(
        retractions(&store),
        vec![(
            "only-second-address".to_owned(),
            "resource_definition_changed".to_owned()
        )]
    );
    assert_eq!(
        live_ids(&store),
        vec!["shared".to_owned()],
        "a record still present is re-activated by the same sweep and never retracted"
    );
}

// ---------------------------------------------------------------------
// The three exemptions. None is a percentage threshold.
// ---------------------------------------------------------------------

/// A live head older than the adapter's own reported `history_start` was
/// never in this sweep's reach. A pruned or re-pointed Esplora is otherwise
/// the wrong retraction that never self-corrects.
#[tokio::test]
async fn history_start_exempts_records_the_sweep_could_not_have_seen() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("history-start");
    let plant = || {
        Run::new(vec![page(
            None,
            vec![
                obs_at("ancient", "10.00", "2026-01-01T00:00:00Z"),
                obs_at("recent", "20.00", "2026-06-01T00:00:00Z"),
            ],
            drained_status(None),
        )])
    };
    let fixture = fixture(
        &scratch,
        vec![
            plant(),
            // The provider's history now begins in March: `ancient`
            // predates it, `recent` does not.
            Run::new(vec![page(
                None,
                vec![obs_at("recent", "20.00", "2026-06-01T00:00:00Z")],
                drained_status(None),
            )])
            .statuses(json!([{
                "resource_id": support::RESOURCE_ID,
                "outcome": {"fetched": {"page_empty": true}},
                "history_start": "2026-03-01T00:00:00Z"
            }])),
            // The control: the SAME sweep with no history_start does
            // retract it, so the exemption above is the thing doing the
            // work and not some accident of the fixture.
            Run::new(vec![page(
                None,
                vec![obs_at("recent", "20.00", "2026-06-01T00:00:00Z")],
                drained_status(None),
            )]),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let exempted = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(exempted.complete, "{:?}", exempted.disqualified_reason);
    assert_eq!(
        exempted.retracted, 0,
        "a record older than history_start is EXEMPT, not absent"
    );
    assert!(live_ids(&store).contains(&"ancient".to_owned()));
    assert!(discrepancy_kinds(&store).contains(&"history_start_exempt".to_owned()));

    let control = only(
        refresh_run(&mut store, &fixture, 2, SweepOptions::default())
            .await
            .0,
    );
    assert_eq!(
        control.retracted, 1,
        "without history_start the very same absence IS a retraction"
    );
    assert_eq!(
        retractions(&store),
        vec![(
            "ancient".to_owned(),
            "absent_from_complete_sweep".to_owned()
        )]
    );
}

/// **A resource the `status.read` reply left out is not a resource that
/// reported no bound.**
///
/// §6: every requested `resource_id` appears in `statuses` EXACTLY ONCE --
/// never zero times. A reply that omits one is malformed, and the host used
/// to read the omission as "this resource declared no `history_start`",
/// which is the widest possible answer: the whole of history is in reach
/// and every absence is evidence. So a malformed reply SILENTLY REMOVED the
/// bound, and the record the previous test exempts got retracted instead.
///
/// The asymmetry decides the direction. A missing bound must never widen
/// what an absence may be evidence of, so a reply that fails to cover a
/// requested resource is refused outright rather than reconciled against.
#[tokio::test]
async fn a_status_reply_that_omits_a_requested_resource_grants_no_bound() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("status-omitted");
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(vec![page(
                None,
                vec![
                    obs_at("ancient", "10.00", "2026-01-01T00:00:00Z"),
                    obs_at("recent", "20.00", "2026-06-01T00:00:00Z"),
                ],
                drained_status(None),
            )]),
            // The same sweep as the exemption case above -- `ancient` is
            // gone from the page -- except that `status.read` covers
            // nothing at all, so the host never learns where this
            // resource's history begins.
            Run::new(vec![page(
                None,
                vec![obs_at("recent", "20.00", "2026-06-01T00:00:00Z")],
                drained_status(None),
            )])
            .statuses(json!([])),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let refused = support::refresh_run_result(&mut store, &fixture, 1).await;
    let message = refused
        .expect_err("a reply that covers no requested resource is malformed")
        .to_string();
    assert!(
        message.contains(support::RESOURCE_ID) && message.contains("status.read"),
        "the error names the op and the resource it failed to cover: {message}"
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "a reply that never said where history begins licenses no absence"
    );
    assert!(live_ids(&store).contains(&"ancient".to_owned()));
}

/// A sweep from a different vantage retracts NOTHING, records the change,
/// and adopts the new vantage -- and the sweep after it retracts normally.
/// The rule terminates; it does not disable retraction for ever.
#[tokio::test]
async fn a_vantage_change_defers_one_sweep_and_then_terminates() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("vantage");
    let dropped = || {
        Run::new(vec![page(
            None,
            vec![obs("keep", "10.00")],
            drained_status(None),
        )])
    };
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(vec![page(
                None,
                vec![obs("keep", "10.00"), obs("drop", "20.00")],
                drained_status(None),
            )])
            .provider_id("esplora-a"),
            dropped().provider_id("esplora-b"),
            dropped().provider_id("esplora-b"),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let moved = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(moved.complete, "{:?}", moved.disqualified_reason);
    assert_eq!(
        moved.retracted, 0,
        "a different vantage is a different view, not evidence that anything is gone"
    );
    assert!(discrepancy_kinds(&store).contains(&"vantage_changed".to_owned()));
    assert!(live_ids(&store).contains(&"drop".to_owned()));

    let settled = only(
        refresh_run(&mut store, &fixture, 2, SweepOptions::default())
            .await
            .0,
    );
    assert_eq!(
        settled.retracted, 1,
        "THE RULE TERMINATES: the next sweep from that vantage retracts normally"
    );
    assert_eq!(
        retractions(&store),
        vec![("drop".to_owned(), "absent_from_complete_sweep".to_owned())]
    );
}

/// A qualifying sweep with ZERO observations against a non-empty live set
/// retracts nothing and says so. `--confirm-empty` is the only way past it.
/// 100% is the one constant that needs no justification; there is no
/// percentage threshold anywhere in this code, because 99% is not a number
/// anyone can defend.
#[tokio::test]
async fn an_empty_sweep_retracts_nothing_without_confirm_empty() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("empty");
    let empty = || {
        Run::new(vec![page(
            None,
            vec![],
            json!({
                "resource_id": support::RESOURCE_ID,
                "outcome": {"fetched": {"page_empty": true}},
                "page": {"cursor_resumable": "exact", "next": null}
            }),
        )])
    };
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(vec![page(
                None,
                vec![obs("a", "10.00"), obs("b", "20.00")],
                drained_status(None),
            )]),
            empty(),
            empty(),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let refused = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(refused.complete, "the sweep itself qualified");
    assert_eq!(refused.retracted, 0);
    assert!(discrepancy_kinds(&store).contains(&"empty_sweep".to_owned()));
    assert!(
        refused
            .discrepancies
            .iter()
            .any(|d| d.detail.contains("--confirm-empty")),
        "refresh must PRINT the refusal and name the way past it"
    );
    assert_eq!(live_ids(&store).len(), 2);

    let confirmed = only(
        refresh_run(
            &mut store,
            &fixture,
            2,
            SweepOptions {
                resume: false,
                confirm_empty: true,
            },
        )
        .await
        .0,
    );
    assert_eq!(
        confirmed.retracted, 2,
        "--confirm-empty is the ONLY way to retract 100% of a resource"
    );
    assert!(live_ids(&store).is_empty());
}
