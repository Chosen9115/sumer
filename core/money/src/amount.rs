//! Exact decimal amounts: `(asset, coefficient, scale)`, no floats.

use crate::{AssetId, MoneyError, MAX_DIGITS, MAX_INPUT_LEN, MAX_SCALE};
use num_bigint::BigInt;
use std::fmt;

/// An exact decimal amount tied to an asset.
///
/// Internally `units * 10^-scale` in the given `asset`. There is exactly one
/// fallible constructor, `Amount::checked`, which every other constructor
/// (parse, add, sub, neg, Deserialize) routes through (directly, or — for
/// `neg`, which cannot fail — by construction of an equivalent invariant).
/// That is what makes serialization infallible: every live `Amount` has a
/// coefficient with at most `MAX_DIGITS` significant digits and a scale of
/// at most `MAX_SCALE`, so it always has a `Display` string the grammar in
/// `Amount::parse` accepts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "AmountWire", into = "AmountWire")]
pub struct Amount {
    asset: AssetId,
    units: BigInt,
    scale: u8,
}

impl Amount {
    /// Builds an `Amount` from already-parsed fields, bypassing validation.
    /// Every caller of this function must independently prove the
    /// `checked` invariants (digit count, scale) already hold.
    fn new_unchecked(asset: AssetId, units: BigInt, scale: u8) -> Amount {
        Amount {
            asset,
            units,
            scale,
        }
    }

    /// The only validating constructor. Enforces `digits(units) <=
    /// MAX_DIGITS` and `scale <= MAX_SCALE`.
    fn checked(asset: AssetId, units: BigInt, scale: u8) -> Result<Amount, MoneyError> {
        // Order is normative: byte length (checked in `parse`), then digits,
        // then scale. An input violating both must report the same error in
        // every implementation, or two adapters reject it differently.
        let digits = digit_count(&units);
        if digits > MAX_DIGITS {
            return Err(MoneyError::TooManyDigits { digits });
        }
        if scale > MAX_SCALE {
            return Err(MoneyError::ScaleTooLarge {
                scale: usize::from(scale),
            });
        }
        Ok(Amount::new_unchecked(asset, units, scale))
    }

    /// Parses the canonical grammar `-?(0|[1-9][0-9]*)(\.[0-9]+)?`.
    ///
    /// Checks the byte length against `MAX_INPUT_LEN` before parsing.
    /// Byte-oriented and total: never panics, never indexes past the end,
    /// on any input including non-ASCII bytes.
    pub fn parse(asset: AssetId, s: &str) -> Result<Amount, MoneyError> {
        if s.len() > MAX_INPUT_LEN {
            return Err(MoneyError::InputTooLong { len: s.len() });
        }
        if s.is_empty() {
            return Err(MoneyError::Empty);
        }

        let bytes = s.as_bytes();
        let total = bytes.len();

        let (neg, rest) = match bytes.split_first() {
            Some((b'-', tail)) => (true, tail),
            _ => (false, bytes),
        };
        // Byte offset where `rest` begins: 0 with no sign, 1 with a sign.
        let int_start = total.saturating_sub(rest.len());

        let (int_digits, after_int): (&[u8], &[u8]) = match rest.split_first() {
            None => return Err(MoneyError::MissingIntegerPart),
            Some((b'.', _)) => return Err(MoneyError::MissingIntegerPart),
            Some((b'0', tail)) => {
                if tail.first().is_some_and(u8::is_ascii_digit) {
                    return Err(MoneyError::LeadingZero { at: int_start });
                }
                (&rest[..1], tail)
            }
            Some((d, _)) if d.is_ascii_digit() => {
                let count = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                (&rest[..count], &rest[count..])
            }
            Some(_) => return Err(MoneyError::InvalidByte { at: int_start }),
        };

        let (frac_digits, tail, scale_count): (&[u8], &[u8], usize) = match after_int.split_first()
        {
            None => (after_int, after_int, 0),
            Some((b'.', frac_rest)) => {
                let count = frac_rest.iter().take_while(|b| b.is_ascii_digit()).count();
                if count == 0 {
                    // `MissingFractionDigits` means truncation: the input ended
                    // after the point. A byte that is present but is not a digit
                    // gets reported where it sits, because an offset is what an
                    // adapter author can act on.
                    let at = total.saturating_sub(frac_rest.len());
                    return match frac_rest.first() {
                        None => Err(MoneyError::MissingFractionDigits),
                        Some(b'e' | b'E') => Err(MoneyError::ExponentNotation { at }),
                        Some(_) => Err(MoneyError::InvalidByte { at }),
                    };
                }
                (&frac_rest[..count], &frac_rest[count..], count)
            }
            Some((b'e' | b'E', _)) => {
                let at = total.saturating_sub(after_int.len());
                return Err(MoneyError::ExponentNotation { at });
            }
            Some(_) => {
                let at = total.saturating_sub(after_int.len());
                return Err(MoneyError::InvalidByte { at });
            }
        };

        // Anything left after the fraction digits is trailing garbage.
        if let Some(b) = tail.first() {
            let at = total.saturating_sub(tail.len());
            return if matches!(b, b'e' | b'E') {
                Err(MoneyError::ExponentNotation { at })
            } else {
                Err(MoneyError::InvalidByte { at })
            };
        }

        if neg {
            let is_zero_value = int_digits == b"0" && frac_digits.iter().all(|&b| b == b'0');
            if is_zero_value {
                return Err(MoneyError::SignOnZero);
            }
        }

        // `scale_count` is bounded by MAX_INPUT_LEN (checked above), so this
        // always fits in a u8; the fallible conversion is kept instead of an
        // `as` cast so this stays total rather than silently wrapping.
        let scale = u8::try_from(scale_count)
            .map_err(|_| MoneyError::ScaleTooLarge { scale: scale_count })?;

        let mut digit_bytes =
            Vec::with_capacity(int_digits.len().saturating_add(frac_digits.len()));
        digit_bytes.extend_from_slice(int_digits);
        digit_bytes.extend_from_slice(frac_digits);
        let magnitude = digits_to_bigint(&digit_bytes);
        let units = if neg { -magnitude } else { magnitude };

        Amount::checked(asset, units, scale)
    }

    /// The asset this amount is denominated in.
    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    /// The number of fraction digits.
    pub fn scale(&self) -> u8 {
        self.scale
    }

    /// Whether this amount's value is exactly zero.
    pub fn is_zero(&self) -> bool {
        self.units == BigInt::from(0)
    }

    /// Rescales both operands to `max(self.scale, other.scale)`, then adds.
    /// `Err(AssetMismatch)` if the assets differ. `Err(CoefficientOverflow)`
    /// if the rescaled sum has more than `MAX_DIGITS` significant digits —
    /// rescaling multiplies a coefficient by a power of ten and can push it
    /// past the limit even when both inputs were within it.
    pub fn add(&self, other: &Amount) -> Result<Amount, MoneyError> {
        if self.asset != other.asset {
            return Err(MoneyError::AssetMismatch);
        }
        let scale = self.scale.max(other.scale);
        let a = rescale_units(&self.units, self.scale, scale);
        let b = rescale_units(&other.units, other.scale, scale);
        // `parse` reports `TooManyDigits { digits }` because the count helps
        // when a single literal is too long; arithmetic reports the bare
        // `CoefficientOverflow`, per the wire contract.
        Amount::checked(self.asset.clone(), a + b, scale).map_err(|err| match err {
            MoneyError::TooManyDigits { .. } => MoneyError::CoefficientOverflow,
            other => other,
        })
    }

    /// `self - other`. Same rescale-then-bound-check semantics as `add`.
    pub fn sub(&self, other: &Amount) -> Result<Amount, MoneyError> {
        self.add(&other.neg())
    }

    /// Negation. Infallible: flipping the sign changes neither the
    /// coefficient's magnitude (so digit count is unchanged) nor the scale,
    /// so the `checked` invariants that already hold for `self` continue to
    /// hold without re-validation.
    pub fn neg(&self) -> Amount {
        Amount::new_unchecked(self.asset.clone(), -&self.units, self.scale)
    }

    /// Compares two amounts of the same asset. `Err(AssetMismatch)` if the
    /// assets differ.
    pub fn cmp_same_asset(&self, other: &Amount) -> Result<std::cmp::Ordering, MoneyError> {
        if self.asset != other.asset {
            return Err(MoneyError::AssetMismatch);
        }
        let scale = self.scale.max(other.scale);
        let a = rescale_units(&self.units, self.scale, scale);
        let b = rescale_units(&other.units, other.scale, scale);
        Ok(a.cmp(&b))
    }

    /// Exact representation equality: same asset, same coefficient, same
    /// scale. Distinct from the value-based `PartialEq` (`1.5 == 1.50` is
    /// true for `PartialEq` but false for `same_repr`). Test support only.
    #[doc(hidden)]
    pub fn same_repr(&self, other: &Amount) -> bool {
        self.asset == other.asset && self.units == other.units && self.scale == other.scale
    }

    /// `(units, scale)` with trailing zeros stripped from `units` and
    /// `scale` reduced to match — the canonical form two value-equal
    /// amounts share regardless of how they were written (`1.50` and `1.5`
    /// both normalize to `(15, 1)`; any zero normalizes to `(0, 0)`).
    ///
    /// This exists for `Hash` alone. Equality goes through
    /// `cmp_same_asset`, so there is exactly one comparison path.
    fn normalized(&self) -> (BigInt, u8) {
        let mut units = self.units.clone();
        let mut scale = self.scale;
        let ten = BigInt::from(10);
        let zero = BigInt::from(0);
        while scale > 0 {
            let remainder = &units % &ten;
            if remainder != zero {
                break;
            }
            units = &units / &ten;
            scale = scale.saturating_sub(1);
        }
        (units, scale)
    }
}

/// Counts the significant decimal digits of `n`'s magnitude, exactly (no
/// floating point). `to_str_radix` never emits a sign or leading zeros
/// (except the single digit "0" for zero itself), so its length is exactly
/// the significant digit count — including the zero case, which is 1
/// digit, not 0.
fn digit_count(n: &BigInt) -> usize {
    n.magnitude().to_str_radix(10).len()
}

/// `10^exp` as a `BigInt`.
fn pow10(exp: u8) -> BigInt {
    BigInt::from(10).pow(u32::from(exp))
}

/// Multiplies `units` by `10^(to_scale - from_scale)`. Callers must ensure
/// `to_scale >= from_scale` (true whenever `to_scale` is a `max` of scales
/// including `from_scale`).
fn rescale_units(units: &BigInt, from_scale: u8, to_scale: u8) -> BigInt {
    if from_scale == to_scale {
        return units.clone();
    }
    let diff = to_scale.saturating_sub(from_scale);
    units * pow10(diff)
}

/// Parses a run of ASCII digit bytes (`b'0'..=b'9'`, already validated by
/// the caller) into its magnitude as a `BigInt`.
fn digits_to_bigint(digits: &[u8]) -> BigInt {
    let ten = BigInt::from(10);
    let mut acc = BigInt::from(0);
    for &b in digits {
        let digit = BigInt::from(b.wrapping_sub(b'0'));
        acc = &acc * &ten + &digit;
    }
    acc
}

/// Renders `units * 10^-scale` as the canonical decimal string: optional
/// `-`, then digits, with a `.` inserted `scale` places from the right
/// (padding with a leading `0.` and zeros when the coefficient has fewer
/// digits than the scale). This is the one and only formatting function;
/// `Display` calls it and nothing else, so the wire string and the printed
/// string are always byte-identical.
fn format_decimal(units: &BigInt, scale: u8) -> String {
    let neg = units < &BigInt::from(0);
    let magnitude = units.magnitude().to_str_radix(10);
    let scale = usize::from(scale);

    let mut out = String::with_capacity(magnitude.len().saturating_add(2));
    if neg {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&magnitude);
    } else if magnitude.len() > scale {
        let split = magnitude.len().saturating_sub(scale);
        out.push_str(&magnitude[..split]);
        out.push('.');
        out.push_str(&magnitude[split..]);
    } else {
        out.push_str("0.");
        let pad = scale.saturating_sub(magnitude.len());
        for _ in 0..pad {
            out.push('0');
        }
        out.push_str(&magnitude);
    }
    out
}

impl PartialEq for Amount {
    /// Value-based equality: `1.5 == 1.50` is true for amounts of the same
    /// asset. Amounts of different assets are never equal.
    fn eq(&self, other: &Self) -> bool {
        // One comparison path, so `Eq` and `PartialOrd` cannot drift apart.
        self.cmp_same_asset(other) == Ok(std::cmp::Ordering::Equal)
    }
}

impl Eq for Amount {}

impl std::hash::Hash for Amount {
    /// Hashes the normalized form, so `Hash` agrees with the value-based
    /// `Eq` (`hash(1.5) == hash(1.50)`).
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let (units, scale) = self.normalized();
        self.asset.hash(state);
        units.hash(state);
        scale.hash(state);
    }
}

impl PartialOrd for Amount {
    /// `None` when the assets differ — there is no total order across
    /// assets. `Ord` is intentionally not implemented.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.cmp_same_asset(other).ok()
    }
}

impl fmt::Display for Amount {
    /// Writes only the decimal string, byte-identical to the wire `amount`
    /// field.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_decimal(&self.units, self.scale))
    }
}

/// Wire shape for `Amount`: `{"asset": "...", "amount": "-10.25"}`, both
/// fields strings. A JSON number in `amount` fails with serde's own
/// invalid-type error before any Sumer code runs.
#[derive(serde::Serialize)]
struct AmountWire {
    asset: String,
    amount: String,
}

/// Hand-written so the wire shape is exactly the object documented in
/// `spec/money.md`. A derived `Deserialize` also accepts a positional array
/// (`["usd","1.00"]`) because serde generates a `visit_seq`, and
/// `deny_unknown_fields` does not suppress it — that would let a Rust adapter
/// accept frames another language's adapter rejects.
impl<'de> serde::Deserialize<'de> for AmountWire {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{Error, MapAccess, Visitor};

        struct WireVisitor;

        impl<'de> Visitor<'de> for WireVisitor {
            type Value = AmountWire;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with string fields `asset` and `amount`")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<AmountWire, A::Error> {
                let mut asset: Option<String> = None;
                let mut amount: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "asset" if asset.is_some() => {
                            return Err(A::Error::duplicate_field("asset"))
                        }
                        "amount" if amount.is_some() => {
                            return Err(A::Error::duplicate_field("amount"))
                        }
                        "asset" => asset = Some(map.next_value()?),
                        "amount" => amount = Some(map.next_value()?),
                        other => return Err(A::Error::unknown_field(other, &["asset", "amount"])),
                    }
                }
                Ok(AmountWire {
                    asset: asset.ok_or_else(|| A::Error::missing_field("asset"))?,
                    amount: amount.ok_or_else(|| A::Error::missing_field("amount"))?,
                })
            }
        }

        d.deserialize_map(WireVisitor)
    }
}

impl TryFrom<AmountWire> for Amount {
    type Error = MoneyError;

    fn try_from(wire: AmountWire) -> Result<Self, MoneyError> {
        let asset = AssetId::new(&wire.asset)?;
        Amount::parse(asset, &wire.amount)
    }
}

impl From<Amount> for AmountWire {
    fn from(amount: Amount) -> AmountWire {
        AmountWire {
            asset: amount.asset.as_str().to_owned(),
            amount: amount.to_string(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn amt(asset: &str, s: &str) -> Amount {
        Amount::parse(AssetId::new(asset).unwrap(), s).unwrap()
    }

    // --- digit counting ---

    #[test]
    fn digit_count_of_zero_is_one() {
        assert_eq!(digit_count(&BigInt::from(0)), 1);
    }

    #[test]
    fn digit_count_ignores_sign() {
        assert_eq!(digit_count(&BigInt::from(-12345)), 5);
        assert_eq!(digit_count(&BigInt::from(12345)), 5);
    }

    #[test]
    fn digit_count_of_large_number() {
        let s = "9".repeat(200);
        let n = digits_to_bigint(s.as_bytes());
        assert_eq!(digit_count(&n), 200);
    }

    // --- normalization ---

    #[test]
    fn normalizes_trailing_zeros() {
        let a = amt("USD", "1.50");
        assert_eq!(a.normalized(), (BigInt::from(15), 1));
    }

    #[test]
    fn normalizes_zero_to_scale_zero() {
        let a = amt("USD", "0.000");
        assert_eq!(a.normalized(), (BigInt::from(0), 0));
    }

    #[test]
    fn normalizes_integer_unchanged() {
        let a = amt("USD", "100");
        assert_eq!(a.normalized(), (BigInt::from(100), 0));
    }

    #[test]
    fn different_assets_never_equal() {
        let a = amt("USD", "1.5");
        let b = amt("EUR", "1.5");
        assert_ne!(a, b);
        assert_eq!(a.partial_cmp(&b), None);
        assert_eq!(a.cmp_same_asset(&b), Err(MoneyError::AssetMismatch));
    }

    // --- rescaling ---

    #[test]
    fn rescale_multiplies_by_power_of_ten() {
        let units = BigInt::from(5);
        assert_eq!(rescale_units(&units, 0, 2), BigInt::from(500));
        assert_eq!(rescale_units(&units, 2, 2), BigInt::from(5));
    }

    #[test]
    fn pow10_boundary() {
        assert_eq!(pow10(0), BigInt::from(1));
        assert_eq!(pow10(3), BigInt::from(1000));
    }

    // --- MAX_DIGITS boundary ---

    #[test]
    fn accepts_exactly_max_digits() {
        let s = "9".repeat(MAX_DIGITS);
        assert!(Amount::parse(AssetId::new("USD").unwrap(), &s).is_ok());
    }

    #[test]
    fn rejects_one_over_max_digits() {
        let s = "9".repeat(MAX_DIGITS + 1);
        let err = Amount::parse(AssetId::new("USD").unwrap(), &s).unwrap_err();
        assert_eq!(
            err,
            MoneyError::TooManyDigits {
                digits: MAX_DIGITS + 1
            }
        );
    }

    #[test]
    fn add_within_bound_succeeds_even_with_many_digits() {
        // 78-digit coefficient (uint256::MAX magnitude) at scale 0 rescaled
        // against a scale-38 amount produces 116 digits -- still Ok, since
        // 116 <= MAX_DIGITS (128).
        let uint256_max_wei =
            "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert_eq!(uint256_max_wei.len(), 78);
        let a = amt("USD", uint256_max_wei);
        let b = amt("USD", &format!("0.{}", "1".repeat(38)));
        let sum = a.add(&b).expect("116 digits is within MAX_DIGITS");
        assert_eq!(digit_count(&sum.units), 116);
    }

    // --- add/sub/neg semantics ---

    #[test]
    fn add_rescales_to_max_scale() {
        let a = amt("USD", "5.0");
        let b = amt("USD", "-5.0");
        let sum = a.add(&b).unwrap();
        assert_eq!(sum.scale(), 1);
        assert!(sum.is_zero());
        assert_eq!(sum.to_string(), "0.0");
    }

    #[test]
    fn sub_is_add_of_negation() {
        let a = amt("USD", "10.25");
        let b = amt("USD", "0.25");
        assert_eq!(a.sub(&b).unwrap(), amt("USD", "10.00"));
    }

    #[test]
    fn neg_preserves_scale_and_digits() {
        let a = amt("USD", "10.25");
        let n = a.neg();
        assert_eq!(n.scale(), a.scale());
        assert_eq!(n.to_string(), "-10.25");
        assert_eq!(n.neg().to_string(), "10.25");
    }

    // --- Display / parse round trip ---

    // --- parse rejection table ---

    // --- serde ---
}
