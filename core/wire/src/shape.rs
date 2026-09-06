//! Object-only deserialization for wire types.
//!
//! serde's derived `Deserialize` for a struct also generates a `visit_seq`,
//! so `["a","p","s","2026-09-06T00:00:00Z",null,"complete"]` deserializes
//! into a struct whose wire shape is documented as an object -- and
//! `deny_unknown_fields` does not constrain shape, only names. That lets a
//! Rust adapter accept frames another language's adapter rejects, which is
//! exactly the cross-implementation divergence `spec/wire.md` exists to
//! prevent. `sumer-money`'s `AmountWire` was hand-written for this reason;
//! this macro applies the same rule to every other wire type without
//! hand-writing each one.
//!
//! The mechanism: `#[serde(remote = "Self")]` makes serde generate
//! *inherent* `deserialize`/`serialize` functions instead of trait impls,
//! and this macro writes the trait impls -- deserialization through a
//! visitor that implements `visit_map` and nothing else (so any other JSON
//! shape is an `invalid type` error), serialization forwarded unchanged.

macro_rules! object_only {
    // Deserialize only -- for a type that does not also derive `Serialize`
    // with `remote = "Self"`.
    ($ty:ty, $expecting:expr) => {
        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct ObjectOnly;
                impl<'de> serde::de::Visitor<'de> for ObjectOnly {
                    type Value = $ty;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str($expecting)
                    }
                    fn visit_map<A: serde::de::MapAccess<'de>>(
                        self,
                        map: A,
                    ) -> Result<$ty, A::Error> {
                        <$ty>::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    }
                }
                d.deserialize_map(ObjectOnly)
            }
        }
    };
    // ...plus the `Serialize` trait impl the remote derive replaced with an
    // inherent function. Only valid for a type that derives `Serialize`:
    // without that derive there is no inherent `serialize` to forward to and
    // the call below would recurse into itself.
    ($ty:ty, $expecting:expr, serialize) => {
        object_only!($ty, $expecting);

        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                <$ty>::serialize(self, s)
            }
        }
    };
}

pub(crate) use object_only;
