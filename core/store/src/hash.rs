//! `content_hash`: SHA-256 over **adapter-authored content only**.
//!
//! The field list is the frozen contract's, verbatim: `local_id`,
//! `provider_id`, `supersedes_provider_id`, `state`, `tombstone_reason`,
//! `surface`, `posting`, `amount`, `fees`, `raw_sign`, `description`,
//! `provider_extra`, `observed_at`, `effective_at`, `completeness` --
//! **plus `resource_id`, a deliberate amendment to that list.**
//!
//! # Why `resource_id` was added
//!
//! Records are keyed by `(adapter_id, local_id)` (`spec/observation.md`
//! §3), which is exactly why a `local_id` CAN reach the store under a
//! second `resource_id`: the key holds no resource, §3 lets resource ids
//! collide, and [`sumer_host::fold`] keys chains adapter-wide precisely so
//! a record that moves resource keeps one chain. An adapter that emits a
//! bare txid across two wallets is therefore reachable, and while
//! `resource_id` was outside the hash such a record deduped against the
//! head stored under the OLD resource: nothing was stored under the new
//! one, the head kept the old resource's attribution, and the old
//! resource's next sweep retracted -- as `resource_definition_changed` --
//! a record the adapter had reported in that very refresh. Which resource
//! was swept first decided the outcome, and nothing constrains that order.
//!
//! A record appearing under a different resource is a **changed fact**:
//! the same principle that makes a byte-identical re-emission of a buried
//! record append a revision. The hash must discriminate on every field the
//! system discriminates on; one that omits one is a trap for whoever reads
//! it next. §3 now also forbids an adapter to report one `local_id`
//! under two `resource_id`s at the same time -- which does not make this
//! redundant: that is an obligation on adapters, and a retraction may not
//! rest on an obligation a buggy adapter can break. With `resource_id`
//! hashed, a violation is merely loud (a revision per sweep, the record
//! under whichever resource swept last) instead of silent (the record
//! live under no resource at all).
//!
//! **What is excluded, and why the exclusion is the whole point.**
//! `received_at`, `staleness`, `revision`, `crawl_id` differ on every
//! sweep. Hash them and no hash ever matches a stored one, so the "same
//! record, seen again" case never fires and a 5000-transaction wallet
//! appends 5000 rows on every refresh -- history that grows without a
//! single fact changing. `provenance.adapter_id` and
//! `provenance.provider_id` are excluded too: the adapter is already part
//! of the chain key, and the provider is the *vantage*, whose change is
//! handled by the vantage exemption rather than by re-appending every
//! record in the wallet.
//!
//! Two fields on an `Observation` are named `surface` and `provider_id`
//! (once on the record, once on its provenance). The contract's list means
//! the record's own, which is what this hashes -- the provenance surface is
//! evidence about where the read came from, not content.
//!
//! Determinism comes from `serde_json`'s default `Map` being a `BTreeMap`:
//! every object this builds, including a nested `provider_extra`, is
//! serialized with its keys in sorted order.

use sha2::{Digest, Sha256};
use sumer_wire::Observation;

/// Lowercase hex SHA-256 of the canonical JSON of the fields above.
#[must_use]
pub fn content_hash(observation: &Observation) -> String {
    let mut fields = serde_json::Map::new();
    let mut put = |key: &str, value: serde_json::Value| {
        fields.insert(key.to_owned(), value);
    };
    put("resource_id", observation.resource_id.as_str().into());
    put("local_id", observation.local_id.as_str().into());
    put("provider_id", opt_str(observation.provider_id.as_deref()));
    put(
        "supersedes_provider_id",
        opt_str(observation.supersedes_provider_id.as_deref()),
    );
    put("state", enum_json(&observation.state));
    put(
        "tombstone_reason",
        opt_str(observation.tombstone_reason.as_deref()),
    );
    put("surface", observation.surface.as_str().into());
    put("posting", enum_json(&observation.posting));
    put("amount", money_json(Some(&observation.amount)));
    put("fees", money_json(observation.fees.as_ref()));
    put("raw_sign", enum_json(&observation.raw_sign));
    put("description", observation.description.as_str().into());
    put(
        "provider_extra",
        serde_json::Value::Object(observation.provider_extra.clone()),
    );
    put(
        "observed_at",
        observation.provenance.observed_at.as_str().into(),
    );
    put(
        "effective_at",
        opt_str(
            observation
                .provenance
                .effective_at
                .as_ref()
                .map(sumer_wire::Rfc3339::as_str),
        ),
    );
    put(
        "completeness",
        enum_json(&observation.provenance.completeness),
    );

    let canonical = serde_json::Value::Object(fields).to_string();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex(&hasher.finalize())
}

fn opt_str(value: Option<&str>) -> serde_json::Value {
    match value {
        Some(s) => s.into(),
        None => serde_json::Value::Null,
    }
}

/// The wire spelling of a small `Serialize` enum (`"active"`,
/// `"posted"`, ...). These are unit variants with `rename_all =
/// "snake_case"`, so serialization is infallible in practice; a failure
/// would still have to produce *something*, and `null` is the one value
/// that cannot be confused with a real spelling.
fn enum_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// `{"asset": "...", "amount": "..."}` -- the amount stays the adapter's
/// exact decimal string, never a number.
fn money_json(amount: Option<&sumer_money::Amount>) -> serde_json::Value {
    match amount {
        Some(amount) => serde_json::json!({
            "asset": amount.asset().as_str(),
            "amount": amount.to_string(),
        }),
        None => serde_json::Value::Null,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        // Writing to a String is infallible; `let _` documents that rather
        // than hiding a real error.
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Lowercase hex SHA-256 of arbitrary bytes -- the resource-definition
/// fingerprint uses the same primitive as `content_hash`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use sumer_money::{Amount, AssetId};
    use sumer_wire::{
        Completeness, ObservationState, Posting, Provenance, ProvenanceWire, RawSign, Rfc3339,
        Staleness,
    };

    fn observation(amount: &str, received_at: &str, staleness: Staleness) -> Observation {
        Observation {
            resource_id: "acct".to_owned(),
            local_id: "L1".to_owned(),
            provider_id: None,
            supersedes_provider_id: None,
            state: ObservationState::Active,
            tombstone_reason: None,
            surface: "checking".to_owned(),
            posting: Posting::Posted,
            amount: Amount::parse(AssetId::new("usd").unwrap(), amount).unwrap(),
            fees: None,
            raw_sign: RawSign::ProviderPositive,
            description: "coffee".to_owned(),
            provider_extra: serde_json::Map::new(),
            provenance: Provenance::stamp(
                ProvenanceWire {
                    adapter_id: "a1".to_owned(),
                    provider_id: "p1".to_owned(),
                    surface: "checking".to_owned(),
                    observed_at: Rfc3339::new("2026-01-01T00:00:00Z").unwrap(),
                    effective_at: None,
                    completeness: Completeness::Complete,
                },
                Rfc3339::new(received_at).unwrap(),
                staleness,
            ),
        }
    }

    /// The whole reason `content_hash` exists. `received_at` and
    /// `staleness` differ on EVERY sweep, so a hash that included them
    /// would never match a stored one -- the "same record, seen again"
    /// case would never fire and a 5,000-transaction wallet would append
    /// 5,000 rows on every refresh, for ever, without one fact changing.
    ///
    /// This is a unit test rather than an end-to-end one on purpose: the
    /// host clock has second precision, so two refreshes in one test run
    /// usually share a `received_at` and an end-to-end check would pass
    /// against a hash that DID include it. Two timestamps a year apart
    /// cannot be papered over that way.
    #[test]
    fn host_stamped_provenance_is_not_part_of_the_hash() {
        let first = observation("42.00", "2026-01-01T00:00:00Z", Staleness::Live);
        let second = observation("42.00", "2027-06-30T23:59:59Z", Staleness::Cached);
        assert_eq!(
            content_hash(&first),
            content_hash(&second),
            "received_at and staleness are host-stamped, not adapter-authored content"
        );
    }

    /// And the other half: adapter-authored content that DID change must
    /// move the hash, or a revision would be silently swallowed.
    #[test]
    fn a_changed_amount_moves_the_hash() {
        let before = observation("42.00", "2026-01-01T00:00:00Z", Staleness::Live);
        let after = observation("42.37", "2026-01-01T00:00:00Z", Staleness::Live);
        assert_ne!(content_hash(&before), content_hash(&after));
    }

    /// The same record under a second `resource_id` is a CHANGED FACT.
    /// While this was outside the hash it deduped against the head stored
    /// under the old resource, so it was stored nowhere, shown nowhere,
    /// and retracted by the old resource's next sweep.
    #[test]
    fn the_resource_is_content() {
        let here = observation("42.00", "2026-01-01T00:00:00Z", Staleness::Live);
        let mut moved = here.clone();
        moved.resource_id = "other".to_owned();
        assert_ne!(content_hash(&here), content_hash(&moved));
    }

    /// Scale is content. `42.0` and `42.00` are the same *value* and two
    /// different things for a provider to have said, so they are two
    /// different records -- consistent with storing the adapter's exact
    /// string rather than a normalized one.
    #[test]
    fn scale_is_content() {
        let coarse = observation("42.0", "2026-01-01T00:00:00Z", Staleness::Live);
        let fine = observation("42.00", "2026-01-01T00:00:00Z", Staleness::Live);
        assert_ne!(content_hash(&coarse), content_hash(&fine));
    }
}
