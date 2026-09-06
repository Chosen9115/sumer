//! Asset identifiers: opaque, byte-compared, never normalized.

use crate::{MoneyError, MAX_ASSET_ID_LEN};
use std::fmt;
use std::str::FromStr;

/// An opaque asset identifier (e.g. `"USD"`, `"BTC"`, a chain-qualified token id).
///
/// No normalization of any kind is performed: two `AssetId`s are equal iff
/// their bytes are equal. Case, whitespace, etc. are all significant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AssetId(String);

impl AssetId {
    /// Non-empty, at most `MAX_ASSET_ID_LEN` bytes, every byte a printable
    /// ASCII character (`0x21..=0x7E`, i.e. excluding space and control
    /// bytes). No normalization.
    pub fn new(s: &str) -> Result<Self, MoneyError> {
        if s.is_empty() {
            return Err(MoneyError::AssetIdEmpty);
        }
        if s.len() > MAX_ASSET_ID_LEN {
            return Err(MoneyError::AssetIdTooLong { len: s.len() });
        }
        if let Some(at) = s.bytes().position(|b| !(0x21..=0x7E).contains(&b)) {
            return Err(MoneyError::AssetIdInvalidByte { at });
        }
        Ok(AssetId(s.to_owned()))
    }

    /// The bare asset id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AssetId {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, MoneyError> {
        AssetId::new(s)
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for AssetId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for AssetId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deserializing straight to `String` rejects non-string JSON values
        // (numbers, bools, objects, ...) with serde's own invalid-type error,
        // before AssetId::new ever runs.
        let s = String::deserialize(deserializer)?;
        AssetId::new(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(AssetId::new(""), Err(MoneyError::AssetIdEmpty));
    }

    #[test]
    fn rejects_too_long() {
        let s = "A".repeat(MAX_ASSET_ID_LEN + 1);
        assert_eq!(
            AssetId::new(&s),
            Err(MoneyError::AssetIdTooLong {
                len: MAX_ASSET_ID_LEN + 1
            })
        );
    }

    #[test]
    fn accepts_exactly_max_len() {
        let s = "A".repeat(MAX_ASSET_ID_LEN);
        assert!(AssetId::new(&s).is_ok());
    }

    #[test]
    fn rejects_space_and_control_bytes() {
        assert_eq!(
            AssetId::new("US D"),
            Err(MoneyError::AssetIdInvalidByte { at: 2 })
        );
        assert_eq!(
            AssetId::new("USD\n"),
            Err(MoneyError::AssetIdInvalidByte { at: 3 })
        );
    }

    #[test]
    fn no_normalization_case_sensitive() {
        let upper = AssetId::new("USD").unwrap();
        let lower = AssetId::new("usd").unwrap();
        assert_ne!(upper, lower);
        assert_eq!(upper.as_str(), "USD");
    }

    #[test]
    fn serde_round_trip() {
        let id = AssetId::new("BTC").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"BTC\"");
        let back: AssetId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn serde_rejects_non_string() {
        let err = serde_json::from_str::<AssetId>("42").unwrap_err();
        assert!(err.to_string().contains("string") || err.is_data());
    }
}
