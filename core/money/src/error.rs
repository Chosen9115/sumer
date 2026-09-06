//! Error variants for `sumer-money`.
//!
//! Rule (contract hard rule 2): errors never carry the rejected payload —
//! only a class plus a byte offset or a count. These flow to adapter stderr
//! and the wider plan requires secret redaction, so the *value* a caller
//! typed must never appear in a `MoneyError`.

use thiserror::Error;

/// Everything that can go wrong constructing an `Amount` or an `AssetId`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MoneyError {
    #[error("amount string is empty")]
    Empty,

    #[error(
        "amount string is {len} bytes, exceeds the {max}-byte input limit",
        max = crate::MAX_INPUT_LEN
    )]
    InputTooLong { len: usize },

    #[error("invalid byte at offset {at}")]
    InvalidByte { at: usize },

    #[error("leading zero not allowed at offset {at}")]
    LeadingZero { at: usize },

    #[error("sign not allowed on a zero value")]
    SignOnZero,

    #[error("missing integer part before the decimal point")]
    MissingIntegerPart,

    #[error("missing fraction digits after the decimal point")]
    MissingFractionDigits,

    // The fix differs per language because each one's default decimal
    // formatter emits exponent notation for legitimate values: Python's
    // str(Decimal) can print "1E-7", Java's BigDecimal.toString() likewise,
    // Go's big.Float.Text('e', ...) if misused, and JS decimal.js's
    // .toString(). Naming all four fixes here means the person wiring up an
    // adapter in any of those languages gets the answer without having to
    // ask.
    #[error(
        "exponent notation at offset {at} is not allowed; format the decimal as plain \
         digits before encoding it (Python: format(d, 'f'), not str(d); \
         Java: toPlainString(), not toString(); Go: Text('f', -1); \
         JS decimal.js: .toFixed())"
    )]
    ExponentNotation { at: usize },

    #[error(
        "coefficient has {digits} significant digits, exceeds the {max}-digit limit",
        max = crate::MAX_DIGITS
    )]
    TooManyDigits { digits: usize },

    #[error(
        "scale {scale} exceeds the maximum scale of {max}",
        max = crate::MAX_SCALE
    )]
    ScaleTooLarge { scale: usize },

    #[error(
        "coefficient overflow: result exceeds the {max}-digit limit",
        max = crate::MAX_DIGITS
    )]
    CoefficientOverflow,

    #[error("asset mismatch between operands")]
    AssetMismatch,

    #[error("asset id is empty")]
    AssetIdEmpty,

    #[error(
        "asset id is {len} bytes, exceeds the {max}-byte limit",
        max = crate::MAX_ASSET_ID_LEN
    )]
    AssetIdTooLong { len: usize },

    #[error("asset id has invalid byte at offset {at}")]
    AssetIdInvalidByte { at: usize },
}
