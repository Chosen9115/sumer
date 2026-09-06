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
use std::collections::{BTreeMap, HashMap};
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

// ---------------------------------------------------------------------
// Shared JSON shaping helpers. These used to live in `runner.rs`, where
// only the drivers could reach them; `exec.rs` and `ledger.rs` judge the
// same values and need the same rendering, so they live here with the
// rest of the comparison primitives.
// ---------------------------------------------------------------------

/// A fixture that names an `expect` key in the wrong shape must fail
/// loudly. Quietly returning instead is how an assertion stops being able
/// to fail -- several of this suite's assertions had already got there by
/// other routes.
pub fn expect_object<'a>(
    value: &'a Value,
    what: &str,
    failures: &mut Vec<Failure>,
) -> Option<&'a serde_json::Map<String, Value>> {
    value.as_object().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!("{what} must be an object, got {}", brief(value)),
        ));
        None
    })
}

pub fn expect_array<'a>(
    value: &'a Value,
    what: &str,
    failures: &mut Vec<Failure>,
) -> Option<&'a Vec<Value>> {
    value.as_array().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!("{what} must be an array, got {}", brief(value)),
        ));
        None
    })
}

pub fn expect_str<'a>(
    value: &'a Value,
    what: &str,
    failures: &mut Vec<Failure>,
) -> Option<&'a str> {
    value.as_str().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!("{what} must be a string, got {}", brief(value)),
        ));
        None
    })
}

pub fn expect_u64(value: &Value, what: &str, failures: &mut Vec<Failure>) -> Option<u64> {
    value.as_u64().or_else(|| {
        failures.push(Failure::new(
            "setup",
            format!(
                "{what} must be a non-negative integer, got {}",
                brief(value)
            ),
        ));
        None
    })
}

/// Caps a JSON value's rendering so a failure message stays readable when
/// the offending value is a leaked 70 KB blob.
#[must_use]
pub fn brief(value: &Value) -> String {
    let rendered = value.to_string();
    if rendered.len() <= 240 {
        return rendered;
    }
    let head: String = rendered.chars().take(240).collect();
    format!("{head}... [{} bytes total]", rendered.len())
}

/// Serializes a wire observation (or balance line) for comparison against
/// a fixture's declared entry.
#[must_use]
pub fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The same observation with the one field nothing can predict removed:
/// `provenance.received_at`, a fresh clock reading on every run.
///
/// `staleness` deliberately **stays**. It is host-derived, not adapter-sent,
/// but the host derives it per resource from that resource's own outcome
/// (spec/observation.md §1's freshness table), so it is a genuine
/// per-observation claim and a reproducible one: two runs of the same
/// fixture stamp it identically. It used to be stripped here alongside
/// `received_at`, which made it assertable in `expect.provenance` (balance
/// lines, compared without this view) and nowhere else -- and cost
/// `oversized_observation` the per-observation `staleness: cached` claim
/// that is exactly what broke when a degrade overwrote a stale outcome.
#[must_use]
pub fn stable_view(json: &Value) -> Value {
    let mut json = json.clone();
    if let Some(prov) = json
        .pointer_mut("/provenance")
        .and_then(Value::as_object_mut)
    {
        prov.remove("received_at");
    }
    json
}

/// Compares `expected` against `actual` as a subset -- with one exception:
/// `provider_extra` must match **exactly**.
///
/// A subset comparison cannot express "and nothing else", and
/// `provider_extra` is the one field where that is the whole assertion: A10
/// step 1 says the field is *replaced* by `{"_truncated": true,
/// "_original_bytes": N}`, and `fdx_lossless` says every FDX field lands
/// there verbatim and nothing else does. An adversarial review proved the
/// subset form accepts 70 KB of leaked payload sitting beside the
/// truncation marker.
pub fn diff_observation(actual: &Value, expected: &Value, path: &str, diffs: &mut Vec<String>) {
    let mut expected = expected.clone();
    if let Value::Object(map) = &mut expected {
        if let Some(expected_extra) = map.remove("provider_extra") {
            let actual_extra = actual.get("provider_extra").cloned().unwrap_or(Value::Null);
            if actual_extra != expected_extra {
                diffs.push(format!(
                    "{path}.provider_extra must be EXACTLY {}, got {}",
                    brief(&expected_extra),
                    brief(&actual_extra)
                ));
            }
        }
    }
    json_subset_diff(actual, &expected, path, diffs);
}

/// How two values for one `local_id` disagree. An observation *history* is
/// an array, and rendering two whole arrays side by side just truncates
/// into noise -- so a length mismatch says so, and a content mismatch names
/// the first index that differs.
#[must_use]
pub fn disagreement(actual: &Value, expected: &Value) -> String {
    if let (Value::Array(a), Value::Array(e)) = (actual, expected) {
        if a.len() != e.len() {
            return format!(
                "{} observation(s) in this execution, {} in the other",
                a.len(),
                e.len()
            );
        }
        if let Some((i, (a_i, e_i))) = a.iter().zip(e).enumerate().find(|(_, (x, y))| x != y) {
            return format!("observation {i} differs: {} != {}", brief(a_i), brief(e_i));
        }
    }
    format!("{} != {}", brief(actual), brief(expected))
}

/// Every way two keyed content maps can disagree, one line per key.
#[must_use]
pub fn content_diff(
    actual: &BTreeMap<String, Value>,
    expected: &BTreeMap<String, Value>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (key, expected_obs) in expected {
        match actual.get(key) {
            None => out.push(format!("{key:?}: missing")),
            Some(actual_obs) if actual_obs != expected_obs => {
                out.push(format!(
                    "{key:?}: {}",
                    disagreement(actual_obs, expected_obs)
                ));
            }
            Some(_) => {}
        }
    }
    for key in actual.keys().filter(|k| !expected.contains_key(*k)) {
        out.push(format!("{key:?}: unexpected, not in the other execution"));
    }
    out
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
