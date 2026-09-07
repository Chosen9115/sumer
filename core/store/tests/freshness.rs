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
