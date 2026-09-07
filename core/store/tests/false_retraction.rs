//! Four ways a sweep concluded a live financial record was gone on
//! evidence that did not say so, and two ways the bookkeeping under them
//! was wrong.
//!
//! Every case here failed before the fix that carries it, and each one
//! failed by **retracting** (or, for the last two, by writing the wrong row
//! and re-sending a dead cursor), not by asserting something cosmetic. A
//! green assertion against a broken implementation is worth nothing, so
//! each test is written to be watched go red first.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::{json, Value};
use support::{
    chain_len, drained_status, fixture, live_ids, obs, obs_at, only, page, refresh_run,
    retractions, store, Run, Scratch, ADAPTER_ID, DERIVATION, RESOURCE_ID,
};

use sumer_store::sweep::{self, SweepOptions};

/// A `history.read` rule with the full reply body written out, for the
/// cases whose whole point is a `statuses` array [`support::page`] cannot
/// express (two entries for one resource) or a body the adapter must be
/// told **not** to degrade for itself.
fn raw_page(when: Value, body: Value) -> Value {
    json!({"when": when, "do": [{"op": "reply_ok", "body": body}]})
}

/// One observation big enough that the cap has to act on it. `pad` inflates
/// `description` on the way out, so the size is real bytes on the wire
/// rather than a number a fixture asserted into existence.
fn pad(index: usize) -> Value {
    json!({"path": ["observations", index, "description"], "bytes": 70_000})
}

/// run 0 of most cases: `keep` and `drop` live, plus whatever else.
fn plant(extra: Vec<Value>) -> Run {
    let mut observations = vec![obs("keep", "10.00"), obs("drop", "20.00")];
    observations.extend(extra);
    Run::new(vec![page(None, observations, drained_status(None))])
}

// ---------------------------------------------------------------------
// (1) One dropped record must not erase another's evidence
// ---------------------------------------------------------------------

/// **Two oversized records on one page are two exemptions, not one.**
///
/// `spec/observation.md` §8.1: a `degraded` naming a `local_id` exempts
/// that id. The host enforces the cap itself (§6, "the host enforces the
/// cap too"), once per dropped record -- and while `degraded` was a single
/// slot, the second drop overwrote the first. The first record was then
/// absent from a sweep the gate still called complete, which is the
/// definition of a retraction.
#[tokio::test]
async fn two_oversized_records_are_both_exempt_not_just_the_last() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("two-oversized");
    let fixture = fixture(
        &scratch,
        vec![
            plant(vec![obs("huge-a", "30.00"), obs("huge-b", "40.00")]),
            // The adapter does NOT degrade (no `degrade` action), so both
            // oversized records arrive whole and the HOST's own cap
            // enforcement is what drops them.
            Run::new(vec![json!({
                "when": {},
                "do": [{"op": "reply_ok", "pad": [pad(1), pad(2)], "body": {
                    "observations": [
                        obs("keep", "11.00"),
                        obs("huge-a", "30.00"),
                        obs("huge-b", "40.00"),
                    ],
                    "statuses": [drained_status(None)]
                }}]
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
    assert!(
        report.complete,
        "two NAMED degrades still qualify: {:?}",
        report.disqualified_reason
    );
    assert_eq!(
        retractions(&store),
        vec![("drop".to_owned(), "absent_from_complete_sweep".to_owned())],
        "only the genuinely absent record is retracted -- both oversized \
         records were named, and a named degrade exempts"
    );
    let live = live_ids(&store);
    assert!(live.contains(&"huge-a".to_owned()), "live: {live:?}");
    assert!(live.contains(&"huge-b".to_owned()), "live: {live:?}");
}

/// **A host-authored degrade must never weaken an adapter-authored one.**
///
/// The adapter reported an ANONYMOUS degrade -- it dropped something and
/// cannot say what -- which disqualifies the sweep (§8.1 condition 5). The
/// host then dropped an oversized record of its own and named it. Writing
/// that into a single slot converted a disqualifier into an exemption and
/// let the sweep retract.
#[tokio::test]
async fn a_host_degrade_cannot_overwrite_the_adapters_anonymous_one() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("degrade-overwrite");
    let fixture = fixture(
        &scratch,
        vec![
            plant(vec![obs("huge", "30.00")]),
            Run::new(vec![json!({
                "when": {},
                "do": [{"op": "reply_ok", "pad": [pad(1)], "body": {
                    "observations": [obs("keep", "11.00"), obs("huge", "30.00")],
                    "statuses": [{
                        "resource_id": RESOURCE_ID,
                        // The adapter's own claim: something was dropped and
                        // it cannot say what.
                        "degraded": [{"bytes": 99_999}],
                        "page": {"cursor_resumable": "exact", "next": null}
                    }]
                }}]
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
    assert!(
        !report.complete,
        "an anonymous degrade disqualifies, and nothing the host writes beside \
         it may erase that"
    );
    assert_eq!(
        report.disqualified_reason.as_deref(),
        Some("an anonymous degraded record")
    );
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "a partial sweep retracts NOTHING"
    );
    assert!(live_ids(&store).contains(&"drop".to_owned()));
}

/// **Condition (8) is not skippable by being large.**
///
/// An observation whose `provenance.adapter_id` names another adapter is
/// refused and disqualifies the sweep, and §8.1 is explicit that it "can
/// never suppress a retraction". Measuring the cap first inverted that: the
/// host dropped the record for size and filed it as a NAMED degrade, so its
/// `local_id` became an exemption, condition (8) never fired, and the sweep
/// retracted everything else it did not carry.
#[tokio::test]
async fn an_oversized_foreign_observation_still_disqualifies() {
    if !support::python3_available() {
        return;
    }
    let mut foreign = obs("foreign", "30.00");
    foreign["provenance"] = json!({"adapter_id": "some-other-adapter"});

    let scratch = Scratch::new("oversized-foreign");
    let fixture = fixture(
        &scratch,
        vec![
            plant(vec![]),
            Run::new(vec![json!({
                "when": {},
                "do": [{"op": "reply_ok", "pad": [pad(1)], "body": {
                    "observations": [obs("keep", "11.00"), foreign],
                    "statuses": [drained_status(None)]
                }}]
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
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "a refused observation can never suppress a retraction, and it certainly \
         cannot license one"
    );
    assert_eq!(
        report.disqualified_reason.as_deref(),
        Some("an observation's provenance named another adapter"),
        "the size of a record that denies its own origin does not change what it is"
    );
    assert!(live_ids(&store).contains(&"drop".to_owned()));
    assert_eq!(
        chain_len(&store, "foreign"),
        0,
        "and it is still refused, not stored"
    );
}

// ---------------------------------------------------------------------
// (2) history_start is an instant, not a string
// ---------------------------------------------------------------------

/// **Both timestamp spellings `sumer_wire` accepts break text comparison.**
///
/// `history_start` bounds what an absence is allowed to be evidence of
/// (§8.2). Compared as text, a record inside the provider's window lands
/// outside it:
///
/// * `"...T00:00:00Z"` sorts AFTER `"...T00:00:00.500Z"` (`Z` > `.`) while
///   the instant is half a second EARLIER;
/// * `"2026-01-01T01:00:00+02:00"` sorts after `"2026-01-01T00:00:00Z"`
///   while naming an instant an hour before it.
///
/// Either way the record is judged to be inside the sweep's reach, is
/// absent from it, and is retracted.
#[tokio::test]
async fn history_start_exempts_by_instant_not_by_text() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("history-start");
    let statuses = json!([{
        "resource_id": RESOURCE_ID,
        "history_start": "2026-01-01T00:00:00.500Z"
    }]);
    let scratch_fixture = fixture(
        &scratch,
        vec![
            Run::new(vec![page(
                None,
                vec![
                    obs("keep", "10.00"),
                    // Half a second before history_start.
                    obs_at("fraction", "20.00", "2026-01-01T00:00:00Z"),
                    // An hour before it, written in +02:00.
                    obs_at("offset", "30.00", "2026-01-01T01:00:00+02:00"),
                    // Genuinely inside the window, and genuinely absent
                    // from the next sweep.
                    obs_at("gone", "40.00", "2026-06-01T00:00:00Z"),
                ],
                drained_status(None),
            )])
            .statuses(statuses.clone()),
            Run::new(vec![page(
                None,
                vec![obs("keep", "11.00")],
                drained_status(None),
            )])
            .statuses(statuses),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &scratch_fixture, 0, SweepOptions::default()).await;

    let report = only(
        refresh_run(&mut store, &scratch_fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert!(report.complete, "{:?}", report.disqualified_reason);
    assert_eq!(
        retractions(&store),
        vec![("gone".to_owned(), "absent_from_complete_sweep".to_owned())],
        "only the record inside the provider's window is retracted"
    );
    let live = live_ids(&store);
    assert!(
        live.contains(&"fraction".to_owned()),
        "a fractional second is not a later instant: {live:?}"
    );
    assert!(
        live.contains(&"offset".to_owned()),
        "+02:00 is not a later instant: {live:?}"
    );
    assert!(support::discrepancy_kinds(&store).contains(&"history_start_exempt".to_owned()));
}

// ---------------------------------------------------------------------
// (3) Contradictory evidence is never complete evidence
// ---------------------------------------------------------------------

/// **A resource named twice in one `statuses` array is a malformed reply.**
///
/// Every reader takes the first entry that matches, so an adapter that
/// answers cleanly and then, in the same array, `stale` with an anonymous
/// `degraded` was judged on the clean half: the sweep read a complete,
/// undegraded page and retracted what it did not carry. The reply is now
/// refused where it is decoded, so no reader has to defend against it.
#[tokio::test]
async fn a_resource_named_twice_in_one_reply_is_refused() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("duplicate-status");
    let fixture = fixture(
        &scratch,
        vec![
            plant(vec![]),
            Run::new(vec![raw_page(
                json!({}),
                json!({
                    "observations": [obs("keep", "11.00")],
                    "statuses": [
                        // A clean, drained, exact page ...
                        {
                            "resource_id": RESOURCE_ID,
                            "outcome": {"fetched": {"page_empty": false}},
                            "page": {"cursor_resumable": "exact", "next": null}
                        },
                        // ... contradicted by the entry beside it.
                        {
                            "resource_id": RESOURCE_ID,
                            "outcome": {"stale": {"as_of": "2026-01-01T00:00:00Z"}},
                            "degraded": [{"bytes": 4096}]
                        }
                    ]
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
    assert_eq!(
        retractions(&store),
        Vec::<(String, String)>::new(),
        "contradictory evidence must never be judged complete"
    );
    assert!(!report.complete);
    let error = report.error.expect("a malformed reply is a failed read");
    assert!(
        error.contains("more than once"),
        "the reason names what was wrong with the reply, got {error:?}"
    );
    assert!(live_ids(&store).contains(&"drop".to_owned()));
}

// ---------------------------------------------------------------------
// (4) Revisions are keyed per adapter, not per resource
// ---------------------------------------------------------------------

/// **A retracted record reported under a second resource must revive.**
///
/// §3 keys revisions by `(adapter_id, local_id)` and the schema enforces
/// `UNIQUE (adapter_id, local_id, revision)`. A fold loaded per
/// `(adapter_id, resource_id)` starts the counter at 1 again when a record
/// moves resource, the insert hits the constraint, and §8.4's revival --
/// "a later sweep appends revision N+1, which exceeds N" -- becomes
/// unreachable for that record for ever, since every later sweep repeats
/// the same collision.
#[tokio::test]
async fn a_record_retracted_under_one_resource_revives_under_another() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("cross-resource-revision");

    fn observation(local_id: &str, resource_id: &str) -> Value {
        json!({
            "resource_id": resource_id,
            "local_id": local_id,
            "state": "active",
            "surface": "s",
            "posting": "posted",
            "amount": {"asset": "usd", "amount": "10.00"},
            "raw_sign": "provider_positive",
            "description": local_id
        })
    }
    fn reply(resource_id: &str, observations: Vec<Value>) -> Run {
        Run::new(vec![raw_page(
            json!({}),
            json!({
                "observations": observations,
                "statuses": [{
                    "resource_id": resource_id,
                    "page": {"cursor_resumable": "exact", "next": null}
                }]
            }),
        )])
    }

    let fixture = fixture(
        &scratch,
        vec![
            reply(
                "r1",
                vec![observation("tx", "r1"), observation("anchor", "r1")],
            ),
            // `tx` gone from r1 -- `anchor` keeps this from being an empty
            // sweep, which exempts on its own.
            reply("r1", vec![observation("anchor", "r1")]),
            // The provider now reports the same record under r2.
            reply("r2", vec![observation("tx", "r2")]),
        ],
    );
    let mut store = store(&scratch);

    async fn sweep(
        store: &mut sumer_store::Store,
        fixture: &std::path::Path,
        run: usize,
        resource_id: &str,
    ) -> sweep::SweepReport {
        let handle = support::connect(fixture, run).await;
        let input = sweep::SweepInput {
            adapter_id: ADAPTER_ID,
            resource_id,
            hello_derivation: DERIVATION,
            fingerprint: "fp1",
            provider_id: "p1",
            history_start: None,
            options: SweepOptions::default(),
        };
        let report = sweep::sweep_resource(store, &handle, &input)
            .await
            .expect("the sweep runs");
        let _ = handle.close().await;
        report
    }

    assert!(sweep(&mut store, &fixture, 0, "r1").await.complete);
    let retracting = sweep(&mut store, &fixture, 1, "r1").await;
    assert_eq!(retracting.retracted, 1, "`tx` left r1 and was retracted");

    let revived = sweep(&mut store, &fixture, 2, "r2").await;
    assert!(revived.complete, "{:?}", revived.disqualified_reason);
    assert_eq!(
        revived.revised, 1,
        "the re-report is revision 2 of the SAME chain, not a second revision 1"
    );
    let live: Vec<String> = sweep::live_records(&store, ADAPTER_ID, "r2")
        .unwrap()
        .into_iter()
        .map(|(local_id, _, _)| local_id)
        .collect();
    assert_eq!(
        live,
        vec!["tx".to_owned()],
        "a revision that exceeds the retraction is what revival IS (§8.4)"
    );
}

// ---------------------------------------------------------------------
// (5) The row a re-observation stamps is the fold's head
// ---------------------------------------------------------------------

/// **"Last inserted row" is not "the fold head".**
///
/// Two observations for one key on one page share a `received_at`, so the
/// fold orders them by `surface` bytewise: `z` arrives first and takes
/// revision 1, `a` arrives second and takes revision 2, and the head is
/// revision 1. Stamping the highest `observation_id` updated revision 2 --
/// so the real head kept the fingerprint of a resource definition that has
/// since moved, which is what §8.3 row 2 reads when the record eventually
/// disappears.
#[tokio::test]
async fn a_re_observation_stamps_the_head_the_fold_chose() {
    if !support::python3_available() {
        return;
    }
    fn surfaced(local_id: &str, surface: &str, amount: &str) -> Value {
        let mut observation = obs(local_id, amount);
        observation["surface"] = json!(surface);
        observation["provenance"] = json!({"surface": surface});
        observation
    }

    let scratch = Scratch::new("stamp-head");
    let head = surfaced("tx", "z", "10.00");
    let fixture = fixture(
        &scratch,
        vec![
            // One page, one key, two surfaces: `z` (revision 1, and the
            // fold's head) then `a` (revision 2, the last row inserted).
            Run::new(vec![page(
                None,
                vec![head.clone(), surfaced("tx", "a", "20.00")],
                drained_status(None),
            )]),
            // The same head re-observed, under a MOVED resource definition.
            Run::new(vec![page(None, vec![head], drained_status(None))])
                .resource_extra(json!({"address_set_sha256": "moved"})),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    let report = only(
        refresh_run(&mut store, &fixture, 1, SweepOptions::default())
            .await
            .0,
    );
    assert_eq!(report.unchanged, 1, "the head was recognised and deduped");

    let current = sumer_store::store::resource(store.conn(), ADAPTER_ID, RESOURCE_ID)
        .unwrap()
        .expect("the resource row")
        .fingerprint
        .expect("the sweep recorded its fingerprint");
    let fingerprint = |revision: i64| -> Option<String> {
        store
            .conn()
            .query_row(
                "SELECT fingerprint FROM observation WHERE local_id = 'tx' AND revision = ?1",
                [revision],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert_eq!(
        fingerprint(1).as_deref(),
        Some(current.as_str()),
        "the FOLD HEAD (surface `z`, revision 1) is the row this sweep re-stamped"
    );
    assert_ne!(
        fingerprint(2).as_deref(),
        Some(current.as_str()),
        "and the superseded revision keeps the definition it was read under"
    );
}

// ---------------------------------------------------------------------
// (6) A drained resume retires the cursor it resumed from
// ---------------------------------------------------------------------

/// **A successful resume must not leave the old cursor eligible for ever.**
///
/// Crawl 1 commits cursor `p2` and dies. `--resume` opens crawl 2 from
/// `p2` and drains it. Crawl 2 is finished, but crawl 1 never is -- so
/// while the resume point was selected as "the newest crawl that never
/// drained and still holds a cursor", the next `--resume` picked crawl 1's
/// dead `p2` again, and the one after that, for ever.
#[tokio::test]
async fn a_drained_resume_retires_the_cursor_it_resumed_from() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("stale-cursor");
    let cursor = json!({"kind": "cursor", "cursor": "p2"});
    let fixture = fixture(
        &scratch,
        vec![
            // Run 0: page 1 commits its cursor, page 2 fails -- crawl 1 is
            // abandoned open, holding `p2`.
            Run::new(vec![
                page(
                    None,
                    vec![obs("keep", "10.00")],
                    drained_status(Some(cursor.clone())),
                ),
                json!({
                    "when": {"resources": [{"resource_id": RESOURCE_ID, "page": cursor}]},
                    "do": [{"op": "reply_err", "code": "internal", "message": "boom"}]
                }),
            ]),
            // Run 1: the resume. It must ask for `p2`, and it drains.
            Run::new(vec![page(
                Some(json!({"kind": "cursor", "cursor": "p2"})),
                vec![obs("second-page", "20.00")],
                drained_status(None),
            )]),
            // Run 2: another `--resume`. The only rule matches a request
            // carrying NO page at all, so a resume that re-sent the dead
            // cursor gets `no scripted rule for this request`.
            Run::new(vec![raw_page(
                json!({"resources": [{"resource_id": RESOURCE_ID}]}),
                json!({
                    "observations": [obs("keep", "10.00"), obs("second-page", "20.00")],
                    "statuses": [drained_status(None)]
                }),
            )]),
        ],
    );
    let mut store = store(&scratch);

    let crashed = only(
        refresh_run(&mut store, &fixture, 0, SweepOptions::default())
            .await
            .0,
    );
    assert!(crashed.error.is_some(), "page 2 failed");
    assert!(
        sumer_store::store::resumable_crawl(store.conn(), ADAPTER_ID, RESOURCE_ID)
            .unwrap()
            .is_some(),
        "the abandoned crawl is what --resume exists for"
    );

    let resume = SweepOptions {
        resume: true,
        confirm_empty: false,
    };
    let resumed = only(refresh_run(&mut store, &fixture, 1, resume).await.0);
    assert!(resumed.error.is_none(), "{:?}", resumed.error);
    assert!(
        sumer_store::store::resumable_crawl(store.conn(), ADAPTER_ID, RESOURCE_ID)
            .unwrap()
            .is_none(),
        "the resume drained; there is nothing left to resume from"
    );

    let third = only(refresh_run(&mut store, &fixture, 2, resume).await.0);
    assert!(
        third.error.is_none(),
        "a third --resume must start at page: None, not re-send the dead cursor: {:?}",
        third.error
    );
    assert!(third.complete);
}

/// `resumable_crawl` must not read every crawl ever run. The query is on
/// the `--resume` path, which runs once per resource per refresh, and crawl
/// history only grows.
#[test]
fn the_resume_lookup_is_indexed() {
    let scratch = Scratch::new("resume-plan");
    let store = store(&scratch);
    let plan: String = store
        .conn()
        .query_row(
            "EXPLAIN QUERY PLAN
             SELECT crawl_id FROM crawl
             WHERE adapter_id = ?1 AND resource_id = ?2 AND drained = 0
               AND next_page IS NOT NULL
             ORDER BY crawl_id DESC LIMIT 1",
            ["a", "r"],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("USING INDEX"),
        "expected an index scan, got {plan:?}"
    );
}
