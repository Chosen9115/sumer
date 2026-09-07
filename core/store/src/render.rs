//! Rendering. ONE module, three rules, and no fourth place that formats a
//! figure.
//!
//! 1. **`amount: null` prints `unknown`, never `0`.** A wallet whose
//!    balance could not be read is not a wallet holding nothing, and a
//!    zero is a number a user acts on.
//! 2. **A read that did not happen never erases a figure, and never keeps
//!    it reading `live`.** It prints the last figure observed as `stale (as
//!    of T)`, or `unavailable` when there has never been one. Blanking the
//!    line loses information the store still holds; leaving it `live`
//!    asserts a freshness nobody measured. Which of the two a line is comes
//!    from `BalanceRow::from_latest_read` -- a fact about whether the
//!    newest read wrote this row, not a marker some failing path had to
//!    remember to write.
//! 3. **Every figure carries source and freshness on its own line.** A
//!    number with no provenance beside it is a number nobody can check.
//!
//! And: **no sums, no totals.** Adding two providers' figures together
//! asserts they are commensurate, comparable and simultaneous. None of
//! those is given, and the sum is the one number nothing can justify.

use sumer_wire::{Observation, ObservationState, Staleness};

use crate::store::{BalanceRow, RetractionRow, StoredObservation};

/// `12:04:31Z` from `2026-09-06T12:04:31Z`. Times of day are what a human
/// scanning a refresh actually reads; the full stamp is in `show`.
#[must_use]
pub fn time_of_day(timestamp: &str) -> &str {
    // `Rfc3339` guarantees at least 20 bytes with `T` at index 10, so this
    // slice is in range for any value that reached the store -- and
    // `get` rather than an index means a corrupt row degrades to the full
    // string instead of panicking.
    timestamp.get(11..20).unwrap_or(timestamp)
}

/// One balance line, rule 3's shape:
///
/// ```text
///   confirmed        130000 sat   live · https://blockstream.info/api · 12:04:31Z
/// ```
///
/// `history` is that category's whole append-only history, oldest first --
/// rule 2 needs it, because the last figure observed can be several reads
/// behind the most recent one.
#[must_use]
pub fn balance_line(category: &str, history: &[&BalanceRow]) -> String {
    let Some(latest) = history.last() else {
        return format!("  {category:<14} unavailable");
    };
    // **Two independent claims, and `live` needs both.** `staleness` is
    // what the ADAPTER said about the figure it handed over;
    // `from_latest_read` is whether any read since has refreshed this line
    // at all. A row the newest read did not rewrite is a record of what
    // was true then -- however live the read that produced it was.
    let live = latest.from_latest_read && latest.staleness == Staleness::Live;
    match (&latest.amount, live) {
        // A figure the adapter just read. `live`, and the amount verbatim.
        (Some(amount), true) => format!(
            "  {category:<14} {:>14} {}   live · {} · {}",
            amount.to_string(),
            amount.asset().as_str(),
            latest.provider_id,
            time_of_day(&latest.received_at)
        ),
        // Either the adapter said outright that this is not current, or
        // no read since has looked again. Both print the figure with the
        // date it was actually observed.
        (Some(amount), false) => format!(
            "  {category:<14} {:>14} {}   stale (as of {}) · {} · {}",
            amount.to_string(),
            amount.asset().as_str(),
            latest.as_of(),
            latest.provider_id,
            time_of_day(&latest.received_at)
        ),
        // Rule 1: the adapter looked and does not know. Not zero.
        (None, true) => format!(
            "  {category:<14} {:>14}     unknown · {} · {}",
            "unknown",
            latest.provider_id,
            time_of_day(&latest.received_at)
        ),
        // Rule 2: the read failed, or never happened. Show the last figure
        // there ever was, marked stale; only say `unavailable` when there
        // is none.
        (None, false) => match history
            .iter()
            .rev()
            .find_map(|row| row.amount.as_ref().map(|amount| (amount, row)))
        {
            // The recovered figure is `row`'s, not `latest`'s: its
            // provider, its own `as_of` (adapter `stale:` reason or
            // receipt time), never the row that just failed to read.
            Some((amount, row)) => format!(
                "  {category:<14} {:>14} {}   stale (as of {}) · {} · {}",
                amount.to_string(),
                amount.asset().as_str(),
                row.as_of(),
                row.provider_id,
                time_of_day(&row.received_at)
            ),
            None => format!(
                "  {category:<14} {:>14}     unavailable · {} · {}",
                "--",
                latest.provider_id,
                time_of_day(&latest.received_at)
            ),
        },
    }
}

impl BalanceRow {
    /// When the figure on this line was observed, as the adapter's own
    /// `stale { as_of }` reported it -- falling back to the host's receipt
    /// time when the outcome carried no `as_of`.
    fn as_of(&self) -> String {
        self.outcome
            .strip_prefix("stale:")
            .unwrap_or(&self.received_at)
            .to_owned()
    }
}

/// One history row: what it is, what it was worth, and when the host
/// learned it.
#[must_use]
pub fn history_line(local_id: &str, revision: u64, observation: &Observation) -> String {
    format!(
        "  {local_id}  r{revision}  {:>16} {}  {}  {}  {} · {}",
        observation.amount.to_string(),
        observation.amount.asset().as_str(),
        posting_label(observation),
        observation.description,
        freshness(observation.provenance.staleness),
        observation.provenance.provider_id,
    )
}

fn posting_label(observation: &Observation) -> &'static str {
    match observation.state {
        ObservationState::Tombstoned => "tombstoned",
        ObservationState::Active => match observation.posting {
            sumer_wire::Posting::Pending => "pending",
            sumer_wire::Posting::Posted => "posted",
            sumer_wire::Posting::Unknown => "unknown",
        },
    }
}

fn freshness(staleness: Staleness) -> &'static str {
    match staleness {
        Staleness::Live => "live",
        Staleness::Cached => "cached",
        Staleness::Unavailable => "unavailable",
    }
}

/// `show`: the whole chain and every retraction against it, unioned by
/// revision -- which is what makes a retraction explainable. The crawl
/// that caused each one is named, because "why is this gone" must have an
/// answer that is not a guess.
#[must_use]
pub fn show_lines(chain: &[StoredObservation], retractions: &[RetractionRow]) -> Vec<String> {
    let mut rows: Vec<(u64, u8, String)> = Vec::new();
    for stored in chain {
        rows.push((
            stored.revision,
            0,
            format!(
                "  r{}  observation  {:>16} {}  {}  crawl {} · {} · received {} · content {}",
                stored.revision,
                stored.observation.amount.to_string(),
                stored.observation.amount.asset().as_str(),
                posting_label(&stored.observation),
                stored.crawl_id,
                stored.observation.provenance.provider_id,
                stored.observation.provenance.received_at.as_str(),
                // The dedup key, so "why did this refresh append a row"
                // has an answer a user can read: two rows with the same
                // content hash cannot both exist, and two with different
                // ones differ in something the adapter actually said.
                stored
                    .content_hash
                    .get(..12)
                    .unwrap_or(&stored.content_hash),
            ),
        ));
    }
    for retraction in retractions {
        rows.push((
            retraction.revision,
            1,
            format!(
                "  r{}  RETRACTED    {}  crawl {} · at {}",
                retraction.revision,
                retraction.reason,
                retraction.crawl_id,
                retraction.retracted_at,
            ),
        ));
    }
    // A retraction sorts after the revision it retracts, so a revival
    // (revision N+1) reads in the order it happened.
    rows.sort_by_key(|(revision, kind, _)| (*revision, *kind));
    rows.into_iter().map(|(_, _, line)| line).collect()
}

/// The one line a refresh prints per resource.
#[must_use]
pub fn sweep_line(report: &crate::sweep::SweepReport) -> String {
    let verdict = match (&report.error, &report.disqualified_reason) {
        (Some(error), _) => format!("sweep: failed -- {error}"),
        (None, Some(reason)) => {
            format!("sweep: partial -- {reason}, no retractions derived")
        }
        (None, None) => "sweep: complete".to_owned(),
    };
    format!(
        "{}/{}: {} new · {} revised · {} retracted · {} unchanged · {}",
        report.adapter_id,
        report.resource_id,
        report.new,
        report.revised,
        report.retracted,
        report.unchanged,
        verdict
    )
}
