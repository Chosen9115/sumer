//! What a refresh that FAILED leaves behind for `sumer balances` to print.
//!
//! Freshness is rendered from the balance stream, and the balance stream is
//! append-only: whatever row sits on top of a `(resource, category)` is what
//! the user is told. A refresh that never reached `balances.read` therefore
//! leaves the LAST SUCCESSFUL read's row on top. The refresh error is
//! reported, but a reported error and a `live` figure are two different
//! things arriving in two different places, and only one of them is the
//! number the user acts on.
//!
//! What makes that row stop reading `live` is not something the failing
//! path wrote. Every refresh of an adapter opens a READ before it does
//! anything that can fail, and a row is fresh only while it belongs to its
//! adapter's current read -- so a refresh that died anywhere at all leaves
//! rows from the read before it, and they say so.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use support::{balance, fixture, refresh_run, store, Run, Scratch};

use sumer_store::store::BalanceRow;
use sumer_store::sweep::SweepOptions;

/// The row `sumer balances` reads: the last one appended for this category.
fn top(store: &sumer_store::Store, category: &str) -> BalanceRow {
    sumer_store::store::balance_history(store.conn(), support::ADAPTER_ID, support::RESOURCE_ID)
        .unwrap()
        .into_iter()
        .rfind(|row| row.category == category)
        .expect("the category has a row")
}

/// **A refresh that fails BEFORE `balances.read` leaves nothing reading
/// `live`.**
///
/// `status.read` runs before the balances do, and its failure returns from
/// the refresh early. The marker that downgraded an unread figure used to
/// be written only inside the `balances.read` arm, so every path that
/// returned before it -- a spawn failure, a `resources.list` failure, this
/// one -- left the last successful read's row on top, stamped `live`. One
/// good read followed by an unbounded run of failures printed a `live`
/// figure from a read that happened days ago, for ever.
///
/// The failing path now writes nothing, which is the point: it opened a
/// read, wrote no row against it, and the figure from the read before is
/// visibly not the current one.
#[tokio::test]
async fn a_failure_before_balances_read_does_not_leave_the_figure_live() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("early-failure");
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(Vec::new()).balances(vec![balance("available", Some("100.00"))]),
            // Everything after `resources.list` fails.
            Run::new(Vec::new()).status_read_err(),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let live = top(&store, "available");
    assert_eq!(live.outcome, "fetched", "the first read really did land");
    assert!(live.amount.is_some());

    let handle = support::connect(&fixture, 1).await;
    let failed = sumer_store::refresh::refresh_adapter(
        &mut store,
        &handle,
        support::ADAPTER_ID,
        SweepOptions::default(),
    )
    .await;
    let _ = handle.close().await;
    assert!(failed.is_err(), "the refresh failed and said so");

    let after = top(&store, "available");
    assert!(
        !after.from_latest_read,
        "a read that did not happen must not go on reading `live`; the row \
         on top is still the one the LAST read wrote"
    );
    assert_eq!(
        after.balance_id, live.balance_id,
        "and the failure wrote no row of its own -- there is nothing for a \
         host to have fabricated a provider field onto"
    );

    // What the user actually sees: the figure survives, its freshness does
    // not.
    let history = sumer_store::store::balance_history(
        store.conn(),
        support::ADAPTER_ID,
        support::RESOURCE_ID,
    )
    .unwrap();
    let rows: Vec<&BalanceRow> = history
        .iter()
        .filter(|r| r.category == "available")
        .collect();
    let line = sumer_store::render::balance_line("available", &rows);
    assert!(
        !line.contains("live") && line.contains("100.00") && line.contains("stale (as of"),
        "the freshness the renderer reads is downgraded, not the error log \
         alone: {line}"
    );
}

/// The whole balance history of one category, oldest first.
fn rows(store: &sumer_store::Store, category: &str) -> Vec<BalanceRow> {
    sumer_store::store::balance_history(store.conn(), support::ADAPTER_ID, support::RESOURCE_ID)
        .unwrap()
        .into_iter()
        .filter(|row| row.category == category)
        .collect()
}

/// What `sumer balances` prints for a category.
fn line(store: &sumer_store::Store, category: &str) -> String {
    let history = rows(store, category);
    sumer_store::render::balance_line(category, &history.iter().collect::<Vec<_>>())
}

/// **A balance for a resource the call never asked about is refused, not
/// stamped `live` on an outcome nobody gave** (spec/observation.md §6).
///
/// The adapter lists the account, is read, and then its next SUCCESSFUL
/// listing drops the account -- so the next `balances.read` asks about
/// nothing at all. The reply volunteers a figure for the dropped account
/// anyway, with honest provenance and no status entry for it.
///
/// Every check the reply passed was pointed the other way: coverage
/// validates the REQUESTED resources (none), and the provenance names the
/// connection's own adapter. So the line was stored against the current
/// read, with a host-synthesized `unknown` outcome and the `Live` staleness
/// that a resource with no status defaulted to -- and `sumer balances`
/// printed a figure as `live` on a freshness claim no §6 outcome ever made.
#[tokio::test]
async fn a_balance_for_a_resource_nobody_asked_about_is_refused() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("volunteered-balance");
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(Vec::new()).balances(vec![balance("available", Some("42.00"))]),
            // A successful listing that no longer mentions the account,
            // and a reply that speaks about it regardless.
            Run::new(Vec::new())
                .lists(&[])
                .balances(vec![balance("available", Some("999.00"))])
                .balance_statuses(serde_json::json!([])),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    assert!(line(&store, "available").contains("live"), "the first read");
    let before = rows(&store, "available").len();

    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    assert_eq!(
        rows(&store, "available").len(),
        before,
        "a figure whose freshness no status established never enters the store"
    );
    let available = line(&store, "available");
    assert!(
        !available.contains("999.00"),
        "and it never reaches the screen: {available}"
    );
    assert!(
        !available.contains("live") && available.contains("42.00"),
        "what is left is the last figure that WAS asked for, stale: {available}"
    );
}

/// **And a status beside the figure does not buy it in.** The rule above is
/// bounded by the REQUEST, not by whether a status happens to exist. This is
/// the reply that separates the two: the adapter has stopped listing the
/// account, so nobody asked about it, and it volunteers a balance *and* a
/// matching `fetched` status for it anyway -- honest provenance, plausible
/// figure, complete paperwork.
///
/// A status-bounded rule (`adr/0006` records it as the rejected alternative)
/// accepts this one: the status is right there, so the figure gets a §6
/// outcome and a `Live` staleness derived from it, and `999.00` goes back on
/// screen reading `live` for a resource the listing dropped. That is the same
/// user-facing bug as the unstatused case, with better paperwork -- and the
/// weaker rule passes the unstatused test, which is why this one has to
/// exist.
#[tokio::test]
async fn a_volunteered_balance_is_refused_even_when_a_status_comes_with_it() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("statused-volunteered-balance");
    let fixture = fixture(
        &scratch,
        vec![
            Run::new(Vec::new()).balances(vec![balance("available", Some("42.00"))]),
            Run::new(Vec::new())
                .lists(&[])
                .balances(vec![balance("available", Some("999.00"))])
                .balance_statuses(serde_json::json!([
                    {"resource_id": support::RESOURCE_ID,
                     "outcome": {"fetched": {"page_empty": false}}}
                ])),
        ],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;
    assert!(line(&store, "available").contains("live"), "the first read");
    let before = rows(&store, "available").len();

    refresh_run(&mut store, &fixture, 1, SweepOptions::default()).await;

    assert_eq!(
        rows(&store, "available").len(),
        before,
        "a figure for a resource the call never named never enters the \
         store, however complete the paperwork around it"
    );
    let available = line(&store, "available");
    assert!(
        !available.contains("999.00"),
        "and it never reaches the screen: {available}"
    );
    assert!(
        !available.contains("live") && available.contains("42.00"),
        "what is left is the last figure that WAS asked for, stale: {available}"
    );
}

/// The control: a resource that is still listed and does carry a status is
/// read, stored and rendered `live` exactly as before. The rule above
/// refuses a reply that speaks out of turn -- not a reply that answers.
#[tokio::test]
async fn a_listed_and_statused_balance_is_still_stored() {
    if !support::python3_available() {
        return;
    }
    let scratch = Scratch::new("statused-balance");
    let fixture = fixture(
        &scratch,
        vec![Run::new(Vec::new())
            .balances(vec![balance("available", Some("42.00"))])
            .balance_statuses(
                serde_json::json!([{"resource_id": support::RESOURCE_ID, "outcome": {"fetched": {"page_empty": false}}}]),
            )],
    );
    let mut store = store(&scratch);
    refresh_run(&mut store, &fixture, 0, SweepOptions::default()).await;

    let stored = rows(&store, "available");
    assert_eq!(stored.len(), 1, "the figure the adapter reported is stored");
    assert_eq!(
        stored[0].outcome, "fetched",
        "with the outcome the adapter gave it, not one the host made up"
    );
    let available = line(&store, "available");
    assert!(
        available.contains("42.00") && available.contains("live"),
        "and it renders live: {available}"
    );
}
