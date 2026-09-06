//! Property-based tests for `sumer-money`, written independently against
//! `spec/money.md` / the frozen PR-1 API contract — not against the sibling
//! implementation. Generators are deliberately hostile: they bias toward the
//! 128-digit / 38-scale boundaries rather than only producing small values.

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use num_bigint::{BigInt, Sign};
use proptest::prelude::*;

use sumer_money::{Amount, AssetId, MoneyError, MAX_DIGITS, MAX_SCALE};

// =====================================================================
// Small shared helpers
// =====================================================================

fn must_ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

fn asset(s: &str) -> AssetId {
    must_ok(AssetId::new(s))
}

fn asset_amount(asset_str: &str, amount_str: &str) -> Result<Amount, MoneyError> {
    Amount::parse(asset(asset_str), amount_str)
}

fn digits_str(digits: &[u8]) -> String {
    digits.iter().map(|d| char::from(b'0' + d)).collect()
}

fn sign_str(neg: bool) -> &'static str {
    if neg {
        "-"
    } else {
        ""
    }
}

/// Count of significant digits in a canonical decimal string (sign and dot
/// excluded) — matches the contract's "digits(units)" notion.
fn digit_count(s: &str) -> usize {
    s.chars().filter(char::is_ascii_digit).count()
}

fn assert_within_bounds(a: &Amount) {
    assert!(
        a.scale() <= MAX_SCALE,
        "scale {} exceeds MAX_SCALE {}",
        a.scale(),
        MAX_SCALE
    );
    let digits = digit_count(&a.to_string());
    assert!(
        digits <= MAX_DIGITS,
        "digits {digits} exceeds MAX_DIGITS {MAX_DIGITS}"
    );
}

// =====================================================================
// Valid-amount generator: a (sign, digits, scale) triple, hostile toward
// the 128-digit / 38-scale boundaries, mapped into a canonical string.
// =====================================================================

/// digits[0..int_len] is the integer part, digits[int_len..] the fraction
/// part, where int_len = digits.len() - scale. Guarantees: no redundant
/// leading zero, no sign on an all-zero value.
fn valid_triplet() -> impl Strategy<Value = (bool, Vec<u8>, usize)> {
    let d_strategy = prop_oneof![
        3 => 1usize..=12usize,
        2 => 118usize..=128usize,
        5 => 1usize..=128usize,
    ];
    d_strategy
        .prop_flat_map(|d| {
            let max_scale = d.saturating_sub(1).min(usize::from(MAX_SCALE));
            let near_boundary_lo = max_scale.saturating_sub(3);
            let scale_strategy = prop_oneof![
                2 => 0usize..=max_scale,
                1 => near_boundary_lo..=max_scale,
            ];
            (Just(d), scale_strategy)
        })
        .prop_flat_map(|(d, scale)| {
            (
                Just(d),
                Just(scale),
                proptest::collection::vec(0u8..=9u8, d),
                any::<bool>(),
            )
        })
        .prop_map(|(d, scale, mut digits, sign)| {
            let int_len = d - scale;
            if int_len > 1 && digits[0] == 0 {
                digits[0] = 1;
            }
            let all_zero = digits.iter().all(|&x| x == 0);
            (sign && !all_zero, digits, scale)
        })
}

fn build_canonical(sign: bool, digits: &[u8], scale: usize) -> String {
    let ds = digits_str(digits);
    let int_len = ds.len() - scale;
    let (int_part, frac_part) = ds.split_at(int_len);
    if scale == 0 {
        format!("{}{int_part}", sign_str(sign))
    } else {
        format!("{}{int_part}.{frac_part}", sign_str(sign))
    }
}

fn canonical_amount_string() -> impl Strategy<Value = String> {
    valid_triplet().prop_map(|(sign, digits, scale)| build_canonical(sign, &digits, scale))
}

fn valid_integer_part() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("0".to_string()),
        (1u8..=9u8, proptest::collection::vec(0u8..=9u8, 0..=15))
            .prop_map(|(first, rest)| format!("{}{}", char::from(b'0' + first), digits_str(&rest))),
    ]
}

// =====================================================================
// Independent (non-crate) decimal <-> BigInt oracle, used only for the
// exactness property. This is deliberately a separate, from-scratch
// implementation so it cannot share a bug with the crate under test.
// =====================================================================

fn independent_bigint(s: &str) -> (bool, BigInt, u32) {
    let negative = s.starts_with('-');
    let unsigned = if negative { &s[1..] } else { s };
    let (int_part, frac_part) = match unsigned.split_once('.') {
        Some((i, f)) => (i, f),
        None => (unsigned, ""),
    };
    let scale = u32::try_from(frac_part.len()).unwrap_or(u32::MAX);
    let combined = format!("{int_part}{frac_part}");
    let magnitude = BigInt::parse_bytes(combined.as_bytes(), 10).unwrap_or_else(|| BigInt::from(0));
    (negative, magnitude, scale)
}

/// 10^exp as a `BigInt`, via repeated multiplication rather than relying on
/// a `pow` method whose exact trait/import path varies across num-bigint
/// versions — `exp` here is always <= MAX_SCALE (38), so the loop is cheap.
fn pow10(exp: u32) -> BigInt {
    let ten = BigInt::from(10);
    let mut result = BigInt::from(1);
    for _ in 0..exp {
        result *= &ten;
    }
    result
}

fn format_canonical(sign_negative: bool, magnitude: &BigInt, scale: u32) -> String {
    let mag_str = magnitude.to_str_radix(10);
    let is_zero = mag_str == "0";
    let scale_usize = usize::try_from(scale).unwrap_or(usize::MAX);
    let digits = if mag_str.len() <= scale_usize {
        let pad = scale_usize + 1 - mag_str.len();
        format!("{}{mag_str}", "0".repeat(pad))
    } else {
        mag_str
    };
    let sign = if sign_negative && !is_zero { "-" } else { "" };
    if scale_usize == 0 {
        format!("{sign}{digits}")
    } else {
        let split_at = digits.len() - scale_usize;
        let (int_part, frac_part) = digits.split_at(split_at);
        format!("{sign}{int_part}.{frac_part}")
    }
}

// =====================================================================
// Property 1 — representation round-trip (`same_repr`, not `==`).
// =====================================================================

proptest! {
    #[test]
    fn prop01_representation_round_trip(s in canonical_amount_string()) {
        let a = must_ok(asset_amount("usd", &s));
        let shown = a.to_string();
        let reparsed = must_ok(asset_amount("usd", &shown));
        prop_assert!(a.same_repr(&reparsed));
    }
}

// =====================================================================
// Property 2 — string bijection: parse(s).to_string() == s.
// =====================================================================

proptest! {
    #[test]
    fn prop02_string_bijection(s in canonical_amount_string()) {
        let a = must_ok(asset_amount("usd", &s));
        prop_assert_eq!(a.to_string(), s);
    }
}

// =====================================================================
// Property 3 — JSON round-trip; every `{"amount": <number>}` rejected.
// =====================================================================

fn json_number_literal() -> impl Strategy<Value = String> {
    (
        any::<bool>(),
        0u64..=1_000_000_000u64,
        proptest::option::of(1u32..=6u32),
        proptest::option::of((any::<bool>(), 1u32..=5u32)),
    )
        .prop_map(|(neg, int_val, frac_digits, exp)| {
            let mut s = String::new();
            if neg {
                s.push('-');
            }
            s.push_str(&int_val.to_string());
            if let Some(fd) = frac_digits {
                s.push('.');
                s.push_str(&"3".repeat(usize::try_from(fd).unwrap_or(0)));
            }
            if let Some((eneg, edig)) = exp {
                s.push('e');
                if eneg {
                    s.push('-');
                }
                s.push_str(&edig.to_string());
            }
            s
        })
}

proptest! {
    #[test]
    fn prop03_json_round_trip(s in canonical_amount_string()) {
        let a = must_ok(asset_amount("usd", &s));
        let json = must_ok(serde_json::to_string(&a));
        let back: Amount = must_ok(serde_json::from_str(&json));
        prop_assert!(a.same_repr(&back));
    }

    #[test]
    fn prop03b_json_number_amount_always_rejected(num_str in json_number_literal()) {
        let json = format!(r#"{{"asset":"usd","amount":{num_str}}}"#);
        let result: Result<Amount, _> = serde_json::from_str(&json);
        prop_assert!(result.is_err(), "json = {json}");
    }
}

// =====================================================================
// Property 4 — exactness: Ok(add) equals an independent BigInt sum at
// max scale; (a+b)-b == a by value.
// =====================================================================

proptest! {
    #[test]
    fn prop04_add_is_exact_and_invertible(
        a_str in canonical_amount_string(),
        b_str in canonical_amount_string(),
    ) {
        let a = must_ok(asset_amount("x", &a_str));
        let b = must_ok(asset_amount("x", &b_str));

        let (neg_a, mag_a, scale_a) = independent_bigint(&a_str);
        let (neg_b, mag_b, scale_b) = independent_bigint(&b_str);
        let common_scale = scale_a.max(scale_b);

        let va: BigInt = if neg_a { -mag_a } else { mag_a };
        let vb: BigInt = if neg_b { -mag_b } else { mag_b };
        // rescale to common_scale before summing, mirroring "rescale then add"
        let va = va * pow10(common_scale - scale_a);
        let vb = vb * pow10(common_scale - scale_b);
        let total = va + vb;

        let total_negative = total.sign() == Sign::Minus;
        let magnitude_result: BigInt = if total_negative { -total } else { total };
        let expected_digits = magnitude_result.to_str_radix(10).len();

        let actual = a.add(&b);
        if expected_digits <= MAX_DIGITS {
            let sum = must_ok(actual);
            let expected_str = format_canonical(total_negative, &magnitude_result, common_scale);
            prop_assert_eq!(sum.to_string(), expected_str);

            let back = must_ok(sum.sub(&b));
            prop_assert_eq!(back, a);
        } else {
            prop_assert_eq!(actual, Err(MoneyError::CoefficientOverflow));
        }
    }
}

// =====================================================================
// Property 5 — commutativity and associativity of add.
// =====================================================================

proptest! {
    #[test]
    fn prop05_add_commutative_and_associative(
        a_str in canonical_amount_string(),
        b_str in canonical_amount_string(),
        c_str in canonical_amount_string(),
    ) {
        let a = must_ok(asset_amount("x", &a_str));
        let b = must_ok(asset_amount("x", &b_str));
        let c = must_ok(asset_amount("x", &c_str));

        match (a.add(&b), b.add(&a)) {
            (Ok(v1), Ok(v2)) => prop_assert_eq!(v1, v2),
            (Err(e1), Err(e2)) => prop_assert_eq!(e1, e2),
            (r1, r2) => prop_assert!(false, "commutativity broke: {:?} vs {:?}", r1, r2),
        }

        let left = a.add(&b).and_then(|ab| ab.add(&c));
        let right = b.add(&c).and_then(|bc| a.add(&bc));
        if let (Ok(l), Ok(r)) = (&left, &right) {
            prop_assert_eq!(l, r);
        }
    }
}

// =====================================================================
// Property 6 — cross-asset arithmetic always errors, never a value.
// =====================================================================

proptest! {
    #[test]
    fn prop06_cross_asset_always_mismatch(
        a_str in canonical_amount_string(),
        b_str in canonical_amount_string(),
    ) {
        let a = must_ok(asset_amount("asset-a", &a_str));
        let b = must_ok(asset_amount("asset-b", &b_str));
        prop_assert_eq!(a.add(&b), Err(MoneyError::AssetMismatch));
        prop_assert_eq!(a.sub(&b), Err(MoneyError::AssetMismatch));
        prop_assert_eq!(a.cmp_same_asset(&b), Err(MoneyError::AssetMismatch));
    }
}

// =====================================================================
// Property 7 — every malformed generator maps to its named variant.
// Never a panic (proptest fails the case if one occurs), never Ok.
// =====================================================================

fn malformed_exponent() -> impl Strategy<Value = (String, MoneyError)> {
    (
        any::<bool>(),
        1u64..=999_999u64,
        proptest::sample::select(vec!['e', 'E']),
        any::<bool>(),
        1u32..=5u32,
    )
        .prop_map(|(neg, n, e_char, neg_exp, exp)| {
            let mantissa = format!("{}{n}", sign_str(neg));
            let at = mantissa.len();
            let s = format!("{mantissa}{e_char}{}{exp}", sign_str(neg_exp));
            (s, MoneyError::ExponentNotation { at })
        })
}

fn malformed_leading_zero() -> impl Strategy<Value = (String, MoneyError)> {
    (
        1u8..=9u8,
        proptest::collection::vec(0u8..=9u8, 0..=10),
        proptest::option::of(proptest::collection::vec(0u8..=9u8, 1..=10)),
    )
        .prop_map(|(first, rest, frac)| {
            let int_part = format!("0{}{}", char::from(b'0' + first), digits_str(&rest));
            let s = match frac {
                Some(f) => format!("{int_part}.{}", digits_str(&f)),
                None => int_part,
            };
            (s, MoneyError::LeadingZero { at: 0 })
        })
}

fn malformed_missing_integer_part() -> impl Strategy<Value = (String, MoneyError)> {
    (any::<bool>(), proptest::collection::vec(0u8..=9u8, 1..=10)).prop_map(|(neg, frac)| {
        let s = format!("{}.{}", sign_str(neg), digits_str(&frac));
        (s, MoneyError::MissingIntegerPart)
    })
}

fn malformed_missing_fraction_digits() -> impl Strategy<Value = (String, MoneyError)> {
    (any::<bool>(), valid_integer_part()).prop_map(|(neg, int_part)| {
        let s = format!("{}{int_part}.", sign_str(neg));
        (s, MoneyError::MissingFractionDigits)
    })
}

fn malformed_sign_on_zero() -> impl Strategy<Value = (String, MoneyError)> {
    proptest::collection::vec(Just(0u8), 0..=10).prop_map(|zeros| {
        let s = if zeros.is_empty() {
            "-0".to_string()
        } else {
            format!("-0.{}", digits_str(&zeros))
        };
        (s, MoneyError::SignOnZero)
    })
}

fn malformed_separator() -> impl Strategy<Value = (String, MoneyError)> {
    (1u8..=9u8, proptest::collection::vec(0u8..=9u8, 3..=7))
        .prop_flat_map(|(first, rest)| {
            let digits = format!("{}{}", char::from(b'0' + first), digits_str(&rest));
            let len = digits.len();
            (Just(digits), 1..len)
        })
        .prop_map(|(digits, idx)| {
            let mut s = digits;
            s.insert(idx, ',');
            (s, MoneyError::InvalidByte { at: idx })
        })
}

fn malformed_whitespace() -> impl Strategy<Value = (String, MoneyError)> {
    canonical_amount_string()
        .prop_flat_map(|s| {
            let len = s.len();
            (Just(s), 0..=len)
        })
        .prop_map(|(s, idx)| {
            let mut bytes = s.into_bytes();
            bytes.insert(idx, b' ');
            let s = String::from_utf8(bytes).unwrap_or_default();
            (s, MoneyError::InvalidByte { at: idx })
        })
}

fn malformed_non_ascii_digits() -> impl Strategy<Value = (String, MoneyError)> {
    valid_integer_part().prop_map(|int_part| {
        let converted: String = int_part
            .chars()
            .map(|c| {
                let d = c.to_digit(10).unwrap_or(0);
                char::from_u32(0x0660 + d).unwrap_or('?')
            })
            .collect();
        (converted, MoneyError::InvalidByte { at: 0 })
    })
}

fn malformed_over_long() -> impl Strategy<Value = (String, MoneyError)> {
    (193usize..=300usize).prop_map(|len| ("9".repeat(len), MoneyError::InputTooLong { len }))
}

fn malformed_over_scale() -> impl Strategy<Value = (String, MoneyError)> {
    (39usize..=90usize).prop_map(|scale| {
        (
            format!("0.{}", "1".repeat(scale)),
            MoneyError::ScaleTooLarge { scale },
        )
    })
}

fn malformed_too_many_digits() -> impl Strategy<Value = (String, MoneyError)> {
    (129usize..=192usize)
        .prop_map(|digits| ("9".repeat(digits), MoneyError::TooManyDigits { digits }))
}

fn malformed_any() -> impl Strategy<Value = (String, MoneyError)> {
    prop_oneof![
        malformed_exponent(),
        malformed_leading_zero(),
        malformed_missing_integer_part(),
        malformed_missing_fraction_digits(),
        malformed_sign_on_zero(),
        malformed_separator(),
        malformed_whitespace(),
        malformed_non_ascii_digits(),
        malformed_over_long(),
        malformed_over_scale(),
        malformed_too_many_digits(),
    ]
}

proptest! {
    #[test]
    fn prop07_malformed_maps_to_named_variant((s, expected) in malformed_any()) {
        let result = asset_amount("usd", &s);
        prop_assert_eq!(result, Err(expected));
    }
}

// =====================================================================
// Property 8 — Eq/Hash coherence on scale-padded pairs.
// =====================================================================

fn scale_padded_pair() -> impl Strategy<Value = (String, String)> {
    valid_triplet()
        .prop_flat_map(|(sign, digits, scale)| {
            let d = digits.len();
            let max_k = (usize::from(MAX_SCALE) - scale).min(MAX_DIGITS - d);
            (Just((sign, digits, scale)), 0..=max_k)
        })
        .prop_map(|((sign, digits, scale), k)| {
            let s1 = build_canonical(sign, &digits, scale);
            let mut digits2 = digits;
            digits2.extend(std::iter::repeat_n(0, k));
            let s2 = build_canonical(sign, &digits2, scale + k);
            (s1, s2)
        })
}

proptest! {
    #[test]
    fn prop08_hash_eq_coherence_on_scale_padding((s1, s2) in scale_padded_pair()) {
        let a = must_ok(asset_amount("usd", &s1));
        let b = must_ok(asset_amount("usd", &s2));
        prop_assert_eq!(&a, &b);

        let mut h1 = DefaultHasher::new();
        a.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        b.hash(&mut h2);
        prop_assert_eq!(h1.finish(), h2.finish());
    }
}

// =====================================================================
// Property 9 — cmp_same_asset is a total order; a < b iff (a-b) negative.
// =====================================================================

proptest! {
    #[test]
    fn prop09_total_order_and_sign_relation(
        a_str in canonical_amount_string(),
        b_str in canonical_amount_string(),
        c_str in canonical_amount_string(),
    ) {
        let a = must_ok(asset_amount("usd", &a_str));
        let b = must_ok(asset_amount("usd", &b_str));
        let c = must_ok(asset_amount("usd", &c_str));

        let ab = must_ok(a.cmp_same_asset(&b));
        let ba = must_ok(b.cmp_same_asset(&a));
        prop_assert_eq!(ab, ba.reverse());

        let bc = must_ok(b.cmp_same_asset(&c));
        let ac = must_ok(a.cmp_same_asset(&c));
        if ab != Ordering::Greater && bc != Ordering::Greater {
            prop_assert_ne!(ac, Ordering::Greater);
        }

        if let Ok(diff) = a.sub(&b) {
            let is_negative = diff.to_string().starts_with('-');
            prop_assert_eq!(ab == Ordering::Less, is_negative);
            prop_assert_eq!(diff.is_zero(), ab == Ordering::Equal);
        }
    }
}

// =====================================================================
// Property 10 — constructor invariant: every Ok from any public path
// satisfies digits <= MAX_DIGITS and scale <= MAX_SCALE.
// =====================================================================

proptest! {
    #[test]
    fn prop10_constructor_invariant_holds_everywhere(
        a_str in canonical_amount_string(),
        b_str in canonical_amount_string(),
    ) {
        let a = must_ok(asset_amount("usd", &a_str));
        let b = must_ok(asset_amount("usd", &b_str));
        assert_within_bounds(&a);
        assert_within_bounds(&b);
        assert_within_bounds(&a.neg());
        if let Ok(sum) = a.add(&b) {
            assert_within_bounds(&sum);
        }
        if let Ok(diff) = a.sub(&b) {
            assert_within_bounds(&diff);
        }
    }
}

// =====================================================================
// Property 11 — serialize-after-arithmetic: for every Ok from add/sub,
// parse(result.to_string()) succeeds (and so does a JSON round trip).
// =====================================================================

proptest! {
    #[test]
    fn prop11_serialize_after_arithmetic(
        a_str in canonical_amount_string(),
        b_str in canonical_amount_string(),
    ) {
        let a = must_ok(asset_amount("usd", &a_str));
        let b = must_ok(asset_amount("usd", &b_str));

        if let Ok(sum) = a.add(&b) {
            prop_assert!(asset_amount("usd", &sum.to_string()).is_ok());
            let json = serde_json::to_string(&sum);
            prop_assert!(json.is_ok());
            if let Ok(j) = json {
                let back: Result<Amount, _> = serde_json::from_str(&j);
                prop_assert!(back.is_ok());
            }
        }
        if let Ok(diff) = a.sub(&b) {
            prop_assert!(asset_amount("usd", &diff.to_string()).is_ok());
        }
    }
}
