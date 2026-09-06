//! Assertion primitives shared by every case in `runner.rs`: A1-A11 from
//! the frozen contract, section (g).
//!
//! The core mechanism is [`json_subset_diff`], a recursive comparator that
//! checks every field a fixture's `expect` names is present in the actual
//! reply with an equal value -- fields the fixture does not mention are
//! ignored, since each case deliberately asserts only what it is testing.
//! A field literally named `amount` is special-cased to compare via
//! [`sumer_money::Amount::cmp_same_asset`] rather than string/JSON equality
//! (A1: `"42.50"` and `"42.500"` must compare equal), which is also the one
//! place a value that was silently round-tripped through a float would
//! surface as a mismatch.
//!
//! Every failure names the JSON field path and the concrete actual-vs-
//! expected difference: a suite whose failures are unreadable does not get
//! used.

use serde_json::Value;
use std::collections::HashMap;
use sumer_money::{Amount, AssetId};

/// One failed assertion.
#[derive(Debug, Clone)]
pub struct Failure {
    /// Which named assertion this is evidence against (e.g. `"A1"`). Not
    /// exclusive -- a single wrong field can be evidence for more than one
    /// assertion in principle, but each call site names the one it is
    /// primarily checking.
    pub assertion: String,
    /// The concrete, human-readable difference.
    pub message: String,
}

impl Failure {
    #[must_use]
    pub fn new(assertion: impl Into<String>, message: impl Into<String>) -> Failure {
        Failure {
            assertion: assertion.into(),
            message: message.into(),
        }
    }
}

/// Recursively checks that every field named in `expected` is present in
/// `actual` with an equal value, appending a message to `diffs` for each
/// mismatch. `actual` may carry extra fields the fixture doesn't mention;
/// those are never flagged.
pub fn json_subset_diff(actual: &Value, expected: &Value, path: &str, diffs: &mut Vec<String>) {
    match expected {
        Value::Object(exp_map) => {
            let Value::Object(act_map) = actual else {
                diffs.push(format!("{path}: expected an object, actual was {actual}"));
                return;
            };
            for (k, exp_v) in exp_map {
                let child = format!("{path}.{k}");
                match act_map.get(k) {
                    None => diffs.push(format!("{child}: missing (expected {exp_v})")),
                    Some(act_v) if k == "amount" => compare_amount(act_v, exp_v, &child, diffs),
                    Some(act_v) => json_subset_diff(act_v, exp_v, &child, diffs),
                }
            }
        }
        Value::Array(exp_arr) => {
            let Value::Array(act_arr) = actual else {
                diffs.push(format!("{path}: expected an array, actual was {actual}"));
                return;
            };
            if act_arr.len() != exp_arr.len() {
                diffs.push(format!(
                    "{path}: expected {} element(s), got {} -- actual={actual}",
                    exp_arr.len(),
                    act_arr.len()
                ));
                return;
            }
            for (i, (a, e)) in act_arr.iter().zip(exp_arr).enumerate() {
                json_subset_diff(a, e, &format!("{path}[{i}]"), diffs);
            }
        }
        other => {
            if actual != other {
                diffs.push(format!("{path}: expected {other}, got {actual}"));
            }
        }
    }
}

/// `amount`-field comparison: `null` means unknown on both sides (A3 --
/// unknown is never zero, so a mismatched nullness is always a hard
/// failure), otherwise both sides are parsed and compared via
/// `Amount::cmp_same_asset` -- never raw string equality, per A1.
fn compare_amount(actual: &Value, expected: &Value, path: &str, diffs: &mut Vec<String>) {
    match (actual, expected) {
        (Value::Null, Value::Null) => {}
        (Value::Null, _) => diffs.push(format!("{path}: expected {expected}, got null (unknown)")),
        (_, Value::Null) => diffs.push(format!("{path}: expected null (unknown), got {actual}")),
        _ => match (parse_amount_json(actual), parse_amount_json(expected)) {
            (Ok(a), Ok(e)) => match a.cmp_same_asset(&e) {
                Ok(std::cmp::Ordering::Equal) => {}
                Ok(_) => diffs.push(format!(
                    "{path}: {actual} and {expected} are not numerically equal (cmp_same_asset)"
                )),
                Err(err) => diffs.push(format!("{path}: {actual} vs {expected}: {err}")),
            },
            (Err(e), _) => diffs.push(format!(
                "{path}: could not parse actual amount {actual}: {e}"
            )),
            (_, Err(e)) => diffs.push(format!(
                "{path}: could not parse expected amount {expected}: {e}"
            )),
        },
    }
}

fn parse_amount_json(v: &Value) -> Result<Amount, String> {
    let asset = v
        .get("asset")
        .and_then(Value::as_str)
        .ok_or("missing \"asset\"")?;
    let amount = v
        .get("amount")
        .and_then(Value::as_str)
        .ok_or("missing \"amount\" string")?;
    let asset = AssetId::new(asset).map_err(|e| e.to_string())?;
    Amount::parse(asset, amount).map_err(|e| e.to_string())
}

/// A7: every requested `resource_id` appears in `statuses_json` exactly
/// once, and `statuses_json` names nothing beyond what was requested. The
/// degenerate implementation this kills is "reply with a single blanket
/// status" or "report a resource twice" -- either shows up as a count other
/// than 1.
pub fn assert_status_coverage(
    requested: &[String],
    statuses_json: &[Value],
    failures: &mut Vec<Failure>,
) {
    let mut seen: HashMap<&str, u32> = HashMap::new();
    for s in statuses_json {
        if let Some(rid) = s.get("resource_id").and_then(Value::as_str) {
            *seen.entry(rid).or_insert(0) += 1;
        } else {
            failures.push(Failure::new(
                "A7",
                format!("a statuses entry has no resource_id: {s}"),
            ));
        }
    }
    for rid in requested {
        match seen.get(rid.as_str()) {
            None => failures.push(Failure::new(
                "A7",
                format!("resource_id {rid:?} is missing from statuses"),
            )),
            Some(1) => {}
            Some(n) => failures.push(Failure::new(
                "A7",
                format!("resource_id {rid:?} appears {n} times in statuses, expected exactly once"),
            )),
        }
    }
    let requested_set: std::collections::HashSet<&str> =
        requested.iter().map(String::as_str).collect();
    for extra in seen.keys().filter(|k| !requested_set.contains(*k)) {
        failures.push(Failure::new(
            "A7",
            format!("statuses names {extra:?}, which was never requested"),
        ));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn subset_diff_ignores_unmentioned_fields() {
        let actual = serde_json::json!({"a": 1, "b": 2});
        let expected = serde_json::json!({"a": 1});
        let mut diffs = Vec::new();
        json_subset_diff(&actual, &expected, "$", &mut diffs);
        assert!(diffs.is_empty());
    }

    #[test]
    fn subset_diff_flags_missing_field() {
        let actual = serde_json::json!({"a": 1});
        let expected = serde_json::json!({"b": 2});
        let mut diffs = Vec::new();
        json_subset_diff(&actual, &expected, "$", &mut diffs);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("$.b"));
    }

    #[test]
    fn amount_field_compares_numerically_not_by_string() {
        let actual = serde_json::json!({"amount": {"asset": "USD", "amount": "42.50"}});
        let expected = serde_json::json!({"amount": {"asset": "USD", "amount": "42.500"}});
        let mut diffs = Vec::new();
        json_subset_diff(&actual, &expected, "$", &mut diffs);
        assert!(
            diffs.is_empty(),
            "42.50 and 42.500 must compare equal: {diffs:?}"
        );
    }

    #[test]
    fn amount_field_null_vs_value_is_a_hard_mismatch() {
        let actual = serde_json::json!({"amount": null});
        let expected = serde_json::json!({"amount": {"asset": "USD", "amount": "0.00"}});
        let mut diffs = Vec::new();
        json_subset_diff(&actual, &expected, "$", &mut diffs);
        assert_eq!(
            diffs.len(),
            1,
            "unknown must never satisfy an asserted zero"
        );
    }

    #[test]
    fn status_coverage_flags_missing_and_duplicate() {
        let mut failures = Vec::new();
        assert_status_coverage(
            &["a".to_owned(), "b".to_owned()],
            &[serde_json::json!({"resource_id": "a", "outcome": "unavailable"})],
            &mut failures,
        );
        assert!(failures.iter().any(|f| f.message.contains("\"b\"")));
    }
}
