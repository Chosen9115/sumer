//! A differential test for [`sumer_host::time::instant`] whose oracle is
//! **able to disagree with it**.
//!
//! The test this replaces ran a quarter of a million values against a
//! Python oracle and reported zero disagreements, while two bugs sat in
//! the code it was checking. It could not have failed:
//!
//! * its impossible-date oracle was `date(y, m, 1) + timedelta(days=d-1)`,
//!   which normalises `2026-02-30` into `2026-03-02` *exactly the way the
//!   implementation does*. An oracle built out of the behaviour under test
//!   agrees with it by construction, including where that behaviour is
//!   wrong.
//! * Python's `datetime` is microsecond-precision, so it cannot represent
//!   a tenth fractional digit, let alone tell `.0000000001` from
//!   `.0000000002`. The whole sub-nanosecond region was invisible to it.
//!
//! So the oracle here is derived from the *calendar and the definition of
//! a fraction*, not from the code:
//!
//! * dates are counted with a month-length table and the Gregorian leap
//!   rule, accumulating year by year — a construction with nothing in
//!   common with the closed-form era arithmetic in `time.rs`, and one that
//!   **rejects** a day the month does not have instead of rolling it
//!   forward. Its own day numbers are pinned to a third implementation
//!   (GNU `date -u -d '<date>' +%s`, glibc, proleptic Gregorian) at the
//!   anchors in [`oracle_matches_externally_known_epoch_days`], so the
//!   oracle is anchored to reality and not merely to itself.
//! * fractions are read as what RFC 3339 says they are — a decimal
//!   fraction of a second — and converted by exact integer division, which
//!   *fails* rather than truncates when the value is finer than a
//!   nanosecond. Ordering is compared on the full digit string, at
//!   whatever precision the input carries.
//!
//! **What this oracle cannot check**, and how that is covered instead:
//!
//! * `(i64, u32)` cannot represent a sub-nanosecond instant, so no
//!   differential over that region is possible — there is nothing to
//!   compare against. It is covered by a policy assertion instead
//!   ([`sub_nanosecond_fractions_are_unorderable`]) plus the ordering
//!   property below, which is stated on the *exact decimal* and so still
//!   holds where the return type gives out.
//! * leap seconds: `:60` is accepted by the wire validator and is not a
//!   real extra second in this arithmetic. Out of scope here; the value it
//!   maps to is asserted in `time.rs`'s own tests, not oracled.
//! * this says nothing about the wire validator's own accept/reject set,
//!   only about ordering what it has already accepted.
//!
//! [`corpus_is_not_hollow`] asserts the corpus actually contains the cases
//! that make the oracle able to disagree, so this test cannot quietly
//! decay back into one that always passes.

// Integration-test binary: a panic here fails the test, which is the
// intended failure mode, not the "return an error" the workspace lint
// enforces for library code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cmp::Ordering;

use sumer_host::time::{instant, now_rfc3339};
use sumer_wire::Rfc3339;

// ---------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------

const MONTH_LENGTHS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn month_length(year: i64, month: usize) -> i64 {
    if month == 2 && is_leap(year) {
        29
    } else {
        MONTH_LENGTHS[month - 1]
    }
}

/// Days since 1970-01-01 for a civil date, or `None` if the calendar has
/// no such date. Counted, not computed: year by year, then month by month
/// out of the table. Deliberately not the algorithm under test.
fn oracle_days(year: i64, month: i64, day: i64) -> Option<i64> {
    let month = usize::try_from(month).ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > month_length(year, month) {
        return None;
    }
    let mut days = 0i64;
    if year >= 1970 {
        for y in 1970..year {
            days += if is_leap(y) { 366 } else { 365 };
        }
    } else {
        for y in year..1970 {
            days -= if is_leap(y) { 366 } else { 365 };
        }
    }
    for m in 1..month {
        days += month_length(year, m);
    }
    Some(days + day - 1)
}

/// The nanoseconds a fraction's digits name, or `None` when the value is
/// finer than a nanosecond. RFC 3339's fraction is `digits / 10^len`
/// seconds, so the nanosecond count is `digits * 10^9 / 10^len` and it
/// exists only when that division is exact. Trailing zeros carry no value
/// and are dropped first, so `.5000000000` is 500 000 000 ns and only
/// `.0000000001` is genuinely unrepresentable.
fn oracle_nanos(digits: &str) -> Option<u32> {
    let significant = digits.trim_end_matches('0');
    if significant.len() > 9 {
        return None;
    }
    let mut numerator: u128 = 0;
    for c in significant.chars() {
        numerator = numerator * 10 + u128::from(c.to_digit(10)?);
    }
    let scale = 10u128.checked_pow(u32::try_from(significant.len()).ok()?)?;
    let nanos = numerator * 1_000_000_000 / scale;
    u32::try_from(nanos).ok()
}

/// A timestamp split into `(seconds since the epoch, fraction digits)`,
/// with the UTC offset already applied and the fraction kept at *full*
/// input precision. `None` when the value names no instant at all.
///
/// A second parser on purpose — a differential test with one parser is a
/// tautology — and written by splitting on the separators rather than by
/// byte offsets, so it does not repeat `time.rs`'s indexing arithmetic.
fn oracle_exact(text: &str) -> Option<(i64, String)> {
    let (body, offset_secs) = if let Some(rest) = text.strip_suffix(['Z', 'z']) {
        (rest.to_owned(), 0i64)
    } else {
        let sign_at = text.rfind(['+', '-']).filter(|at| *at > 10)?;
        let (body, offset) = text.split_at(sign_at);
        let (sign, hhmm) = offset.split_at(1);
        let (hh, mm) = hhmm.split_once(':')?;
        let magnitude = field(hh, 2)? * 3600 + field(mm, 2)? * 60;
        (
            body.to_owned(),
            if sign == "+" { magnitude } else { -magnitude },
        )
    };

    let (date, time) = body.split_once(['T', 't'])?;
    let mut date_fields = date.split('-');
    let year = field(date_fields.next()?, 4)?;
    let month = field(date_fields.next()?, 2)?;
    let day = field(date_fields.next()?, 2)?;
    if date_fields.next().is_some() {
        return None;
    }

    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, fraction),
        None => (time, ""),
    };
    if !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut clock_fields = clock.split(':');
    let hour = field(clock_fields.next()?, 2)?;
    let minute = field(clock_fields.next()?, 2)?;
    let second = field(clock_fields.next()?, 2)?;
    if clock_fields.next().is_some() {
        return None;
    }

    let days = oracle_days(year, month, day)?;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some((secs, fraction.to_owned()))
}

/// The oracle's own view of `instant`'s return type: `None` also when the
/// fraction is finer than a nanosecond, because there is then no
/// `(i64, u32)` that is the right answer.
fn oracle_instant(text: &str) -> Option<(i64, u32)> {
    let (secs, fraction) = oracle_exact(text)?;
    Some((secs, oracle_nanos(&fraction)?))
}

fn field(text: &str, width: usize) -> Option<i64> {
    if text.len() != width || !text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Order two timestamps at *full* input precision, with no nanosecond
/// ceiling. This is the ordering the retraction gate is really asking
/// about; `instant`'s is an approximation of it.
fn oracle_cmp(left: &str, right: &str) -> Option<Ordering> {
    let (left_secs, left_fraction) = oracle_exact(left)?;
    let (right_secs, right_fraction) = oracle_exact(right)?;
    let width = left_fraction.len().max(right_fraction.len());
    Some(
        left_secs
            .cmp(&right_secs)
            .then_with(|| pad(&left_fraction, width).cmp(&pad(&right_fraction, width))),
    )
}

fn pad(digits: &str, width: usize) -> String {
    let mut padded = digits.to_owned();
    while padded.len() < width {
        padded.push('0');
    }
    padded
}

// ---------------------------------------------------------------------
// Corpora
// ---------------------------------------------------------------------

/// Years chosen for the leap rule's every branch (century, 400-year,
/// ordinary), both signs of the epoch offset, and the ends of the 4-digit
/// range the wire validator admits.
fn corpus_years() -> Vec<i64> {
    let mut years: Vec<i64> = (1968..=2032).collect();
    years.extend([
        1, 4, 100, 400, 1582, 1600, 1699, 1700, 1899, 1900, 2100, 2400, 9999,
    ]);
    years
}

/// Every fraction is a *string*, never a parsed number, so the corpus can
/// hold values no Rust or Python time type can represent.
const FRACTIONS: [&str; 18] = [
    "0",
    "00",
    "000000000",
    "0000000000",
    "1",
    "5",
    "25",
    "50",
    "500",
    "999999999",
    "5000000000",
    "0000000001",
    "0000000002",
    "9999999999",
    "00000000010000000000",
    "1000000000000000000000000000000000000000",
    "0000000000000000000000000000000000000001",
    "123456789987654321",
];

fn fraction_corpus() -> Vec<String> {
    let mut out = Vec::new();
    for base in ["2026-01-01T00:00:00", "2026-06-30T23:59:59"] {
        out.push(format!("{base}Z"));
        for fraction in FRACTIONS {
            out.push(format!("{base}.{fraction}Z"));
            out.push(format!("{base}.{fraction}+02:00"));
        }
    }
    out
}

fn parse(text: &str) -> Option<Rfc3339> {
    Rfc3339::new(text).ok()
}

// ---------------------------------------------------------------------
// The oracle is pinned to something outside this repository
// ---------------------------------------------------------------------

/// Day numbers taken from GNU coreutils `date -u -d '<date>' +%s` divided
/// by 86 400 — a third implementation, neither ours nor the oracle's. If
/// the oracle ever drifts, this is what catches it.
#[test]
fn oracle_matches_externally_known_epoch_days() {
    let anchors = [
        (1970, 1, 1, 0),
        (1969, 12, 31, -1),
        (2000, 1, 1, 10_957),
        (2038, 1, 19, 24_855),
        (1900, 1, 1, -25_567),
        (2016, 2, 29, 16_860),
        (2100, 3, 1, 47_541),
        (1600, 2, 29, -135_081),
        (1700, 3, 1, -98_556),
        (1, 1, 1, -719_162),
        (9999, 12, 31, 2_932_896),
    ];
    for (year, month, day, expected) in anchors {
        assert_eq!(
            oracle_days(year, month, day),
            Some(expected),
            "oracle disagrees with GNU date at {year:04}-{month:02}-{day:02}"
        );
    }
    // And the calendar it refuses.
    for (year, month, day) in [(2026, 2, 29), (2026, 2, 30), (2026, 4, 31), (1900, 2, 29)] {
        assert_eq!(
            oracle_days(year, month, day),
            None,
            "{year:04}-{month:02}-{day:02} is not a date"
        );
    }
}

/// The test's own liveness check. Every clause here is a way the corpus
/// could stop reaching the region a bug lives in; if one goes to zero this
/// test is hollow again and says so out loud.
#[test]
fn corpus_is_not_hollow() {
    let impossible = corpus_years()
        .into_iter()
        .flat_map(|year| {
            (1..=12).flat_map(move |month| (1..=31).map(move |day| (year, month, day)))
        })
        .filter(|(year, month, day)| oracle_days(*year, *month, *day).is_none())
        .count();
    assert!(
        impossible >= 500,
        "the date corpus must contain dates the calendar does not have, found {impossible}"
    );

    let sub_nanosecond = FRACTIONS
        .iter()
        .filter(|fraction| oracle_nanos(fraction).is_none())
        .count();
    assert!(
        sub_nanosecond >= 4,
        "the fraction corpus must reach past nanosecond resolution, found {sub_nanosecond}"
    );

    // The pair that a nine-digit truncation collapses: distinct instants
    // that a naive implementation calls equal.
    let earlier = "2026-01-01T00:00:00.0000000001Z";
    let later = "2026-01-01T00:00:00.0000000002Z";
    assert_eq!(oracle_cmp(earlier, later), Some(Ordering::Less));
    assert!(parse(earlier).is_some() && parse(later).is_some());
}

// ---------------------------------------------------------------------
// The differential
// ---------------------------------------------------------------------

/// Every year × every month × every day number the validator admits,
/// including the ~750 that name no date. `instant` must agree with the
/// oracle on *both* whether the value is orderable and what it orders as.
#[test]
fn every_admissible_date_agrees_with_the_oracle() {
    let mut checked = 0usize;
    for year in corpus_years() {
        for month in 1..=12 {
            for day in 1..=31 {
                let text = format!("{year:04}-{month:02}-{day:02}T00:00:00Z");
                let Some(ts) = parse(&text) else {
                    continue;
                };
                checked += 1;
                let expected = oracle_days(year, month, day).map(|days| (days * 86_400, 0));
                assert_eq!(
                    instant(&ts),
                    expected,
                    "{text}: an impossible date must be unorderable, never silently rolled \
                     into a different one -- rolling it moves the record across history_start"
                );
            }
        }
    }
    assert!(checked > 20_000, "corpus shrank to {checked}");
}

/// Clock fields and UTC offsets, over dates that do exist.
#[test]
fn clock_fields_and_offsets_agree_with_the_oracle() {
    for date in [
        "1969-12-31",
        "1970-01-01",
        "2000-02-29",
        "2026-09-06",
        "2400-12-31",
    ] {
        for hour in [0, 1, 12, 23] {
            for minute in [0, 30, 59] {
                for second in [0, 1, 59] {
                    for offset in ["Z", "+00:00", "+02:00", "-02:00", "+23:59", "-23:59"] {
                        let text = format!("{date}T{hour:02}:{minute:02}:{second:02}{offset}");
                        let Some(ts) = parse(&text) else {
                            panic!("{text} should be a valid wire timestamp");
                        };
                        assert_eq!(instant(&ts), oracle_instant(&text), "{text}");
                    }
                }
            }
        }
    }
}

/// Fractions, at every precision including ones no time type carries.
#[test]
fn fractions_agree_with_the_oracle() {
    for text in fraction_corpus() {
        let Some(ts) = parse(&text) else {
            panic!("{text} should be a valid wire timestamp");
        };
        assert_eq!(instant(&ts), oracle_instant(&text), "{text}");
    }
}

/// The region the oracle's *return type* cannot express, stated as policy
/// rather than as a comparison: a fraction finer than a nanosecond has no
/// correct `(i64, u32)`, so it must be unorderable. Truncating it is what
/// makes two different instants compare equal, and an equal compare is
/// what turns the gate's `at >= start` from false into true — a retraction
/// on a record that strictly precedes the bound.
#[test]
fn sub_nanosecond_fractions_are_unorderable() {
    for fraction in ["0000000001", "9999999999", "123456789987654321"] {
        let text = format!("2026-01-01T00:00:00.{fraction}Z");
        let ts = parse(&text).unwrap();
        assert_eq!(
            instant(&ts),
            None,
            "{text} is finer than a nanosecond and has no (secs, nanos) that is right"
        );
    }
    // Trailing zeros are not extra precision: these stay orderable.
    for (fraction, nanos) in [("5000000000", 500_000_000), ("0000000000", 0)] {
        let text = format!("2026-01-01T00:00:00.{fraction}Z");
        let ts = parse(&text).unwrap();
        assert_eq!(instant(&ts).map(|i| i.1), Some(nanos), "{text}");
    }
}

/// The property the retraction gate actually rests on, stated against the
/// *exact decimal* ordering so that it keeps its meaning past nanosecond
/// resolution: `instant` may refuse to order two timestamps, but it must
/// never order them differently from the way they really are — and in
/// particular must never report as equal two values one of which strictly
/// precedes the other.
#[test]
fn ordering_never_contradicts_the_exact_decimal_ordering() {
    let corpus = fraction_corpus();
    let mut compared = 0usize;
    for left in &corpus {
        for right in &corpus {
            let (Some(left_ts), Some(right_ts)) = (parse(left), parse(right)) else {
                continue;
            };
            let (Some(left_instant), Some(right_instant)) = (instant(&left_ts), instant(&right_ts))
            else {
                continue;
            };
            compared += 1;
            assert_eq!(
                left_instant.cmp(&right_instant),
                oracle_cmp(left, right).unwrap(),
                "{left} vs {right}"
            );
        }
    }
    assert!(compared > 1_000, "pair corpus shrank to {compared}");
}

/// The same property in the shape `sweep.rs` uses it: a record whose
/// timestamp strictly precedes `history_start` is outside the provider's
/// window and must never become retraction-eligible.
#[test]
fn a_record_before_history_start_is_never_retraction_eligible() {
    let corpus = fraction_corpus();
    for at in &corpus {
        for start in &corpus {
            if oracle_cmp(at, start) != Some(Ordering::Less) {
                continue;
            }
            let (Some(at_ts), Some(start_ts)) = (parse(at), parse(start)) else {
                continue;
            };
            let eligible = match (instant(&at_ts), instant(&start_ts)) {
                (Some(at), Some(start)) => at >= start,
                _ => false,
            };
            assert!(
                !eligible,
                "{at} precedes {start} yet the gate would retract it"
            );
        }
    }
}

/// The host's own stamp is a timestamp the oracle can read, and reads the
/// same way. Cheap end-to-end tie between the formatter and the parser.
#[test]
fn now_rfc3339_agrees_with_the_oracle() {
    let ts = now_rfc3339();
    assert_eq!(instant(&ts), oracle_instant(ts.as_str()), "{}", ts.as_str());
    let (secs, nanos) = instant(&ts).unwrap();
    assert_eq!(nanos, 0, "second precision");
    assert!(secs > 1_750_000_000, "a plausibly current instant: {secs}");
}
