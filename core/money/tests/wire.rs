//! Table-driven wire-format integration tests for `sumer-money`, written
//! independently against `spec/money.md` / the frozen PR-1 API contract.
//!
//! These exercise the crate strictly through its public surface (`Amount`,
//! `AssetId`, `MoneyError`, and serde) — no access to private fields.

use std::collections::HashMap;

use sumer_money::{Amount, AssetId, MoneyError};

/// Fails the test with a readable message instead of `.unwrap()`/`.expect()`
/// (both denied by the workspace clippy lints, which may cover test targets).
fn must_ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

fn must_err<T: std::fmt::Debug>(r: Result<T, MoneyError>) -> MoneyError {
    match r {
        Err(e) => e,
        Ok(v) => panic!("expected Err, got Ok({v:?})"),
    }
}

fn asset(s: &str) -> AssetId {
    must_ok(AssetId::new(s))
}

fn parse(asset_str: &str, amount: &str) -> Result<Amount, MoneyError> {
    Amount::parse(asset(asset_str), amount)
}

// ---------------------------------------------------------------------
// Byte-exact accepted values
// ---------------------------------------------------------------------

#[test]
fn one_satoshi() {
    let a = must_ok(parse("btc", "0.00000001"));
    assert_eq!(a.scale(), 8);
    assert_eq!(a.to_string(), "0.00000001");
}

#[test]
fn twenty_one_million_btc_in_satoshis() {
    // 21_000_000 * 10^8
    let a = must_ok(parse("btc-sats", "2100000000000000"));
    assert_eq!(a.scale(), 0);
    assert_eq!(a.to_string(), "2100000000000000");
}

#[test]
fn uint256_max_wei() {
    let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let a = must_ok(parse("wei", max));
    assert_eq!(a.scale(), 0);
    assert_eq!(a.to_string(), max);
}

#[test]
fn scale_two_cent_amount() {
    let a = must_ok(parse("usd", "0.01"));
    assert_eq!(a.scale(), 2);
    assert_eq!(a.to_string(), "0.01");
}

#[test]
fn teller_shaped_negative_amount() {
    let a = must_ok(parse("usd-checking", "-10.25"));
    assert_eq!(a.scale(), 2);
    assert_eq!(a.to_string(), "-10.25");
}

// ---------------------------------------------------------------------
// JSON wire shape
// ---------------------------------------------------------------------

#[test]
fn json_round_trip_string_amount() {
    let json = r#"{"asset":"usd-checking","amount":"-10.25"}"#;
    let a: Amount = must_ok(serde_json::from_str(json));
    assert_eq!(a.asset().as_str(), "usd-checking");
    assert_eq!(a.to_string(), "-10.25");

    let out = must_ok(serde_json::to_string(&a));
    assert_eq!(out, json);

    // and it deserializes again identically (full round trip)
    let a2: Amount = must_ok(serde_json::from_str(&out));
    assert!(a.same_repr(&a2));
}

#[test]
fn plaid_shaped_json_number_is_rejected() {
    // Plaid-style: amount as a bare JSON number, not a string.
    let json = r#"{"asset":"chase-checking","amount":-10.25}"#;
    let result: Result<Amount, _> = serde_json::from_str(json);
    assert!(
        result.is_err(),
        "a JSON number in `amount` must be rejected by serde"
    );
}

#[test]
fn json_unknown_field_is_rejected() {
    let json = r#"{"asset":"usd","amount":"1.00","currency":"USD"}"#;
    let result: Result<Amount, _> = serde_json::from_str(json);
    assert!(result.is_err(), "deny_unknown_fields must reject extras");
}

// ---------------------------------------------------------------------
// The serialize-after-arithmetic case (the exact bug the design review
// caught): a value that is legal at its own scale can become illegal to
// *represent* only after rescaling for arithmetic — and here it stays
// legal, and MUST still serialize and re-parse successfully.
// ---------------------------------------------------------------------

#[test]
fn serialize_after_arithmetic_near_the_boundary_stays_ok() {
    let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let a = must_ok(parse("wei", max)); // 78 digits, scale 0
    let tiny = must_ok(parse("wei", "0.00000000000000000000000000000000000001")); // scale 38, 1 digit

    let sum = must_ok(a.add(&tiny)); // rescaled to scale 38 -> 78 + 38 = 116 digits, <= 128: Ok
    assert_eq!(sum.scale(), 38);

    let expected = "115792089237316195423570985008687907853269984665640564039457584007913129639935\
         .00000000000000000000000000000000000001";
    assert_eq!(sum.to_string(), expected);

    // Must serialize and re-parse without error — this is the permanent
    // regression guard.
    let json = must_ok(serde_json::to_string(&sum));
    let reparsed: Amount = must_ok(serde_json::from_str(&json));
    assert!(sum.same_repr(&reparsed));

    let reparsed_from_display = must_ok(parse("wei", &sum.to_string()));
    assert!(sum.same_repr(&reparsed_from_display));
}

#[test]
fn coefficient_overflow_on_rescale_during_add() {
    // A 128-digit coefficient at scale 0 is legal on its own (exactly at
    // MAX_DIGITS), but adding any nonzero scale-38 amount forces a rescale
    // to scale 38, which would need 128 + 38 = 166 digits: over budget.
    let big_128_digits = format!("1{}", "0".repeat(127));
    assert_eq!(big_128_digits.len(), 128);

    let a = must_ok(parse("wei", &big_128_digits));
    let tiny = must_ok(parse("wei", "0.00000000000000000000000000000000000001"));

    let err = must_err(a.add(&tiny));
    assert_eq!(err, MoneyError::CoefficientOverflow);

    // sub hits the same rescale path.
    let err = must_err(a.sub(&tiny));
    assert_eq!(err, MoneyError::CoefficientOverflow);
}

// ---------------------------------------------------------------------
// The complete rejection table, each asserting its NAMED error variant.
// ---------------------------------------------------------------------

#[test]
fn rejection_table_amount_parse() {
    let cases: Vec<(&str, MoneyError)> = vec![
        ("", MoneyError::Empty),
        ("007.50", MoneyError::LeadingZero { at: 0 }),
        ("1e18", MoneyError::ExponentNotation { at: 1 }),
        ("1E-7", MoneyError::ExponentNotation { at: 1 }),
        (".5", MoneyError::MissingIntegerPart),
        ("5.", MoneyError::MissingFractionDigits),
        // `MissingFractionDigits` is truncation ONLY. A byte that is present but
        // is not a digit is reported at its offset, because an offset is what an
        // adapter author can act on. Found by proptest, kept as a fixed case so
        // the rule does not depend on a seed file surviving.
        ("100. 0", MoneyError::InvalidByte { at: 4 }),
        ("100.x", MoneyError::InvalidByte { at: 4 }),
        ("5.e3", MoneyError::ExponentNotation { at: 2 }),
        ("-0", MoneyError::SignOnZero),
        ("-0.00", MoneyError::SignOnZero),
        ("+5", MoneyError::InvalidByte { at: 0 }),
        ("1,000", MoneyError::InvalidByte { at: 1 }),
        (" 5", MoneyError::InvalidByte { at: 0 }),
        ("5 ", MoneyError::InvalidByte { at: 1 }),
        ("NaN", MoneyError::InvalidByte { at: 0 }),
        ("Infinity", MoneyError::InvalidByte { at: 0 }),
        (
            "\u{0661}\u{0662}\u{0663}",
            MoneyError::InvalidByte { at: 0 },
        ), // "١٢٣"
    ];

    for (input, expected) in cases {
        let err = must_err(parse("usd", input));
        assert_eq!(err, expected, "input = {input:?}");
    }
}

#[test]
fn rejection_table_too_many_digits() {
    let s = "9".repeat(129);
    let err = must_err(parse("usd", &s));
    assert_eq!(err, MoneyError::TooManyDigits { digits: 129 });
}

#[test]
fn rejection_table_scale_too_large() {
    let s = format!("0.{}", "1".repeat(39));
    let err = must_err(parse("usd", &s));
    assert_eq!(err, MoneyError::ScaleTooLarge { scale: 39 });
}

#[test]
fn rejection_table_input_too_long_checked_before_parsing() {
    // 193 bytes of digits: also over the digit budget, but the length
    // check runs first per the contract ("checked BEFORE parsing").
    let s = "9".repeat(193);
    let err = must_err(parse("usd", &s));
    assert_eq!(err, MoneyError::InputTooLong { len: 193 });
}

#[test]
fn rejection_table_asset_id() {
    assert_eq!(must_err_asset(AssetId::new("")), MoneyError::AssetIdEmpty);

    let long = "a".repeat(129);
    assert_eq!(
        must_err_asset(AssetId::new(&long)),
        MoneyError::AssetIdTooLong { len: 129 }
    );

    // space (0x20) is below the printable range 0x21..=0x7E
    assert_eq!(
        must_err_asset(AssetId::new("bad id")),
        MoneyError::AssetIdInvalidByte { at: 3 }
    );

    // DEL (0x7F) is above the printable range
    assert_eq!(
        must_err_asset(AssetId::new("id\u{7F}")),
        MoneyError::AssetIdInvalidByte { at: 2 }
    );
}

fn must_err_asset(r: Result<AssetId, MoneyError>) -> MoneyError {
    match r {
        Err(e) => e,
        Ok(v) => panic!("expected Err, got Ok({v:?})"),
    }
}

// ---------------------------------------------------------------------
// Value equality vs. representation, and Hash/Eq coherence.
// ---------------------------------------------------------------------

#[test]
fn value_equality_ignores_trailing_zero_scale() {
    let a = must_ok(parse("usd", "1.5"));
    let b = must_ok(parse("usd", "1.50"));
    assert_eq!(a, b, "1.5 and 1.50 are the same value");
    assert!(!a.same_repr(&b), "but they are NOT the same representation");
}

#[test]
fn hash_agrees_with_eq_across_scale_padding() {
    let a = must_ok(parse("usd", "1.5"));
    let b = must_ok(parse("usd", "1.50"));

    let mut map: HashMap<Amount, i32> = HashMap::new();
    map.insert(a.clone(), 1);
    map.insert(b.clone(), 2); // must overwrite the same logical entry

    assert_eq!(map.len(), 1, "1.5 and 1.50 must hash to one HashMap entry");
    assert_eq!(map.get(&a), Some(&2));
    assert_eq!(map.get(&b), Some(&2));
}

// ---------------------------------------------------------------------
// Display <-> parse bijection
// ---------------------------------------------------------------------

#[test]
fn display_output_reparses_to_the_same_representation() {
    let inputs = [
        "0",
        "0.0",
        "100",
        "-10.25",
        "0.00000001",
        "1.50",
        "1.5",
        "115792089237316195423570985008687907853269984665640564039457584007913129639935",
    ];
    for s in inputs {
        let a = must_ok(parse("usd", s));
        let shown = a.to_string();
        assert_eq!(shown, s, "Display must be byte-identical to the wire input");
        let reparsed = must_ok(parse("usd", &shown));
        assert!(
            a.same_repr(&reparsed),
            "re-parsing Display output must reproduce the same representation for {s:?}"
        );
    }
}

// ---------------------------------------------------------------------
// Cross-asset arithmetic
// ---------------------------------------------------------------------

#[test]
fn cross_asset_add_is_asset_mismatch() {
    let a = must_ok(parse("usd", "5"));
    let b = must_ok(parse("eur", "5"));
    assert_eq!(must_err(a.add(&b)), MoneyError::AssetMismatch);
    assert_eq!(must_err(a.sub(&b)), MoneyError::AssetMismatch);
    assert_eq!(a.cmp_same_asset(&b), Err(MoneyError::AssetMismatch));
}

// ---------------------------------------------------------------------
// Zero handling (hard rule 4): 5.0 + (-5.0) = 0.0, scale preserved from max.
// ---------------------------------------------------------------------

#[test]
fn zero_preserves_scale_from_the_operands() {
    let a = must_ok(parse("usd", "5.0"));
    let b = must_ok(parse("usd", "-5.0"));
    let sum = must_ok(a.add(&b));
    assert!(sum.is_zero());
    assert_eq!(sum.scale(), 1);
    assert_eq!(sum.to_string(), "0.0");
}
