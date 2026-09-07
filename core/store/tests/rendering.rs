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

use sumer_money::{Amount, AssetId};
use sumer_store::render::balance_line;
use sumer_store::store::{balance_history, BalanceRow};
use sumer_store::sweep::SweepOptions;
use sumer_wire::Staleness;

fn line(store: &sumer_store::Store, category: &str) -> String {
    let history = balance_history(store.conn(), support::ADAPTER_ID, support::RESOURCE_ID).unwrap();
    let rows: Vec<&BalanceRow> = history.iter().filter(|r| r.category == category).collect();
    balance_line(category, &rows)
}

/// A hand-built row, for the two `balance_line` tests below that check
/// which row's fields end up on the recovered line -- no adapter, no
/// fixture, just the shape `balance_history` would hand back.
fn row(
    provider_id: &str,
    amount: Option<&str>,
    received_at: &str,
    staleness: Staleness,
    outcome: &str,
    from_latest_read: bool,
) -> BalanceRow {
    BalanceRow {
        balance_id: 0,
        adapter_id: support::ADAPTER_ID.to_owned(),
        resource_id: support::RESOURCE_ID.to_owned(),
        category: "available".to_owned(),
        canonical_hint: None,
        amount: amount.map(|a| Amount::parse(AssetId::new("usd").unwrap(), a).unwrap()),
        provider_id: provider_id.to_owned(),
        received_at: received_at.to_owned(),
        staleness,
        outcome: outcome.to_owned(),
        from_latest_read,
    }
}

/// **The bug**: a null balance from a DIFFERENT provider than the one that
/// last reported a figure must not steal that figure's identity. Provider
/// "a" reported `42.00`, cached as of 2026-01-01, received 2026-01-10.
/// Provider "b" later reports nothing at all. The recovered `42.00` is
/// still A's: A's provider, A's own as-of date, not the date B's failed
/// read was received.
#[test]
fn a_null_balance_from_a_different_provider_keeps_the_original_providers_identity() {
    let cached = row(
        "provider-a",
        Some("42.00"),
        "2026-01-10T00:00:00Z",
        Staleness::Cached,
        "stale:2026-01-01T00:00:00Z",
        true,
    );
    let failed = row(
        "provider-b",
        None,
        "2026-01-20T00:00:00Z",
        Staleness::Unavailable,
        "unavailable",
        true,
    );
    let line = balance_line("available", &[&cached, &failed]);

    assert!(
        line.contains("provider-a"),
        "the figure is A's, so A must be the credited provider: {line}"
    );
    assert!(
        !line.contains("provider-b"),
        "B reported nothing -- it must not be attributed A's number: {line}"
    );
    assert!(
        line.contains("2026-01-01"),
        "the freshness date is the adapter's own as-of, not a receipt time: {line}"
    );
    assert!(
        !line.contains("2026-01-10") && !line.contains("2026-01-20"),
        "neither row's receipt time belongs in the as-of slot: {line}"
    );
}

/// **The same bug, one read later**: a row the newest read did not rewrite
/// prints the ADAPTER's own as-of date, not the receipt time of the row
/// itself. There is no marker row to carry a host timestamp any more --
/// the row is the adapter's, and only its freshness verdict changed.
#[test]
fn a_row_no_later_read_refreshed_keeps_the_adapters_own_as_of_date() {
    let cached = row(
        "provider-a",
        Some("42.00"),
        "2026-01-10T00:00:00Z",
        Staleness::Cached,
        "stale:2026-01-01T00:00:00Z",
        false,
    );
    let line = balance_line("available", &[&cached]);

    assert!(
        line.contains("42.00") && line.contains("stale (as of"),
        "the figure survives, marked stale: {line}"
    );
    assert!(
        line.contains("2026-01-01"),
        "the adapter's own as-of is what dates the figure: {line}"
    );
    assert!(
        !line.contains("live"),
        "a line no read since has refreshed is not live: {line}"
    );
}

/// **The rule itself, with nothing else moving.** Same row, same adapter
/// staleness, same everything -- except whether the newest read is the one
/// that wrote it. That single bit is the whole of freshness now, so it is
/// worth one test that changes nothing else.
#[test]
fn the_same_row_reads_live_only_while_it_is_the_latest_reads_own() {
    let fresh = row(
        "provider-a",
        Some("42.00"),
        "2026-01-10T00:00:00Z",
        Staleness::Live,
        "fetched",
        true,
    );
    let mut superseded = fresh.clone();
    superseded.from_latest_read = false;

    assert!(balance_line("available", &[&fresh]).contains("live"));
    let stale = balance_line("available", &[&superseded]);
    assert!(
        !stale.contains("live") && stale.contains("42.00") && stale.contains("stale (as of"),
        "a read that did not refresh this line leaves the figure, not its freshness: {stale}"
    );
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

/// **F5**: a read that did not happen writes NOTHING.
///
/// The host used to answer "this figure was not refreshed" with a marker
/// row of its own, in the same append-only stream adapter rows live in --
/// which meant every adapter-authored column on it had to be copied
/// forward rather than invented (`spec/observation.md` §1, §8.4). The rule
/// held only as long as every writer remembered it.
///
/// Now the stream holds adapter rows and nothing else: the fact that a
/// category was not refreshed is the ABSENCE of a row from the newest
/// read. A host that writes no row cannot fabricate a provider field, and
/// there is no second kind of row for a reader to have to tell apart.
#[tokio::test]
async fn a_read_that_did_not_happen_writes_no_row_at_all() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-no-host-row");
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
    assert_eq!(
        rows.len(),
        1,
        "the second read said nothing about this category, so it wrote \
         nothing -- the only row is the adapter's own: {rows:?}"
    );
    assert_eq!(rows[0].amount.as_deref(), Some("42.00"));
    assert_eq!(
        rows[0].canonical_hint.as_deref(),
        Some("available"),
        "and it is untouched: {rows:?}"
    );

    // The row is the adapter's, unchanged -- and it is no longer live.
    let available = line(&store, "available");
    assert!(
        available.contains("42.00") && !available.contains("live"),
        "the figure survives the read that did not happen; its freshness \
         does not: {available}"
    );
}

/// **F6**: a resource DROPPED from a successful listing stops reading
/// `live`.
///
/// The first refresh lists the account and reads `42.00`. The second lists
/// nothing at all -- a wholly successful `resources.list` that no longer
/// mentions it, which is what a closed card or a removed wallet looks like
/// -- and reads balances for the resources that remain. `sumer balances`
/// still enumerates the account from the STORE, so the figure is still on
/// screen, and under the old marker scheme it was still on screen marked
/// `live`: every marker loop iterated *this run's* resource ids, so a
/// resource nobody listed was never visited and nothing was ever written
/// against it. Freshness is now a property of the row, so there is no loop
/// to leave a resource out of.
#[tokio::test]
async fn a_resource_dropped_from_a_listing_stops_reading_live() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-dropped-resource");
    let listed = Run::new(vec![page(
        None,
        vec![obs("a", "1.00")],
        drained_status(None),
    )])
    .balances(vec![balance("available", Some("42.00"))]);
    // Same adapter, same connection, a successful listing that simply does
    // not mention the resource any more.
    let dropped = Run::new(Vec::new()).lists(&[]).balances(Vec::new());

    let fixture = fixture(&scratch, vec![listed, dropped]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    assert!(line(&store, "available").contains("live"), "the first read");

    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;
    let available = line(&store, "available");
    assert!(
        !available.contains("live"),
        "a resource this refresh never even asked about cannot be live: {available}"
    );
    assert!(
        available.contains("42.00") && available.contains("stale (as of"),
        "and rule 2 still holds -- the last figure survives, marked stale: {available}"
    );
}

/// **F7**: a balance that names ANOTHER adapter never becomes this
/// adapter's figure.
///
/// Adapter A announces `hello` as A, correctly, and then returns a balance
/// whose `provenance.adapter_id` is B, for `2000.00`. Nothing downstream of
/// the decode can catch it: `append_balance` stores the CONNECTION's
/// adapter id, so the contradiction is discarded and the money is filed
/// under A. `history.read` refuses exactly this shape (§8.1 condition 8);
/// this earlier, separate write was never covered.
///
/// The reply is refused whole, so the refresh reports a failure and writes
/// no balance row -- and, freshness being derived, what was on screen goes
/// stale rather than staying `live` with someone else's number on it.
#[tokio::test]
async fn a_balance_naming_another_adapter_is_refused_and_never_displayed() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("render-foreign-balance");
    let history = || vec![page(None, vec![obs("a", "1.00")], drained_status(None))];
    let honest = Run::new(history()).balances(vec![balance("available", Some("42.00"))]);
    let mut foreign = balance("available", Some("2000.00"));
    foreign["provenance"]["adapter_id"] = json!("some-other-adapter");
    let impostor = Run::new(history()).balances(vec![foreign]);

    let fixture = fixture(&scratch, vec![honest, impostor]);
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    let rows = balance_columns(&store, "available");
    assert_eq!(
        rows.len(),
        1,
        "a balance the connection attributed to another adapter is not \
         stored under this one: {rows:?}"
    );
    let available = line(&store, "available");
    assert!(
        !available.contains("2000.00"),
        "and it never reaches the screen as this adapter's figure: {available}"
    );
    assert!(
        !available.contains("live"),
        "the read was refused, so nothing it did not refresh is live: {available}"
    );
    assert!(
        available.contains("42.00"),
        "the last honest figure is still there, marked stale: {available}"
    );
}

/// The balance columns `BalanceRow` does not carry. Read straight from the
/// table, because the point of the assertions above is what was WRITTEN --
/// and, more often now, what was not.
#[derive(Debug)]
struct RawBalance {
    canonical_hint: Option<String>,
    amount: Option<String>,
}

fn balance_columns(store: &sumer_store::Store, category: &str) -> Vec<RawBalance> {
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT canonical_hint, amount
             FROM balance WHERE category = ?1 ORDER BY balance_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([category], |row| {
            Ok(RawBalance {
                canonical_hint: row.get(0)?,
                amount: row.get(1)?,
            })
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}
