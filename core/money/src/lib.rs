//! `sumer-money`: exact, arbitrary-precision decimal amounts tied to an asset id.
//!
//! No floats anywhere. Every amount is a `(sign, coefficient, scale)` decimal,
//! parsed from and printed to a byte-exact grammar (see `Amount::parse` and
//! the `Display` impl). See the crate's frozen API contract for the grammar
//! and rejection table.

#![forbid(unsafe_code)]

/// Largest allowed scale (number of fraction digits) of an `Amount`.
pub const MAX_SCALE: u8 = 38;
/// Largest allowed count of significant decimal digits in an `Amount`'s coefficient.
pub const MAX_DIGITS: usize = 128;
/// Largest allowed byte length of an `AssetId`.
pub const MAX_ASSET_ID_LEN: usize = 128;
/// Largest allowed byte length of a string passed to `Amount::parse`, checked
/// before any parsing happens.
pub const MAX_INPUT_LEN: usize = 192;

mod amount;
mod asset;
mod error;

pub use amount::Amount;
pub use asset::AssetId;
pub use error::MoneyError;
