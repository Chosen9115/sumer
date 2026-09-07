//! The three rendering rules, against figures a real adapter produced.
//!
//! These are worth their own test because every one of them is a rule
//! about what NOT to print: never a zero for an unknown, never a blank
//! where a figure used to be, never a bare number with no source beside
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::{json, Value};
use support::{balance, drained_status, fixture, obs, page, refresh_run, store, Run, Scratch};

use sumer_store::render::balance_line;
use sumer_store::store::{balance_history, BalanceRow};
use sumer_store::sweep::SweepOptions;

fn line(store: &sumer_store::Store, category: &str) -> String {
    let history = balance_history(store.conn(), support::ADAPTER_ID, support::RESOURCE_ID).unwrap();
    let rows: Vec<&BalanceRow> = history.iter().filter(|r| r.category == category).collect();
    balance_line(category, &rows)
}

/// **L6**: the adapter's amount text, byte for byte. `"42.00"` is not
/// `42`, and it is not `42.0`: the scale the provider chose is part of what
/// it said. Nothing between the wire and the screen may normalize it.
///
/// And rule 1: `amount: null` prints `unknown`. Never `0` -- an account
/// whose balance could not be read is not an account holding nothing, and
/// a zero is a number a user acts on.
#[tokio::test]
async fn amounts_print_byte_for_byte_and_null_prints_unknown() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render");
    let fixture = fixture(
        &scratch,
        vec![Run::new(vec![page(
            None,
            vec![obs("a", "1.00")],
            drained_status(None),
        )])
        .balances(vec![
            balance("available", Some("42.00")),
            balance("trailing", Some("1.500")),
            balance("negative", Some("-0.01")),
            balance("pending", None),
        ])],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    assert!(
        line(&store, "available").contains(" 42.00 usd"),
        "got {}",
        line(&store, "available")
    );
    assert!(
        line(&store, "trailing").contains(" 1.500 usd"),
        "trailing zeros are the provider's scale, not noise: {}",
        line(&store, "trailing")
    );
    assert!(
        line(&store, "negative").contains(" -0.01 usd"),
        "got {}",
        line(&store, "negative")
    );

    let pending = line(&store, "pending");
    assert!(
        pending.contains("unknown"),
        "a null amount prints `unknown`: {pending}"
    );
    assert!(
        !pending.contains(" 0 ") && !pending.contains("0.00"),
        "and NEVER a zero: {pending}"
    );

    // Rule 3: source and freshness on every line.
    assert!(line(&store, "available").contains("live · p1 ·"));
}

/// **Rule 2**: a failed read never erases a figure. It shows the last one
/// there ever was, marked stale -- and only says `unavailable` when there
/// has never been one.
#[tokio::test]
async fn a_failed_read_keeps_the_last_figure_and_marks_it_stale() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-stale");
    let good = || {
        Run::new(vec![page(
            None,
            vec![obs("a", "1.00")],
            drained_status(None),
        )])
        .balances(vec![balance("available", Some("42.00"))])
    };
    // The provider went dark: the adapter still reports the line, with no
    // figure and an `unavailable` outcome.
    let dark = Run::new(vec![page(
        None,
        vec![obs("a", "1.00")],
        drained_status(None),
    )])
    .balances(vec![balance("available", None), balance("never", None)])
    .balance_statuses(json!([{"resource_id": support::RESOURCE_ID, "outcome": "unavailable"}]));

    let fixture = fixture(&scratch, vec![good(), dark]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    let available = line(&store, "available");
    assert!(
        available.contains("42.00") && available.contains("stale (as of"),
        "a failed read shows the last figure, marked stale -- never a blank: {available}"
    );
    let never = line(&store, "never");
    assert!(
        never.contains("unavailable"),
        "a line that never had a figure says so rather than inventing one: {never}"
    );
    assert!(
        never.contains("--") && !never.contains("usd"),
        "with no figure there is no amount and no asset to print, only `--`: {never}"
    );
}

/// **Rule 2, the shape that shipped broken**: the adapter reports NOTHING
/// at all for the resource on the second refresh -- no balance line, not
/// even a null one, just an `unavailable` status with an empty
/// observation list. Before the fix, no balance row recorded that at
/// all, so the row from the first refresh -- staleness `Live` -- was
/// still the latest one, and `sumer balances` kept printing `42.00 ...
/// live` forever.
#[tokio::test]
async fn a_reply_with_no_observation_marks_the_last_figure_stale() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-stale-no-observation");
    let good = Run::new(vec![page(
        None,
        vec![obs("a", "1.00")],
        drained_status(None),
    )])
    .balances(vec![balance("available", Some("42.00"))]);
    let dark = Run::new(vec![page(
        None,
        vec![obs("a", "1.00")],
        drained_status(None),
    )])
    .balances(vec![])
    .balance_statuses(json!([{"resource_id": support::RESOURCE_ID, "outcome": "unavailable"}]));

    let fixture = fixture(&scratch, vec![good, dark]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    let available = line(&store, "available");
    assert!(
        available.contains("42.00") && available.contains("stale (as of"),
        "no observation at all for a resource must not leave the last figure reading live: {available}"
    );
    assert!(
        !available.contains("live"),
        "a refresh with nothing to say about this resource is not a live one: {available}"
    );
}

/// **Rule 2, the other broken shape**: `balances.read` fails outright --
/// not a status the adapter reported, the call itself never lands. Same
/// bug, same fix: the last figure must come back marked stale, not still
/// `live`.
#[tokio::test]
async fn a_failed_balances_read_call_marks_the_last_figure_stale() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-stale-call-failed");
    let fixture =
        fixture_with_failing_balances_read(&scratch, vec![balance("available", Some("42.00"))]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    let available = line(&store, "available");
    assert!(
        available.contains("42.00") && available.contains("stale (as of"),
        "balances.read failing outright must not leave the last figure reading live: {available}"
    );
    assert!(
        !available.contains("live"),
        "a failed read is not a live one: {available}"
    );
}

/// A fixture `support::Run` has no knob for: `balances.read` on the
/// second connection replying `reply_err` instead of ever answering with a
/// status. Built by hand, once, for the one test that needs it -- every
/// other rule is the same shape `Run::build` produces.
fn fixture_with_failing_balances_read(
    scratch: &Scratch,
    first_run_balances: Vec<Value>,
) -> std::path::PathBuf {
    let make_run = |balances_action: Value| {
        json!({
            "label": "store test",
            "hello": {
                "protocol": "1",
                "adapter_id": support::ADAPTER_ID,
                "adapter_version": "0.1.0",
                "capabilities": ["resources.list", "balances.read", "history.read", "status.read"],
                "local_id_derivation": support::DERIVATION,
                "max_in_flight": 1
            },
            "provenance": {
                "adapter_id": support::ADAPTER_ID,
                "provider_id": "p1",
                "surface": "s",
                "observed_at": "2026-01-01T00:00:00Z",
                "completeness": "complete"
            },
            "on": {
                "resources.list": [{"do": [{"op": "reply_ok", "body": {"resources": [{
                    "resource_id": support::RESOURCE_ID,
                    "provider_id": "p1",
                    "kind": "bank_checking",
                    "label": "Checking",
                    "provider_extra": {}
                }]}}]}],
                "status.read": [{"do": [{"op": "reply_ok", "body": {"statuses": [{"resource_id": support::RESOURCE_ID}]}}]}],
                "balances.read": [{"do": [balances_action]}],
                "history.read": [page(None, vec![obs("a", "1.00")], drained_status(None))]
            }
        })
    };
    let good = make_run(json!({
        "op": "reply_ok",
        "body": {"observations": first_run_balances, "statuses": [{"resource_id": support::RESOURCE_ID}]}
    }));
    let bad =
        make_run(json!({"op": "reply_err", "code": "internal", "message": "the provider is down"}));
    let document = json!({"case": "store-test", "script": {"runs": [good, bad]}});
    let path = scratch.path().join("fixture.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    path
}

/// **F4**: a category that stops being reported must stop reading `live`.
///
/// `spec/observation.md` §2 names the provider this happens on: Teller
/// guarantees only that *at least one* of `ledger`/`available` is present
/// on any given response. So a second, entirely SUCCESSFUL read that names
/// one of two categories is the documented normal case. Tracking "did this
/// read cover it" per RESOURCE misses it by exactly one level: the
/// resource was covered, the dropped category was not, and its last row --
/// staleness `Live` -- stays on top forever.
#[tokio::test]
async fn a_category_that_stops_being_reported_stops_reading_live() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-dropped-category");
    let history = || vec![page(None, vec![obs("a", "1.00")], drained_status(None))];
    let both = Run::new(history()).balances(vec![
        balance("available", Some("42.00")),
        balance("unconfirmed", Some("5")),
    ]);
    // Same read, same success, one category dropped.
    let one = Run::new(history()).balances(vec![balance("available", Some("42.00"))]);

    let fixture = fixture(&scratch, vec![both, one]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    let unconfirmed = line(&store, "unconfirmed");
    assert!(
        !unconfirmed.contains("live"),
        "a category this read said nothing about is not live: {unconfirmed}"
    );
    assert!(
        unconfirmed.contains("5 usd") && unconfirmed.contains("stale (as of"),
        "and rule 2 still holds -- the last figure survives, marked stale: {unconfirmed}"
    );
    // The reported category is untouched by any of this.
    assert!(
        line(&store, "available").contains("live"),
        "the category the read DID cover is still live: {}",
        line(&store, "available")
    );
}

/// **F5**: the host-authored marker row invents nothing.
///
/// `spec/observation.md` §1 defines `surface` as the adapter's account of
/// where a read came from and `observed_at` as adapter-claimed and carried
/// through unmodified; §8.4 gives retractions their own table precisely so
/// a host has "no provider field to fabricate". A marker row lands in the
/// same append-only stream adapter rows do, so every adapter-shaped column
/// on it must be COPIED from the row it marks unread -- as `provider_id`
/// already was -- and never authored by the host. `canonical_hint` is a
/// property of the category, not of the read, so it survives too.
#[tokio::test]
async fn a_marker_row_invents_no_adapter_authored_field() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-marker-provenance");
    let history = || vec![page(None, vec![obs("a", "1.00")], drained_status(None))];
    let mut hinted = balance("available", Some("42.00"));
    hinted["canonical_hint"] = json!("available");
    let good = Run::new(history()).balances(vec![hinted]);
    let dark = Run::new(history())
        .balances(vec![])
        .balance_statuses(json!([{"resource_id": support::RESOURCE_ID, "outcome": "unavailable"}]));

    let fixture = fixture(&scratch, vec![good, dark]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    let rows = balance_columns(&store, "available");
    assert_eq!(rows.len(), 2, "one adapter row, then one marker: {rows:?}");
    let (adapter_row, marker) = (&rows[0], &rows[1]);

    assert_eq!(
        marker.canonical_hint, adapter_row.canonical_hint,
        "the hint is a property of the CATEGORY; a read that did not happen \
         does not erase it: {rows:?}"
    );
    assert_eq!(
        marker.surface, adapter_row.surface,
        "`surface` is the adapter's account of where the read came from -- \
         carried forward, never authored by the host: {rows:?}"
    );
    assert_eq!(
        marker.observed_at, adapter_row.observed_at,
        "`observed_at` is adapter-claimed and carried through unmodified \
         (spec/observation.md §1): {rows:?}"
    );
    assert_eq!(
        marker.provider_id, adapter_row.provider_id,
        "and the provider, as before: {rows:?}"
    );

    // The host-authored columns, and the one field that tells a consumer
    // this row is host-authored at all.
    assert_eq!(
        marker.amount, None,
        "never a figure, never a zero: {rows:?}"
    );
    assert_eq!(marker.staleness, "unavailable");
    assert!(
        marker.outcome.starts_with("unread:"),
        "a consumer must be able to tell a host-authored marker from an \
         adapter row, and `outcome` is the host's own column: {rows:?}"
    );
    assert!(
        !adapter_row.outcome.starts_with("unread:"),
        "and a real adapter row never wears that prefix: {rows:?}"
    );
}

/// The balance columns `BalanceRow` does not carry. Read straight from the
/// table, because the point of the assertion above is what was WRITTEN.
#[derive(Debug)]
struct RawBalance {
    canonical_hint: Option<String>,
    amount: Option<String>,
    provider_id: String,
    surface: String,
    observed_at: String,
    staleness: String,
    outcome: String,
}

fn balance_columns(store: &sumer_store::Store, category: &str) -> Vec<RawBalance> {
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT canonical_hint, amount, prov_provider_id, prov_surface,
                    observed_at, staleness, outcome
             FROM balance WHERE category = ?1 ORDER BY balance_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([category], |row| {
            Ok(RawBalance {
                canonical_hint: row.get(0)?,
                amount: row.get(1)?,
                provider_id: row.get(2)?,
                surface: row.get(3)?,
                observed_at: row.get(4)?,
                staleness: row.get(5)?,
                outcome: row.get(6)?,
            })
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}
