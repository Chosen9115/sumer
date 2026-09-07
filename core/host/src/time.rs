//! RFC 3339 arithmetic: the host's own clock, and the one way this
//! workspace turns a validated timestamp into something orderable.
//!
//! Public and reusable on purpose, for the same reason [`crate::fold`] and
//! [`crate::paging`] are: `sumer-store` had its own copy of `now_rfc3339`
//! and the two had already drifted (one panicked on the unreachable arm,
//! the other silently fell back to the Unix epoch). One copy, one failure
//! behaviour.
//!
//! **No date dependency.** A proleptic Gregorian conversion both ways is a
//! self-contained, well-known algorithm (Howard Hinnant's
//! `civil_from_days` / `days_from_civil`), and nothing here needs time
//! zones, locales, or calendar arithmetic beyond it.

use sumer_wire::Rfc3339;

/// The host's own receipt-time stamp, formatted to second precision as
/// `YYYY-MM-DDTHH:MM:SSZ` -- exactly the shape [`Rfc3339::new`] validates.
///
/// # Panics
/// Never, by construction: `year`/`month`/`day`/`hour`/`minute`/`second`
/// are all in range (the civil-calendar algorithm and the
/// `div_euclid`/`rem_euclid` splits guarantee it) and are formatted into
/// exactly the 20-byte shape the validator accepts. The panic is
/// deliberate rather than a fallback timestamp: this value is written into
/// `received_at`, `retracted_at` and `noted_at`, and a silently wrong
/// audit row is worse than a dead process. Every writer here runs inside
/// one transaction, so a panic rolls back rather than tearing the store.
#[must_use]
pub fn now_rfc3339() -> Rfc3339 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX);
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    let text = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z");
    Rfc3339::new(text).unwrap_or_else(|e| {
        unreachable!("now_rfc3339 built a timestamp its own crate rejects: {e}")
    })
}

/// The **instant** a timestamp names, as `(seconds since the Unix epoch,
/// nanoseconds)` -- the only ordering of two RFC 3339 values this
/// workspace performs.
///
/// Comparing the text instead is wrong in two ways that both reach the
/// retraction gate: `"2026-01-01T00:00:00Z" < "2026-01-01T00:00:00.500Z"`
/// is false as strings and true as instants, and
/// `"2026-01-01T01:00:00+02:00"` and `"2026-01-01T00:00:00Z"` are the same
/// instant written two ways. `sumer_wire`'s validator accepts fractional
/// seconds and numeric offsets, so both are reachable input rather than
/// hypotheticals.
///
/// `None` when the text is not a timestamp this can order. Callers deciding
/// whether to retract a record MUST treat that as "cannot order, so do not
/// retract": a wrong exemption costs a stale row, a wrong retraction hides
/// a financial record. Three of those cases are values `sumer_wire`'s
/// validator accepts and this cannot place on a line: a date the calendar
/// does not have (`2026-02-30`, admitted because the validator checks
/// `day <= 31` without the month), a fraction finer than a nanosecond
/// (`.0000000001`, admitted because the validator does not bound the
/// fraction's length), and a leap second (`:60`, admitted because the
/// validator tolerates one). All three would otherwise be answered with a
/// *plausible wrong instant* -- a rolled-forward date, a truncation that
/// makes two different instants equal, or (for the leap second) the
/// following second's own instant -- and a plausible wrong instant is what
/// retracts a record that was never inside the window.
#[must_use]
pub fn instant(ts: &Rfc3339) -> Option<(i64, u32)> {
    let b = ts.as_str().as_bytes();
    // `Rfc3339` has already validated this shape; the parse is written to
    // return `None` rather than trust that, because the cost of being
    // wrong here is a retraction.
    let year: i64 = number(b.get(0..4)?)?;
    let month: i64 = number(b.get(5..7)?)?;
    let day: i64 = number(b.get(8..10)?)?;
    let hour: i64 = number(b.get(11..13)?)?;
    let minute: i64 = number(b.get(14..16)?)?;
    let second: i64 = number(b.get(17..19)?)?;
    // `sumer_wire` admits `:60` to tolerate a leap second, but this clock is
    // plain seconds-since-epoch with no leap-second table, so there is no
    // slot for the extra second: `23:59:60` reduces to the same day-rollover
    // total as the following `00:00:00`. That is not a rounding error, it is
    // a genuinely different instant reported as equal, which turns the
    // retraction gate's `at >= start` from false to true for a record that
    // never entered the window. No current leap-second event is needed --
    // any historical `:60` in stored data reaches this. Unorderable, like
    // the impossible date and the sub-nanosecond fraction below.
    if second == 60 {
        return None;
    }

    let mut i = 19;
    let mut nanos: u32 = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
        // Finer than a nanosecond: `(i64, u32)` has no value that is the
        // right answer, and truncating to nine digits makes two different
        // instants compare EQUAL. That is not a rounding error at the
        // gate, it is a sign flip: with the record at `.0000000001` and
        // `history_start` at `.0000000002` the record strictly precedes
        // the bound, but truncation makes `at >= start` hold and the
        // record retraction-eligible. Unorderable, so the caller exempts.
        // Trailing zeros are not precision -- `.5000000000` is exactly
        // 500ms -- so only a non-zero digit past the ninth disqualifies.
        if b.get(start + 9..i)
            .is_some_and(|beyond_nanos| beyond_nanos.iter().any(|d| *d != b'0'))
        {
            return None;
        }
        // Nine digits of resolution, zero-padded on the right: a shorter
        // fraction is a coarser instant, not a smaller one, so `.5` must
        // order above `.25` rather than below it.
        for slot in 0..9 {
            let digit = match b.get(start + slot) {
                Some(byte) if start + slot < i => u32::from(byte - b'0'),
                // Past the fraction's own digits. `i` is the bound, not the
                // end of the string: the next byte is the offset (`Z`, `+`,
                // `-`), and reading it as a digit is how `.5` became
                // `.92`.
                _ => 0,
            };
            nanos = nanos * 10 + digit;
        }
    }

    let offset_secs = match b.get(i) {
        Some(b'Z' | b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let off_hour: i64 = number(b.get(i + 1..i + 3)?)?;
            let off_min: i64 = number(b.get(i + 4..i + 6)?)?;
            let magnitude = off_hour * 3600 + off_min * 60;
            if *sign == b'+' {
                magnitude
            } else {
                -magnitude
            }
        }
        _ => return None,
    };

    // `Rfc3339` admits `day <= 31` without consulting the month, and
    // `days_from_civil` answers for a day number the calendar does not
    // have by rolling forward -- `2026-02-30` comes back as March 2. An
    // impossible date is not a date, and normalising it into a *different,
    // valid* one moves the record across `history_start` in whichever
    // direction the arithmetic happens to go. `civil_from_days` only ever
    // returns real dates, so the round trip is the check: it holds exactly
    // when the input was a date.
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return None;
    }

    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some((secs, nanos))
}

/// The digits of an ASCII decimal field, or `None` if it is not all digits.
fn number(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: i64 = 0;
    for byte in bytes {
        n = n * 10 + i64::from(byte - b'0');
    }
    Some(n)
}

/// Civil (proleptic Gregorian) date from a day count since the Unix epoch.
/// Howard Hinnant's `civil_from_days`
/// (<http://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// The exact inverse: day count since the Unix epoch from a civil date.
/// Hinnant's `days_from_civil`, which stays monotonic on a day number the
/// calendar does not have (`2026-02-31`) rather than rejecting it. That
/// answer is not a date and callers must not use it as one: [`instant`]
/// round-trips through [`civil_from_days`] to reject those, which works
/// precisely because `civil_from_days` only ever emits real dates.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn at(text: &str) -> (i64, u32) {
        instant(&Rfc3339::new(text).unwrap()).expect("a validated timestamp is orderable")
    }

    #[test]
    fn now_rfc3339_is_well_formed_and_recent() {
        let ts = now_rfc3339();
        assert!(
            ts.as_str().starts_with("20"),
            "expected a 21st-century date, got {ts:?}"
        );
        assert!(ts.as_str().ends_with('Z'));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(20_702), (2026, 9, 6));
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        for days in [-100_000, -1, 0, 1, 11_017, 20_702, 100_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "round trip at {days}");
        }
    }

    /// The two orderings string comparison gets wrong, stated as the
    /// retraction gate meets them.
    #[test]
    fn fractional_seconds_order_as_instants_not_as_text() {
        let plain = "2026-01-01T00:00:00Z";
        let frac = "2026-01-01T00:00:00.500Z";
        assert!(
            plain > frac,
            "the string comparison this replaces reads the earlier instant as LATER"
        );
        assert!(at(plain) < at(frac));
    }

    #[test]
    fn a_numeric_offset_names_the_same_instant_as_z() {
        assert_eq!(at("2026-01-01T01:00:00+02:00"), at("2025-12-31T23:00:00Z"));
        assert_eq!(at("2025-12-31T22:00:00-02:00"), at("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn a_shorter_fraction_is_coarser_not_smaller() {
        assert!(at("2026-01-01T00:00:00.5Z") > at("2026-01-01T00:00:00.25Z"));
        assert_eq!(at("2026-01-01T00:00:00.5Z").1, 500_000_000);
    }
}
