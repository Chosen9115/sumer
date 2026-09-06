//! The observation model: provenance, balances, history observations,
//! per-resource status, and pagination.
//!
//! **Wire types vs. internal types.** `received_at` is host-stamped: the
//! contract requires that an adapter which sends it gets `invalid_request`,
//! never a silently-accepted (or silently-ignored) value. Modelling that as
//! `Option<Rfc3339>` on one shared struct would rely on every call site
//! remembering to check "is this the adapter's copy or the host's copy?" --
//! exactly the kind of thing that gets forgotten under deadline pressure.
//! Instead, the type that is deserialized from the wire (`ProvenanceWire`)
//! has *no field* for `received_at` at all, combined with
//! `#[serde(deny_unknown_fields)]`: a wire payload naming `received_at`
//! fails to deserialize, which callers map to `invalid_request`. The same
//! reasoning applies to `staleness`, which the contract says is "computed
//! BY THE HOST from received_at" -- it cannot be adapter-supplied either,
//! so it lives only on the host-stamped side. The fully-populated
//! `Provenance` (and `Balance`/`Observation`, which embed it) is reachable
//! only through `Provenance::stamp`, which takes the host-computed
//! `received_at`/`staleness` as plain arguments -- this crate defines the
//! *shape* of that computation's inputs and outputs, not the policy
//! (staleness thresholds, clock access) itself, which belongs to the host.

use crate::shape::object_only;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use sumer_money::{Amount, AssetId};

// ---------------------------------------------------------------------
// RFC 3339 timestamps
// ---------------------------------------------------------------------

/// An RFC 3339 timestamp string, structurally validated.
///
/// ponytail: this checks *shape* (field widths, separators, a plausible
/// numeric range per field) byte-by-byte, not calendar correctness -- it
/// accepts `2026-02-30T00:00:00Z`. A full calendar-aware parser is more
/// code for a check no wire fixture in this milestone exercises; if a real
/// adapter ever emits a calendar-invalid timestamp, upgrade this to a
/// proper date library then (there is currently no such dependency in this
/// crate, by design -- see the crate's frozen dependency list).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Rfc3339(String);

/// Why an `Rfc3339` construction was rejected. Carries no payload beyond a
/// byte offset: the rejected string itself is not retained, consistent
/// with `sumer-money`'s rule that error values never carry the rejected
/// input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rfc3339Error {
    pub at: usize,
}

impl fmt::Display for Rfc3339Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid RFC 3339 timestamp at offset {}", self.at)
    }
}

impl std::error::Error for Rfc3339Error {}

impl Rfc3339 {
    /// Validates and wraps a timestamp string.
    pub fn new(s: impl Into<String>) -> Result<Rfc3339, Rfc3339Error> {
        let s = s.into();
        validate_rfc3339(&s)?;
        Ok(Rfc3339(s))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Rfc3339 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Rfc3339::new(s).map_err(D::Error::custom)
    }
}

/// Reads two ASCII digits at `bytes[at..at+2]` as a number, or `None` if
/// out of range or not digits.
fn two_digits(bytes: &[u8], at: usize) -> Option<u32> {
    let a = *bytes.get(at)?;
    let b = *bytes.get(at + 1)?;
    if a.is_ascii_digit() && b.is_ascii_digit() {
        Some(u32::from(a - b'0') * 10 + u32::from(b - b'0'))
    } else {
        None
    }
}

fn validate_rfc3339(s: &str) -> Result<(), Rfc3339Error> {
    let b = s.as_bytes();
    // Minimum: "YYYY-MM-DDTHH:MM:SSZ" is 20 bytes.
    if b.len() < 20 {
        return Err(Rfc3339Error { at: b.len() });
    }
    let year_ok = b[0..4].iter().all(u8::is_ascii_digit);
    if !year_ok {
        return Err(Rfc3339Error { at: 0 });
    }
    if b[4] != b'-' {
        return Err(Rfc3339Error { at: 4 });
    }
    let month = two_digits(b, 5).ok_or(Rfc3339Error { at: 5 })?;
    if !(1..=12).contains(&month) {
        return Err(Rfc3339Error { at: 5 });
    }
    if b[7] != b'-' {
        return Err(Rfc3339Error { at: 7 });
    }
    let day = two_digits(b, 8).ok_or(Rfc3339Error { at: 8 })?;
    if !(1..=31).contains(&day) {
        return Err(Rfc3339Error { at: 8 });
    }
    if !matches!(b[10], b'T' | b't') {
        return Err(Rfc3339Error { at: 10 });
    }
    let hour = two_digits(b, 11).ok_or(Rfc3339Error { at: 11 })?;
    if hour > 23 {
        return Err(Rfc3339Error { at: 11 });
    }
    if b[13] != b':' {
        return Err(Rfc3339Error { at: 13 });
    }
    let minute = two_digits(b, 14).ok_or(Rfc3339Error { at: 14 })?;
    if minute > 59 {
        return Err(Rfc3339Error { at: 14 });
    }
    if b[16] != b':' {
        return Err(Rfc3339Error { at: 16 });
    }
    // Seconds allow 60 to tolerate a leap second.
    let second = two_digits(b, 17).ok_or(Rfc3339Error { at: 17 })?;
    if second > 60 {
        return Err(Rfc3339Error { at: 17 });
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == frac_start {
            return Err(Rfc3339Error { at: i });
        }
    }
    match b.get(i) {
        Some(b'Z' | b'z') => {
            if i + 1 == b.len() {
                Ok(())
            } else {
                Err(Rfc3339Error { at: i + 1 })
            }
        }
        Some(b'+' | b'-') => {
            i += 1;
            let off_hour = two_digits(b, i).ok_or(Rfc3339Error { at: i })?;
            if off_hour > 23 {
                return Err(Rfc3339Error { at: i });
            }
            i += 2;
            if b.get(i) != Some(&b':') {
                return Err(Rfc3339Error { at: i });
            }
            i += 1;
            let off_min = two_digits(b, i).ok_or(Rfc3339Error { at: i })?;
            if off_min > 59 {
                return Err(Rfc3339Error { at: i });
            }
            i += 2;
            if i == b.len() {
                Ok(())
            } else {
                Err(Rfc3339Error { at: i })
            }
        }
        _ => Err(Rfc3339Error {
            at: sign_or_end(i, b.len()),
        }),
    }
}

fn sign_or_end(i: usize, len: usize) -> usize {
    if i < len {
        i
    } else {
        len
    }
}

// ---------------------------------------------------------------------
// Plain-text descriptions
// ---------------------------------------------------------------------

/// Why a `description` was rejected as not-plain-text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlainTextError {
    pub at: usize,
}

impl fmt::Display for PlainTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "description contains an angle bracket or a control byte at offset {}",
            self.at
        )
    }
}

impl std::error::Error for PlainTextError {}

/// The plain-text rule for `description`, stated exactly as
/// `spec/observation.md` §3 states it: reject any ASCII control byte
/// (`U+0000`-`U+001F` except horizontal tab) , `U+007F`, and the two bytes
/// `<` and `>`. Everything else is accepted.
///
/// The spec used to say "HTML, Markdown, or any other markup convention"
/// gets `invalid_request`, which is not implementable: "is this Markdown"
/// has no decidable answer, and a real merchant name contains `*`, `#`,
/// `_`, `[`, and `-` without intending any of them as markup (`***ATM FEE`,
/// `#4471`, `PAYPAL *STEAM`). Rejecting those would reject legitimate
/// provider data; rejecting only *some* Markdown would be a rule no second
/// implementation could reproduce. `<`/`>` are different: they are the two
/// bytes every tag-based dialect needs, they carry no meaning of their own
/// in a payee name, and blocking them is exact.
///
/// The obligation the rejected-markup rule was reaching for lands on the
/// consumer instead, and the spec now says it there: `description` is
/// rendered as text, never as HTML/Markdown source.
pub fn validate_plain_text(s: &str) -> Result<(), PlainTextError> {
    for (i, b) in s.bytes().enumerate() {
        let is_control = b < 0x20 && b != b'\t';
        let is_tag_delim = b == b'<' || b == b'>';
        if is_control || b == 0x7F || is_tag_delim {
            return Err(PlainTextError { at: i });
        }
    }
    Ok(())
}

fn deserialize_plain_text<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s = String::deserialize(d)?;
    validate_plain_text(&s).map_err(D::Error::custom)?;
    Ok(s)
}

// ---------------------------------------------------------------------
// Shared enums
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Staleness {
    Live,
    Cached,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalHint {
    Available,
    Total,
    Pending,
    Held,
    Confirmed,
    Unconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationState {
    Active,
    Tombstoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Posting {
    Pending,
    Posted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawSign {
    ProviderPositive,
    ProviderNegative,
}

// ---------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------

/// Provenance exactly as an adapter may send it: no `received_at`, no
/// `staleness` -- both are host-computed, so there is no field for an
/// adapter to populate (correctly or otherwise). `deny_unknown_fields`
/// turns an adapter that sends `received_at` (or any other unknown key)
/// into an ordinary deserialize failure, which callers map to
/// `invalid_request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ProvenanceWire {
    pub adapter_id: String,
    pub provider_id: String,
    pub surface: String,
    /// Adapter-claimed observation time. Evidence only -- never trusted for
    /// ordering or staleness, both of which key off the host-stamped
    /// `received_at` instead.
    pub observed_at: Rfc3339,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at: Option<Rfc3339>,
    pub completeness: Completeness,
}

object_only!(
    ProvenanceWire,
    "provenance: an object, never a positional array",
    serialize
);

/// Provenance once the host has stamped it: everything in `ProvenanceWire`
/// plus `received_at` and `staleness`. The only constructor is
/// [`Provenance::stamp`] -- there is no `Deserialize` impl, so a
/// `Provenance` can never be produced directly from untrusted wire bytes.
#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub adapter_id: String,
    pub provider_id: String,
    pub surface: String,
    pub observed_at: Rfc3339,
    pub received_at: Rfc3339,
    pub effective_at: Option<Rfc3339>,
    pub staleness: Staleness,
    pub completeness: Completeness,
}

impl Provenance {
    /// The only way to build a `Provenance`: stamps the host-computed
    /// `received_at`/`staleness` onto wire data that structurally cannot
    /// carry either.
    #[must_use]
    pub fn stamp(wire: ProvenanceWire, received_at: Rfc3339, staleness: Staleness) -> Provenance {
        Provenance {
            adapter_id: wire.adapter_id,
            provider_id: wire.provider_id,
            surface: wire.surface,
            observed_at: wire.observed_at,
            received_at,
            effective_at: wire.effective_at,
            staleness,
            completeness: wire.completeness,
        }
    }
}

// ---------------------------------------------------------------------
// Balances
// ---------------------------------------------------------------------

/// A single balance line as an adapter emits it. Balances are a list,
/// never a struct: nothing sums them, nothing assumes a pair (e.g.
/// available/total) exists.
///
/// `amount`'s `deserialize_with` is a plain passthrough, but naming one at
/// all disables serde's built-in "an absent `Option<T>` field defaults to
/// `None`" special case. Without it, a missing `amount` key would
/// deserialize identically to an explicit `null` -- erasing the exact
/// distinction the wire contract draws ("null means the adapter looked and
/// doesn't know" vs. "the field is missing"). With it, `null` still
/// deserializes to `None` (unknown, never zero -- there is no serde
/// attribute anywhere on this field that could fabricate a zero), but an
/// absent key is a hard deserialize error instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct BalanceWire {
    /// Which resource this observation belongs to. Required: a
    /// `balances.read` reply batches observations for multiple resources
    /// into one array (A3 -- batched, not paginated), so without this the
    /// host has no way to attribute a balance line to the resource it came
    /// from.
    pub resource_id: String,
    /// The provider's verbatim category name -- not normalized.
    pub category: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_hint: Option<CanonicalHint>,
    #[serde(deserialize_with = "deserialize_required_amount")]
    pub amount: Option<Amount>,
    pub provenance: ProvenanceWire,
}

object_only!(BalanceWire, "a balance line: an object", serialize);

/// See the `amount` field doc on [`BalanceWire`]: this passthrough exists
/// only to opt the field out of serde's implicit "missing `Option<T>` field
/// becomes `None`" behavior, so a missing `amount` key is a hard error
/// while an explicit `null` still means `None` (unknown).
fn deserialize_required_amount<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Amount>, D::Error> {
    Option::<Amount>::deserialize(d)
}

/// A balance line once the host has stamped its provenance.
#[derive(Debug, Clone, Serialize)]
pub struct Balance {
    pub resource_id: String,
    pub category: String,
    pub canonical_hint: Option<CanonicalHint>,
    pub amount: Option<Amount>,
    pub provenance: Provenance,
}

impl Balance {
    #[must_use]
    pub fn stamp(wire: BalanceWire, received_at: Rfc3339, staleness: Staleness) -> Balance {
        Balance {
            resource_id: wire.resource_id,
            category: wire.category,
            canonical_hint: wire.canonical_hint,
            amount: wire.amount,
            provenance: Provenance::stamp(wire.provenance, received_at, staleness),
        }
    }
}

// ---------------------------------------------------------------------
// History observations
// ---------------------------------------------------------------------

/// A history observation as an adapter emits it. Adapters emit
/// observations, not revisions: there is deliberately no `revision` field
/// here -- the host assigns `revision: u64` by arrival order per
/// `(adapter_id, local_id)`. A restarted or stateless adapter cannot know
/// it is emitting revision 3; requiring it would test a capability no real
/// adapter has.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ObservationWire {
    /// Which resource this observation belongs to. Required for the same
    /// reason as `BalanceWire::resource_id`: a `history.read` reply batches
    /// observations for multiple resources into one array, and without
    /// this the host cannot attribute an observation to its resource.
    pub resource_id: String,
    /// A documented, versioned pure function of provider data (its
    /// derivation is named in `hello.local_id_derivation`).
    pub local_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_provider_id: Option<String>,
    /// Tombstone is not terminal -- a tombstoned entry can be followed by a
    /// later `active` observation of the same `local_id` (e.g. a
    /// blockchain reorg that re-mines a transaction).
    pub state: ObservationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone_reason: Option<String>,
    pub surface: String,
    pub posting: Posting,
    pub amount: Amount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fees: Option<Amount>,
    pub raw_sign: RawSign,
    /// Plain text -- see [`validate_plain_text`] for the exact rule.
    /// Enforced at deserialize time, so a `description` containing markup
    /// never becomes a live `ObservationWire` value in the first place;
    /// callers map the resulting deserialize failure to `invalid_request`.
    #[serde(deserialize_with = "deserialize_plain_text")]
    pub description: String,
    /// Provider-specific fields the wire model doesn't otherwise capture.
    /// Defaults to empty when absent -- unlike `amount`, an empty map is a
    /// legitimate value (the provider simply had nothing extra to say),
    /// not a fabricated stand-in for missing data.
    #[serde(default)]
    pub provider_extra: serde_json::Map<String, serde_json::Value>,
    pub provenance: ProvenanceWire,
}

object_only!(
    ObservationWire,
    "a history observation: an object",
    serialize
);

/// A history observation once the host has stamped its provenance (and,
/// elsewhere, assigned it a revision -- see the module docs on why
/// `revision` is not a field here either).
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub resource_id: String,
    pub local_id: String,
    pub provider_id: Option<String>,
    pub supersedes_provider_id: Option<String>,
    pub state: ObservationState,
    pub tombstone_reason: Option<String>,
    pub surface: String,
    pub posting: Posting,
    pub amount: Amount,
    pub fees: Option<Amount>,
    pub raw_sign: RawSign,
    pub description: String,
    pub provider_extra: serde_json::Map<String, serde_json::Value>,
    pub provenance: Provenance,
}

impl Observation {
    #[must_use]
    pub fn stamp(wire: ObservationWire, received_at: Rfc3339, staleness: Staleness) -> Observation {
        Observation {
            resource_id: wire.resource_id,
            local_id: wire.local_id,
            provider_id: wire.provider_id,
            supersedes_provider_id: wire.supersedes_provider_id,
            state: wire.state,
            tombstone_reason: wire.tombstone_reason,
            surface: wire.surface,
            posting: wire.posting,
            amount: wire.amount,
            fees: wire.fees,
            raw_sign: wire.raw_sign,
            description: wire.description,
            provider_extra: wire.provider_extra,
            provenance: Provenance::stamp(wire.provenance, received_at, staleness),
        }
    }
}

/// The fold total order key: `(received_at, surface, arrival_index)`
/// ascending, `surface` compared bytewise. `String`/`Rfc3339` ordering in
/// Rust already compares `str`s by their UTF-8 bytes, which *is* bytewise
/// comparison, so no separate byte-slice comparison is needed here.
///
/// This assumes `received_at` values being compared were all stamped by
/// the same host clock in the same canonical format (true by construction:
/// `received_at` is never adapter-supplied, see [`Provenance::stamp`]), so
/// lexicographic order on the timestamp string agrees with chronological
/// order.
///
/// This function defines the *order*; it does not build the live set.
/// Folding observations into a live set (applying `supersedes`/tombstone
/// semantics) is `sumer_host::fold`'s job, per the frozen contract -- this
/// crate and that implementation must agree on the order, which is exactly
/// what this function -- and the property test built on it -- pin down.
#[must_use]
pub fn fold_order_key(
    received_at: &Rfc3339,
    surface: &str,
    arrival_index: u64,
) -> (Rfc3339, String, u64) {
    (received_at.clone(), surface.to_owned(), arrival_index)
}

// ---------------------------------------------------------------------
// Resource status
// ---------------------------------------------------------------------

/// Evidence from the provider, riding alongside an outcome. Never an
/// identifier -- `code`/`message`/`raw` are for a human or a log, not for
/// matching logic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ProviderDetail {
    pub code: String,
    pub message: String,
    pub raw: serde_json::Value,
}

object_only!(ProviderDetail, "provider_detail: an object", serialize);

/// The outcome of attempting to read one resource.
///
/// Deliberately **not** internally tagged (no `#[serde(tag = "...")]`): this
/// type is always embedded as `ResourceStatus.outcome`, so an internal tag
/// would have to be named something other than `outcome` to avoid nesting a
/// field named `outcome` inside another field named `outcome`. Every fixture
/// under `conformance/cases/` (and every real adapter reply) uses serde's
/// plain default (externally tagged) representation instead --
/// `{"fetched": {"page_empty": false}}`, a bare string for a unit variant --
/// which is what this derive produces without a `tag` attribute.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOutcome {
    Fetched { page_empty: bool },
    NotFetched,
    Stale { as_of: Rfc3339 },
    RateLimited { retry_after_ms: u64 },
    Unavailable,
    ReauthRequired,
    Revoked,
    Gone,
    ScaRequired,
}

/// The four outcome bodies, each object-only (see [`crate::shape`]): a
/// derived enum accepts `{"fetched": [false]}` for a struct variant, which
/// is the same positional-array trap the wire structs have, one level in.
/// These are private -- the public shape stays the struct variants above.
#[derive(Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
struct FetchedBody {
    page_empty: bool,
}
object_only!(FetchedBody, "a `fetched` body: an object");

#[derive(Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
struct StaleBody {
    as_of: Rfc3339,
}
object_only!(StaleBody, "a `stale` body: an object");

#[derive(Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
struct RateLimitedBody {
    retry_after_ms: u64,
}
object_only!(RateLimitedBody, "a `rate_limited` body: an object");

/// One observation this resource could not deliver because it exceeded
/// [`crate::MAX_OBSERVATION_BYTES`] (spec/observation.md §6), reported
/// beside the resource's outcome rather than instead of it: a resource can
/// be stale *and* have dropped an oversized record, and the outcome is
/// where the freshness fact lives. Set by whichever side did the omitting
/// -- the adapter, or the host enforcing the cap at decode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct Degraded {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_id: Option<String>,
    pub bytes: u64,
}

object_only!(
    Degraded,
    "a `degraded` entry: an object with `bytes` and an optional `local_id`",
    serialize
);

const OUTCOME_VARIANTS: &[&str] = &[
    "fetched",
    "not_fetched",
    "stale",
    "rate_limited",
    "unavailable",
    "reauth_required",
    "revoked",
    "gone",
    "sca_required",
];

/// Hand-written so an outcome names **exactly one** variant, and so each
/// payload variant's body must be an object. `deserialize_any` is what
/// admits both wire forms the contract uses: a bare string for a unit
/// variant (`"unavailable"`) and a single-key object for one with a body
/// (`{"fetched": {"page_empty": false}}`).
impl<'de> Deserialize<'de> for ReadOutcome {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct OutcomeVisitor;

        impl<'de> serde::de::Visitor<'de> for OutcomeVisitor {
            type Value = ReadOutcome;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a read outcome: a bare string, or an object naming exactly one outcome",
                )
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<ReadOutcome, E> {
                match v {
                    "not_fetched" => Ok(ReadOutcome::NotFetched),
                    "unavailable" => Ok(ReadOutcome::Unavailable),
                    "reauth_required" => Ok(ReadOutcome::ReauthRequired),
                    "revoked" => Ok(ReadOutcome::Revoked),
                    "gone" => Ok(ReadOutcome::Gone),
                    "sca_required" => Ok(ReadOutcome::ScaRequired),
                    other => Err(E::unknown_variant(other, OUTCOME_VARIANTS)),
                }
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<ReadOutcome, A::Error> {
                let Some(key) = map.next_key::<String>()? else {
                    return Err(A::Error::custom("an outcome object names one outcome"));
                };
                let outcome = match key.as_str() {
                    "fetched" => ReadOutcome::Fetched {
                        page_empty: map.next_value::<FetchedBody>()?.page_empty,
                    },
                    "stale" => ReadOutcome::Stale {
                        as_of: map.next_value::<StaleBody>()?.as_of,
                    },
                    "rate_limited" => ReadOutcome::RateLimited {
                        retry_after_ms: map.next_value::<RateLimitedBody>()?.retry_after_ms,
                    },
                    other => return Err(A::Error::unknown_variant(other, OUTCOME_VARIANTS)),
                };
                if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(A::Error::custom(
                        "an outcome names exactly one variant, never several",
                    ));
                }
                Ok(outcome)
            }
        }

        d.deserialize_any(OutcomeVisitor)
    }
}

/// One entry of a reply's `statuses` array. Every requested `resource_id`
/// appears exactly once across a reply's `statuses` -- which is why an
/// oversized-observation degrade rides in `degraded` rather than as an
/// outcome of its own: one entry, two independent facts (how fresh this
/// resource's data is, and whether a record was dropped for size).
///
/// `credential_expires_at`/`strong_auth_expires_at`/`history_start` are
/// populated only by `status.read` (the two clocks the contract calls out:
/// credential lifetime and strong-auth/SCA lifetime are tracked
/// separately); `balances.read`/`history.read` leave them `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ResourceStatus {
    pub resource_id: String,
    pub outcome: ReadOutcome,
    /// An observation this resource dropped for exceeding
    /// [`crate::MAX_OBSERVATION_BYTES`]. Independent of `outcome`: the
    /// degrade is not a freshness fact and must never overwrite one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<Degraded>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_detail: Option<ProviderDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<PageReply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_expires_at: Option<Rfc3339>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strong_auth_expires_at: Option<Rfc3339>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_start: Option<Rfc3339>,
}

object_only!(ResourceStatus, "a status entry: an object", serialize);

// ---------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------

/// How to resume (or start) a paginated read. `Window`'s `[start, end)` is
/// half-open; there is no offset/`nextOffset` variant (FDX deprecates it).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    remote = "Self",
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PageRequest {
    Cursor {
        cursor: String,
    },
    Window {
        resource_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asset: Option<AssetId>,
        start: Rfc3339,
        end: Rfc3339,
    },
}

object_only!(
    PageRequest,
    "a page request: an object with a `kind`",
    serialize
);

/// How a durable cursor should be advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorResumable {
    /// Advance the durable cursor after every page (e.g. Bitcoin block
    /// height).
    Exact,
    /// Advance the durable cursor only once `next` is `None` -- an
    /// interrupted multi-page batch restarts from the beginning (e.g.
    /// Plaid).
    BatchRestart,
    /// No durable cursor; resume by `(resource_id, asset, start, end)`.
    None,
}

/// Paging continuation for one resource's read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct PageReply {
    pub cursor_resumable: CursorResumable,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<PageRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_capped_to: Option<Rfc3339>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size_reduced_to: Option<u32>,
}

object_only!(PageReply, "a page reply: an object", serialize);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Rfc3339 {
        Rfc3339::new(s).unwrap()
    }

    // --- Rfc3339 ---

    #[test]
    fn accepts_basic_utc_timestamp() {
        assert!(Rfc3339::new("2026-09-06T12:00:00Z").is_ok());
    }

    #[test]
    fn accepts_fractional_seconds() {
        assert!(Rfc3339::new("2026-09-06T12:00:00.123456Z").is_ok());
    }

    #[test]
    fn accepts_numeric_offset() {
        assert!(Rfc3339::new("2026-09-06T12:00:00+05:30").is_ok());
        assert!(Rfc3339::new("2026-09-06T12:00:00-05:30").is_ok());
    }

    #[test]
    fn rejects_missing_timezone() {
        assert!(Rfc3339::new("2026-09-06T12:00:00").is_err());
    }

    #[test]
    fn rejects_bad_month() {
        assert!(Rfc3339::new("2026-13-06T12:00:00Z").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Rfc3339::new("not a timestamp").is_err());
        assert!(Rfc3339::new("").is_err());
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(Rfc3339::new("2026-09-06T12:00:00Zgarbage").is_err());
    }

    // --- plain text ---

    #[test]
    fn plain_text_accepts_ordinary_memo() {
        assert!(validate_plain_text("Coffee shop purchase - #4471").is_ok());
    }

    #[test]
    fn plain_text_rejects_angle_brackets() {
        let err = validate_plain_text("<script>alert(1)</script>").unwrap_err();
        assert_eq!(err.at, 0);
    }

    #[test]
    fn plain_text_rejects_control_bytes() {
        let err = validate_plain_text("hello\u{0007}world").unwrap_err();
        assert_eq!(err.at, 5);
    }

    #[test]
    fn plain_text_accepts_markdown_looking_merchant_names() {
        // The executable rule is angle brackets and control bytes, not
        // "looks like markup": these are real payee strings.
        assert!(validate_plain_text("**rent**").is_ok());
        assert!(validate_plain_text("PAYPAL *STEAM PURCHASE").is_ok());
        assert!(validate_plain_text("SQ *COFFEE #4471 [ATM]").is_ok());
        assert!(validate_plain_text("A & B_Co - 50% off").is_ok());
    }

    #[test]
    fn plain_text_allows_tab() {
        assert!(validate_plain_text("hello\tworld").is_ok());
    }

    #[test]
    fn observation_wire_rejects_markup_description() {
        let json = serde_json::json!({
            "resource_id": "checking-1",
            "local_id": "abc",
            "state": "active",
            "surface": "checking",
            "posting": "posted",
            "amount": {"asset": "USD", "amount": "10.00"},
            "raw_sign": "provider_positive",
            "description": "<b>rent</b>",
            "provenance": {
                "adapter_id": "a", "provider_id": "p", "surface": "checking",
                "observed_at": "2026-09-06T12:00:00Z", "completeness": "complete"
            }
        });
        let result: Result<ObservationWire, _> = serde_json::from_str(&json.to_string());
        assert!(result.is_err());
    }

    // --- provenance wire/internal split ---

    #[test]
    fn provenance_wire_rejects_received_at() {
        let json = serde_json::json!({
            "adapter_id": "a", "provider_id": "p", "surface": "checking",
            "observed_at": "2026-09-06T12:00:00Z",
            "received_at": "2026-09-06T12:00:01Z",
            "completeness": "complete"
        });
        let result: Result<ProvenanceWire, _> = serde_json::from_str(&json.to_string());
        assert!(result.is_err(), "received_at on the wire must be rejected");
    }

    #[test]
    fn provenance_wire_rejects_staleness() {
        let json = serde_json::json!({
            "adapter_id": "a", "provider_id": "p", "surface": "checking",
            "observed_at": "2026-09-06T12:00:00Z",
            "staleness": "live",
            "completeness": "complete"
        });
        let result: Result<ProvenanceWire, _> = serde_json::from_str(&json.to_string());
        assert!(
            result.is_err(),
            "staleness is host-computed, never adapter-supplied"
        );
    }

    #[test]
    fn provenance_stamp_carries_wire_fields_through() {
        let wire: ProvenanceWire = serde_json::from_str(
            &serde_json::json!({
                "adapter_id": "a", "provider_id": "p", "surface": "checking",
                "observed_at": "2026-09-06T12:00:00Z", "completeness": "complete"
            })
            .to_string(),
        )
        .unwrap();
        let full = Provenance::stamp(wire, ts("2026-09-06T12:00:05Z"), Staleness::Live);
        assert_eq!(full.adapter_id, "a");
        assert_eq!(full.received_at.as_str(), "2026-09-06T12:00:05Z");
        assert_eq!(full.staleness, Staleness::Live);
    }

    // --- amount: null is unknown, never zero ---

    #[test]
    fn balance_amount_null_is_none() {
        let json = serde_json::json!({
            "resource_id": "checking-1",
            "category": "available",
            "amount": null,
            "provenance": {
                "adapter_id": "a", "provider_id": "p", "surface": "checking",
                "observed_at": "2026-09-06T12:00:00Z", "completeness": "unknown"
            }
        });
        let balance: BalanceWire = serde_json::from_str(&json.to_string()).unwrap();
        assert!(balance.amount.is_none());
    }

    #[test]
    fn balance_amount_missing_is_a_hard_error() {
        let json = serde_json::json!({
            "resource_id": "checking-1",
            "category": "available",
            "provenance": {
                "adapter_id": "a", "provider_id": "p", "surface": "checking",
                "observed_at": "2026-09-06T12:00:00Z", "completeness": "unknown"
            }
        });
        let result: Result<BalanceWire, _> = serde_json::from_str(&json.to_string());
        assert!(
            result.is_err(),
            "a missing amount must not silently become None or zero"
        );
    }

    #[test]
    fn balance_wire_requires_resource_id() {
        let json = serde_json::json!({
            "category": "available",
            "amount": null,
            "provenance": {
                "adapter_id": "a", "provider_id": "p", "surface": "checking",
                "observed_at": "2026-09-06T12:00:00Z", "completeness": "unknown"
            }
        });
        let result: Result<BalanceWire, _> = serde_json::from_str(&json.to_string());
        assert!(
            result.is_err(),
            "a balance batching multiple resources must be able to attribute each line"
        );
    }

    #[test]
    fn observation_wire_requires_resource_id() {
        let json = serde_json::json!({
            "local_id": "abc",
            "state": "active",
            "surface": "checking",
            "posting": "posted",
            "amount": {"asset": "USD", "amount": "10.00"},
            "raw_sign": "provider_positive",
            "description": "rent",
            "provenance": {
                "adapter_id": "a", "provider_id": "p", "surface": "checking",
                "observed_at": "2026-09-06T12:00:00Z", "completeness": "complete"
            }
        });
        let result: Result<ObservationWire, _> = serde_json::from_str(&json.to_string());
        assert!(result.is_err(), "history.read batches multiple resources");
    }

    #[test]
    fn balance_stamp_carries_resource_id_through() {
        let wire: BalanceWire = serde_json::from_str(
            &serde_json::json!({
                "resource_id": "checking-1",
                "category": "available",
                "amount": null,
                "provenance": {
                    "adapter_id": "a", "provider_id": "p", "surface": "checking",
                    "observed_at": "2026-09-06T12:00:00Z", "completeness": "unknown"
                }
            })
            .to_string(),
        )
        .unwrap();
        let stamped = Balance::stamp(wire, ts("2026-09-06T12:00:05Z"), Staleness::Live);
        assert_eq!(stamped.resource_id, "checking-1");
    }

    // --- fold order key ---

    #[test]
    fn fold_order_key_orders_by_received_at_then_surface_then_arrival() {
        let a = fold_order_key(&ts("2026-09-06T12:00:00Z"), "checking", 0);
        let b = fold_order_key(&ts("2026-09-06T12:00:01Z"), "checking", 1);
        assert!(a < b);

        let c = fold_order_key(&ts("2026-09-06T12:00:00Z"), "checking", 5);
        let d = fold_order_key(&ts("2026-09-06T12:00:00Z"), "savings", 0);
        assert!(c < d, "surface compared bytewise after received_at ties");
    }

    // --- resource status / outcome ---

    #[test]
    fn resource_status_fetched_round_trip() {
        let status = ResourceStatus {
            resource_id: "acct1".to_owned(),
            outcome: ReadOutcome::Fetched { page_empty: false },
            degraded: None,
            provider_detail: None,
            page: None,
            credential_expires_at: None,
            strong_auth_expires_at: None,
            history_start: None,
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: ResourceStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resource_id, "acct1");
        assert!(matches!(
            back.outcome,
            ReadOutcome::Fetched { page_empty: false }
        ));
        assert!(back.degraded.is_none());
        assert!(
            !json.contains("degraded"),
            "an absent degrade is not written at all: {json}"
        );
    }

    #[test]
    fn a_degraded_resource_keeps_its_own_outcome() {
        // Both facts on one entry: the data is stale, AND one record was
        // dropped for size. Neither overwrites the other.
        let text = r#"{"resource_id":"acct1","outcome":{"stale":{"as_of":"2026-09-06T11:00:00Z"}},
                       "degraded":{"local_id":"huge","bytes":260000}}"#;
        let status: ResourceStatus = serde_json::from_str(text).unwrap();
        assert!(matches!(status.outcome, ReadOutcome::Stale { .. }));
        let degraded = status.degraded.unwrap();
        assert_eq!(degraded.local_id.as_deref(), Some("huge"));
        assert_eq!(degraded.bytes, 260_000);
    }

    #[test]
    fn oversized_observation_is_no_longer_an_outcome() {
        // It moved to `degraded`; an adapter still spelling it as an
        // outcome names a variant that does not exist, rather than
        // silently overwriting the resource's freshness.
        let result: Result<ReadOutcome, _> =
            serde_json::from_str(r#"{"oversized_observation":{"local_id":"huge","bytes":9}}"#);
        assert!(result.is_err());
    }

    #[test]
    fn page_request_window_round_trip() {
        let req = PageRequest::Window {
            resource_id: "acct1".to_owned(),
            asset: Some(AssetId::new("USD").unwrap()),
            start: ts("2026-01-01T00:00:00Z"),
            end: ts("2026-02-01T00:00:00Z"),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: PageRequest = serde_json::from_str(&json).unwrap();
        match back {
            PageRequest::Window { resource_id, .. } => assert_eq!(resource_id, "acct1"),
            PageRequest::Cursor { .. } => panic!("expected Window"),
        }
    }
}
