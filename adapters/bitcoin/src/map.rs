//! Pure mapping: Esplora JSON in, `sumer_wire` types out.
//!
//! **Nothing in this module performs I/O, reads a clock, or touches the
//! network.** Every function here is a pure function of its arguments,
//! including the "current time" (passed in as [`Ctx::observed_at`]). That
//! is deliberate and load-bearing: the frozen contract names *a wrong
//! mapping* as this adapter's riskiest failure, and a wrong mapping is
//! only catchable by tests that can pin the mapping without a provider.
//! Fetching lives in `source.rs`, orchestration in `wallet.rs`, state in
//! `seen.rs`; if a change here needs a socket or a clock, it belongs in
//! one of those instead.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use sumer_money::{Amount, AssetId, MoneyError};
use sumer_wire::{
    BalanceWire, CanonicalHint, Completeness, Degraded, ObservationState, ObservationWire, Posting,
    ProvenanceWire, RawSign, Rfc3339, Rfc3339Error, MAX_OBSERVATION_BYTES,
};

/// The `local_id` derivation this adapter declares in its hello reply.
/// `local_id = "<resource_id>:<txid>"` -- see [`local_id`].
pub const LOCAL_ID_DERIVATION: &str = "btc-txid@1";

/// Bitcoin's smallest unit, and the only asset this adapter ever emits.
/// Scale 0, integers only: there is no BTC conversion anywhere in this
/// crate, because a conversion is a division by 10^8 and `spec/money.md`
/// exists to keep that kind of arithmetic out of money entirely.
pub const ASSET_SAT: &str = "sat";

/// The surface a history observation was read through.
pub const SURFACE_TX: &str = "esplora.tx";
/// The surface a balance line was computed from.
pub const SURFACE_STATS: &str = "esplora.address_stats";

/// The most one PAGE of observations may serialize to: half of
/// `sumer_wire::MAX_FRAME_BYTES`.
///
/// **This is a cap on a page, not the frame limit.** The frame limit
/// belongs to the whole REPLY -- every resource's observations, every
/// resource's status entry and every `provider_detail` share one frame --
/// and lives in `main.rs`'s `Budget`. A page gets the smaller of the two.
/// Confusing the two is what let two resources produce a 1,049,218-byte
/// frame against a 1,048,576-byte cap.
///
/// **Pages are cut by bytes, never by block.** The rejected alternative --
/// "a page never splits a block" -- was a denial of service anyone could
/// trigger: roughly 1300 observations fill a 1 MiB frame, a block holds up
/// to ~6000 transactions, and mailing dust to a published address is free.
/// An oversized frame is a fatal kill with no resync (`spec/wire.md` 2), so
/// one such block would poison every future sync of that wallet forever.
/// Splitting a block is safe because a transaction's position within it is
/// fixed once mined -- which is exactly the promise `exact` resumption
/// makes.
pub const PAGE_BUDGET_BYTES: usize = 512 * 1024;

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

/// Everything that can go wrong turning provider data into wire types.
/// None of these are expected on well-formed Esplora output; they exist so
/// that malformed provider data is an ordinary `Err` (answered with an
/// `internal` envelope error) rather than a panic.
#[derive(Debug)]
pub enum MapError {
    Money(MoneyError),
    Time(Rfc3339Error),
    Json(serde_json::Error),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::Money(e) => write!(f, "money: {e}"),
            MapError::Time(e) => write!(f, "timestamp: byte {}", e.at),
            MapError::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl From<MoneyError> for MapError {
    fn from(e: MoneyError) -> MapError {
        MapError::Money(e)
    }
}
impl From<Rfc3339Error> for MapError {
    fn from(e: Rfc3339Error) -> MapError {
        MapError::Time(e)
    }
}
impl From<serde_json::Error> for MapError {
    fn from(e: serde_json::Error) -> MapError {
        MapError::Json(e)
    }
}

// ---------------------------------------------------------------------
// Esplora payloads
// ---------------------------------------------------------------------

/// One transaction as Esplora returns it, from an address listing -- the
/// only place this adapter reads one from. Unknown fields are ignored on purpose: this is a
/// third-party payload that grows fields between deployments, and
/// rejecting one would take a whole wallet offline over a field we do not
/// read.
#[derive(Debug, Clone, Deserialize)]
pub struct Tx {
    pub txid: String,
    #[serde(default)]
    pub fee: u64,
    pub status: TxStatus,
    #[serde(default)]
    pub vin: Vec<Vin>,
    #[serde(default)]
    pub vout: Vec<Vout>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TxStatus {
    pub confirmed: bool,
    #[serde(default)]
    pub block_height: Option<u64>,
    #[serde(default)]
    pub block_hash: Option<String>,
    #[serde(default)]
    pub block_time: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Vin {
    /// Absent on a coinbase input, which spends nothing.
    #[serde(default)]
    pub prevout: Option<Vout>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Vout {
    #[serde(default)]
    pub scriptpubkey_address: Option<String>,
    pub value: u64,
}

impl Vout {
    fn is_owned(&self, owned: &BTreeSet<String>) -> bool {
        self.scriptpubkey_address
            .as_deref()
            .is_some_and(|a| owned.contains(a))
    }
}

impl Tx {
    /// The height this transaction is confirmed at, or `None` while it is
    /// unconfirmed. A `confirmed: true` status with no `block_height` is
    /// treated as unconfirmed rather than guessed at.
    #[must_use]
    pub fn height(&self) -> Option<u64> {
        if self.status.confirmed {
            self.status.block_height
        } else {
            None
        }
    }
}

/// `GET /address/:address`. Esplora has no balance field at all -- these
/// two counters are the only thing there is to compute one from.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct AddressStats {
    pub chain_stats: Stats,
    pub mempool_stats: Stats,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Stats {
    pub funded_txo_sum: u64,
    pub spent_txo_sum: u64,
}

impl Stats {
    /// Funded minus spent, as a signed value. `mempool_stats` legitimately
    /// goes NEGATIVE when the wallet spends unconfirmed change: the
    /// mempool has already recorded the spend of an output the mempool
    /// also funded, and a later-arriving spend of a *confirmed* output
    /// makes the difference negative outright.
    #[must_use]
    pub fn net(&self) -> i128 {
        i128::from(self.funded_txo_sum) - i128::from(self.spent_txo_sum)
    }
}

// ---------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------

/// `local_id = "<resource_id>:<txid>"`.
///
/// **The `resource_id` prefix is required, not decoration.** The host keys
/// every observation chain by `(adapter_id, local_id)` and NOT by resource
/// (`spec/observation.md` 3). Two of your own wallets paying each other --
/// entirely routine -- share a txid, so a bare-txid `local_id` would merge
/// their chains, so one wallet's records would revise the other wallet's.
#[must_use]
pub fn local_id(resource_id: &str, txid: &str) -> String {
    format!("{resource_id}:{txid}")
}

/// True for a 64-character lowercase hex string. Provider-supplied txids
/// are checked with this before they are interpolated into a request path
/// or a state-file key.
#[must_use]
pub fn is_txid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An integer number of satoshis as an [`Amount`].
///
/// Fallible only in principle -- `"sat"` and `i128::to_string` always
/// satisfy `spec/money.md`'s grammar (in particular `to_string` never
/// produces the signed zero the grammar rejects) -- but the money layer
/// never panics on bad input and neither does this.
pub fn sats(n: i128) -> Result<Amount, MoneyError> {
    Amount::parse(AssetId::new(ASSET_SAT)?, &n.to_string())
}

// ---------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------

/// Unix seconds to an RFC 3339 UTC timestamp.
///
/// Hand-rolled rather than pulling a date crate in for one function:
/// `civil_from_days` below is Howard Hinnant's published algorithm, exact
/// in integer arithmetic for every day in the proleptic Gregorian
/// calendar, and pinned by unit tests against known instants.
pub fn rfc3339_utc(unix_secs: i64) -> Result<Rfc3339, Rfc3339Error> {
    let days = unix_secs.div_euclid(86_400);
    let secs = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    Rfc3339::new(format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z"))
}

/// Days since 1970-01-01 to (year, month, day). Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------

/// The compound, `exact` cursor: `"<height>:<txid>"`, optionally suffixed
/// `":m:<mempool_txid>"`.
///
/// **The confirmed high-water mark is always present**, even mid-mempool.
/// A cursor persisted while the mempool section was still draining still
/// resumes the confirmed section correctly, which a bare mempool token
/// could not. `"0:"` -- height 0, empty txid -- is the bottom: it means
/// "no confirmed transaction has been emitted yet", and it compares below
/// every real `(height, txid)` because a real txid is never empty.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    pub height: u64,
    pub txid: String,
    pub mempool: Option<String>,
}

/// A cursor string this adapter could not have issued.
#[derive(Debug, Clone)]
pub struct CursorError {
    pub cursor: String,
}

impl fmt::Display for CursorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not a cursor this adapter issued: {:?} (expected \"<height>:<txid>\" \
             with an optional \":m:<mempool_txid>\" suffix)",
            self.cursor
        )
    }
}

impl Cursor {
    /// The confirmed high-water mark, ignoring any mempool position.
    #[must_use]
    pub fn confirmed_mark(&self) -> (u64, String) {
        (self.height, self.txid.clone())
    }

    pub fn parse(s: &str) -> Result<Cursor, CursorError> {
        let bad = || CursorError {
            cursor: s.to_owned(),
        };
        let (height, rest) = s.split_once(':').ok_or_else(bad)?;
        let height: u64 = height.parse().map_err(|_| bad())?;
        // A txid is hex, so it can never contain the ":m:" separator: the
        // first occurrence is unambiguously the mempool suffix.
        let (txid, mempool) = match rest.split_once(":m:") {
            Some((t, m)) => (t, Some(m)),
            None => (rest, None),
        };
        if !txid.is_empty() && !is_txid(txid) {
            return Err(bad());
        }
        if mempool.is_some_and(|m| !is_txid(m)) {
            return Err(bad());
        }
        Ok(Cursor {
            height,
            txid: txid.to_owned(),
            mempool: mempool.map(str::to_owned),
        })
    }

    #[must_use]
    pub fn encode(&self) -> String {
        match &self.mempool {
            Some(m) => format!("{}:{}:m:{}", self.height, self.txid, m),
            None => format!("{}:{}", self.height, self.txid),
        }
    }
}

/// Where one observation sits in a sync's total order.
///
/// The derived `Ord` is the emission order and is not incidental: every
/// `Confirmed` sorts before every `Mempool` (the contract's "mempool
/// emitted after all confirmed"), confirmed entries sort ascending by
/// `(height, txid)`, and mempool entries ascending by `txid`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Confirmed { height: u64, txid: String },
    Mempool { txid: String },
}

impl Key {
    /// The cursor that resumes *after* this item. Mempool keys carry the
    /// sync's confirmed high-water mark so that a cursor persisted
    /// mid-mempool still resumes confirmed reads.
    #[must_use]
    pub fn cursor(&self, high_water: &(u64, String)) -> Cursor {
        match self {
            Key::Confirmed { height, txid } => Cursor {
                height: *height,
                txid: txid.clone(),
                mempool: None,
            },
            Key::Mempool { txid } => Cursor {
                height: high_water.0,
                txid: high_water.1.clone(),
                mempool: Some(txid.clone()),
            },
        }
    }
}

// ---------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------

/// The constants one reply's observations share. `observed_at` is passed
/// in rather than read from a clock -- see this module's header.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub resource_id: String,
    pub adapter_id: String,
    pub provider_id: String,
    pub observed_at: Rfc3339,
}

impl Ctx {
    fn provenance(&self, surface: &str, effective_at: Option<Rfc3339>) -> ProvenanceWire {
        ProvenanceWire {
            adapter_id: self.adapter_id.clone(),
            provider_id: self.provider_id.clone(),
            surface: surface.to_owned(),
            observed_at: self.observed_at.clone(),
            effective_at,
            // A wallet-relevant transaction is fully described by the
            // transaction itself; nothing about it is known to be missing.
            completeness: Completeness::Complete,
        }
    }
}

// ---------------------------------------------------------------------
// Balances
// ---------------------------------------------------------------------

/// The two balance lines for one wallet: `confirmed` and `unconfirmed`,
/// never summed and never merged.
///
/// `None` means UNKNOWN and is emitted as `amount: null` -- never `"0"`.
/// A wallet whose balance could not be read has no balance, which is not
/// the same claim as "this wallet holds nothing".
///
/// The category names are this adapter's, and the README says so: Esplora
/// exposes no balance field, so "the provider's verbatim name" has no
/// answer here to be faithful to.
pub fn balance_lines(
    ctx: &Ctx,
    confirmed: Option<i128>,
    unconfirmed: Option<i128>,
) -> Result<Vec<BalanceWire>, MapError> {
    let prov = ctx.provenance(SURFACE_STATS, None);
    let mut out = Vec::with_capacity(2);
    for (category, hint, value) in [
        ("confirmed", CanonicalHint::Confirmed, confirmed),
        ("unconfirmed", CanonicalHint::Unconfirmed, unconfirmed),
    ] {
        out.push(BalanceWire {
            resource_id: ctx.resource_id.clone(),
            category: category.to_owned(),
            canonical_hint: Some(hint),
            amount: value.map(sats).transpose()?,
            provenance: prov.clone(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// History
// ---------------------------------------------------------------------

/// Outputs to the wallet minus inputs from the wallet, in satoshis.
///
/// One observation is one wallet-relevant *transaction*, so this is a
/// single net figure and not one entry per output. A self-transfer between
/// two addresses of the same wallet therefore nets to `-fee`, and a
/// consolidation of one's own outputs to `-fee` as well; a net delta of
/// exactly zero is legal (`spec/money.md` bans only the *signed* zero).
#[must_use]
pub fn net_delta(tx: &Tx, owned: &BTreeSet<String>) -> i128 {
    let received: i128 = tx
        .vout
        .iter()
        .filter(|o| o.is_owned(owned))
        .map(|o| i128::from(o.value))
        .sum();
    let spent: i128 = tx
        .vin
        .iter()
        .filter_map(|i| i.prevout.as_ref())
        .filter(|o| o.is_owned(owned))
        .map(|o| i128::from(o.value))
        .sum();
    received - spent
}

/// True when at least one input of `tx` spends an output of this wallet.
/// The fee is attributable to the wallet only in that case -- a payment
/// *to* the wallet was paid for by whoever sent it.
#[must_use]
pub fn wallet_spent(tx: &Tx, owned: &BTreeSet<String>) -> bool {
    tx.vin
        .iter()
        .filter_map(|i| i.prevout.as_ref())
        .any(|o| o.is_owned(owned))
}

/// `provider_positive` whenever the net delta is >= 0.
///
/// Deterministic, and zero is positive: `raw_sign` records the sign the
/// provider reported, and a zero-delta transaction reports no negative.
#[must_use]
pub fn raw_sign(delta: i128) -> RawSign {
    if delta >= 0 {
        RawSign::ProviderPositive
    } else {
        RawSign::ProviderNegative
    }
}

fn describe(delta: i128) -> String {
    match delta.signum() {
        1 => format!("received {delta} sat"),
        -1 => format!("sent {} sat", delta.unsigned_abs()),
        _ => "self-transfer, net 0 sat".to_owned(),
    }
}

/// One `active` history observation for a wallet-relevant transaction.
pub fn active(ctx: &Ctx, tx: &Tx, owned: &BTreeSet<String>) -> Result<ObservationWire, MapError> {
    let delta = net_delta(tx, owned);
    let height = tx.height();
    let effective_at = tx.status.block_time.map(rfc3339_utc).transpose()?;

    let mut extra = serde_json::Map::new();
    extra.insert("confirmed".to_owned(), tx.status.confirmed.into());
    if let Some(h) = height {
        extra.insert("block_height".to_owned(), h.into());
    }
    if let Some(hash) = &tx.status.block_hash {
        extra.insert("block_hash".to_owned(), hash.clone().into());
    }

    Ok(ObservationWire {
        resource_id: ctx.resource_id.clone(),
        local_id: local_id(&ctx.resource_id, &tx.txid),
        provider_id: Some(tx.txid.clone()),
        supersedes_provider_id: None,
        state: ObservationState::Active,
        tombstone_reason: None,
        surface: SURFACE_TX.to_owned(),
        posting: if height.is_some() {
            Posting::Posted
        } else {
            Posting::Pending
        },
        amount: sats(delta)?,
        // The fee is inside the net delta already (inputs = outputs +
        // fee); it rides beside it as evidence, and only when this wallet
        // is the one that paid it.
        fees: if wallet_spent(tx, owned) {
            Some(sats(i128::from(tx.fee))?)
        } else {
            None
        },
        raw_sign: raw_sign(delta),
        description: describe(delta),
        provider_extra: extra,
        provenance: ctx.provenance(SURFACE_TX, effective_at),
    })
}

// ---------------------------------------------------------------------
// Planning a sync
// ---------------------------------------------------------------------

/// Everything one sync established about the chain, already fetched.
///
/// This is the whole of what a page is planned from: what the provider
/// says NOW. There is no "what it said last time" here, and that is the
/// point -- see ADR 0004 decision 7.
pub struct Chain<'a> {
    /// Every transaction currently known for this wallet: the confirmed
    /// listings and the mempool listings.
    pub txs: &'a BTreeMap<String, Tx>,
    /// Txids the provider currently reports as unconfirmed.
    pub mempool: &'a BTreeSet<String>,
}

/// One sync's full emission list, in order, before it is cut into pages.
pub struct Plan {
    pub items: Vec<(Key, ObservationWire)>,
    /// The confirmed high-water mark this sync reached.
    pub high_water: (u64, String),
}

/// Builds the ordered emission list for one sync.
///
/// Two sections, in this order:
///
/// 1. **Confirmed** -- every confirmed transaction strictly above the
///    resume cursor's `(height, txid)`, ascending.
/// 2. **By txid** -- everything the provider currently reports as
///    unconfirmed, ascending by txid.
///
/// Section 2 is not placed against the confirmed mark -- an unconfirmed
/// transaction has no height to place -- so it is skipped only by the
/// cursor's own `:m:` txid. That is why the "resuming at C returns no
/// confirmed transaction at or below C" invariant exempts this section.
/// It delivers no REVISION: a transaction this crawl reports unconfirmed
/// and a later crawl sees mined at or below the mark is dropped by section
/// 1 and gone from section 2, and only a `history.read` with no `page`
/// brings it back (`adapters/bitcoin/README.md`, "Known limits").
///
/// **Nothing here reads what a previous sync saw.** A transaction the
/// provider no longer lists is simply not in the plan; this adapter does
/// not announce disappearances (ADR 0004 decision 7).
pub fn plan(
    ctx: &Ctx,
    chain: &Chain,
    owned: &BTreeSet<String>,
    from: Option<&Cursor>,
) -> Result<Plan, MapError> {
    let floor = from.map_or_else(|| (0, String::new()), Cursor::confirmed_mark);
    let mut high_water = floor.clone();
    let mut items: Vec<(Key, ObservationWire)> = Vec::new();

    for tx in chain.txs.values() {
        // An unconfirmed transaction has no height and belongs to section
        // 2, which is what keeps it out of this one.
        let Some(height) = tx.height() else { continue };
        let key = (height, tx.txid.clone());
        if key > high_water {
            high_water.clone_from(&key);
        }
        if key <= floor {
            continue;
        }
        items.push((
            Key::Confirmed {
                height,
                txid: tx.txid.clone(),
            },
            active(ctx, tx, owned)?,
        ));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));

    let resume_after = from.and_then(|c| c.mempool.as_deref());
    for txid in chain.mempool {
        if resume_after.is_some_and(|z| txid.as_str() <= z) {
            continue;
        }
        let Some(tx) = chain.txs.get(txid) else {
            continue;
        };
        items.push((Key::Mempool { txid: txid.clone() }, active(ctx, tx, owned)?));
    }

    Ok(Plan { items, high_water })
}

/// The result of cutting a plan into one reply-sized page.
pub struct Page {
    pub observations: Vec<ObservationWire>,
    /// `None` once the plan is drained -- the `exact` terminal state.
    pub next: Option<Cursor>,
    /// Set only when the page cut early, per `spec/observation.md` 5.
    pub page_size_reduced_to: Option<u32>,
    /// Every observation this page dropped for size, per
    /// `spec/observation.md` 6 step 2 -- one entry each, on one status
    /// entry. A single slot kept only the first, and a host cannot exempt
    /// from retraction a record it was never told about.
    pub degraded: Vec<Degraded>,
    /// Serialized bytes of `observations`: what this page spent of the
    /// reply's budget, so the next resource in the same reply knows what
    /// is left.
    pub bytes: usize,
}

/// Cuts a plan at `budget` SERIALIZED BYTES, and runs
/// `spec/observation.md` 6's two-step degrade over what it emits.
///
/// Never at a block boundary: see [`PAGE_BUDGET_BYTES`].
///
/// **The budget is never exceeded, not even by the first observation.** It
/// used to be: one observation was admitted unconditionally so that a page
/// could always make progress. That made the budget advisory, and a reply
/// carrying several resources could then exceed `MAX_FRAME_BYTES` -- a
/// fatal kill with no resync (`spec/wire.md` 2), which is the denial of
/// service the byte-cut exists to prevent, arriving from the other side. A
/// page that cannot afford even one observation emits none and names the
/// point it started from, so the host asks again with the whole budget.
///
/// The degrade is what keeps that from stalling a resource: an observation
/// no page could ever afford is not withheld forever, it is truncated and,
/// failing that, omitted and reported.
pub fn cut_page(plan: Plan, from: Option<&Cursor>, budget: usize) -> Result<Page, MapError> {
    let Plan { items, high_water } = plan;
    let mut observations = Vec::new();
    // The resume cursor names the LAST EMITTED item, never the first
    // withheld one: `exact` promises nothing at or below it is re-sent.
    let mut last_key: Option<Key> = None;
    let mut degraded: Vec<Degraded> = Vec::new();
    let mut used = 0usize;
    let mut cut = false;

    for (key, mut obs) in items {
        let mut size = serde_json::to_vec(&obs)?.len();
        if size > MAX_OBSERVATION_BYTES {
            // Step 1. `provider_extra` is the only field with no bounded
            // shape -- here it is Esplora's `block_hash`, a provider scalar
            // this adapter does not get to assume is small -- so it is the
            // only truncation target. The record itself survives intact.
            let discarded = serde_json::to_vec(&obs.provider_extra)?.len();
            obs.provider_extra = [
                ("_truncated".to_owned(), serde_json::Value::Bool(true)),
                ("_original_bytes".to_owned(), discarded.into()),
            ]
            .into_iter()
            .collect();
            obs.provenance.completeness = Completeness::Partial;
            size = serde_json::to_vec(&obs)?.len();
        }
        if size > MAX_OBSERVATION_BYTES {
            // Step 2. Omit it, report it, and KEEP GOING: one pathological
            // record must never brick a resource. The cursor is not
            // advanced onto it -- it does not need to be, because the page
            // continues and a later emitted item names a resume point above
            // it. EVERY omission is reported, as its own entry on the one
            // status entry this resource gets -- "every requested
            // resource_id appears exactly once" is about status entries,
            // not about how many records one of them may name. Reporting
            // only the first left the rest as unexplained absences, and an
            // unexplained absence is retracted.
            degraded.push(Degraded {
                local_id: Some(obs.local_id.clone()),
                bytes: u64::try_from(size).unwrap_or(u64::MAX),
            });
            continue;
        }
        // Plus the comma that joins it to the previous observation: a
        // budget that ignores JSON punctuation is a budget a thousand
        // observations walk straight through.
        let cost = size.saturating_add(1);
        if used + cost > budget {
            cut = true;
            break;
        }
        used += cost;
        observations.push(obs);
        last_key = Some(key);
    }

    let emitted = u32::try_from(observations.len()).unwrap_or(u32::MAX);
    Ok(Page {
        observations,
        next: cut.then(|| {
            last_key.map_or_else(
                // Nothing was emitted, so nothing new is resumable FROM:
                // the next page starts exactly where this one did.
                || {
                    from.cloned().unwrap_or(Cursor {
                        height: 0,
                        txid: String::new(),
                        mempool: None,
                    })
                },
                |k| k.cursor(&high_water),
            )
        }),
        page_size_reduced_to: cut.then_some(emitted),
        degraded,
        bytes: used,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use sumer_wire::validate_plain_text;

    const ME: &str = "bc1qme";
    const ALSO_ME: &str = "bc1qchange";
    const THEM: &str = "bc1qthem";

    fn owned() -> BTreeSet<String> {
        [ME.to_owned(), ALSO_ME.to_owned()].into_iter().collect()
    }

    fn ctx() -> Ctx {
        Ctx {
            resource_id: "w".to_owned(),
            adapter_id: "sumer-bitcoin".to_owned(),
            provider_id: "https://example.invalid/api".to_owned(),
            observed_at: Rfc3339::new("2026-09-07T00:00:00Z").unwrap(),
        }
    }

    /// `n` as a 64-character hex txid, so ordering by txid is predictable.
    fn txid(n: u32) -> String {
        format!("{n:064x}")
    }

    /// `[(address, value)]` in, `[(address, value)]` out.
    fn tx(
        id: &str,
        height: Option<u64>,
        ins: &[(&str, u64)],
        outs: &[(&str, u64)],
        fee: u64,
    ) -> Tx {
        let status = match height {
            Some(h) => serde_json::json!({
                "confirmed": true,
                "block_height": h,
                "block_hash": format!("{h:064x}"),
                "block_time": 1_600_000_000i64
            }),
            None => serde_json::json!({"confirmed": false}),
        };
        serde_json::from_value(serde_json::json!({
            "txid": id,
            "fee": fee,
            "status": status,
            "vin": ins
                .iter()
                .map(|(a, v)| serde_json::json!({
                    "prevout": {"scriptpubkey_address": a, "value": v}
                }))
                .collect::<Vec<_>>(),
            "vout": outs
                .iter()
                .map(|(a, v)| serde_json::json!({"scriptpubkey_address": a, "value": v}))
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn chain_of(txs: Vec<Tx>) -> (BTreeMap<String, Tx>, BTreeSet<String>) {
        let mempool = txs
            .iter()
            .filter(|t| t.height().is_none())
            .map(|t| t.txid.clone())
            .collect();
        let map = txs.into_iter().map(|t| (t.txid.clone(), t)).collect();
        (map, mempool)
    }

    // -----------------------------------------------------------------
    // Net delta and sign
    // -----------------------------------------------------------------

    #[test]
    fn a_self_transfer_nets_to_minus_the_fee() {
        // 10_000 sat of my own money in, 9_800 back to my own change
        // address, 200 to the miner. One observation, one net figure --
        // not one entry per output.
        let t = tx(
            &txid(1),
            Some(800_000),
            &[(ME, 10_000)],
            &[(ALSO_ME, 9_800)],
            200,
        );
        assert_eq!(net_delta(&t, &owned()), -200);
        assert!(wallet_spent(&t, &owned()));

        let obs = active(&ctx(), &t, &owned()).unwrap();
        assert_eq!(obs.amount.to_string(), "-200");
        assert_eq!(obs.raw_sign, RawSign::ProviderNegative);
        assert_eq!(
            obs.fees.map(|f| f.to_string()),
            Some("200".to_owned()),
            "the wallet paid the fee, so the fee is attributed to it"
        );
    }

    #[test]
    fn a_payment_in_nets_positive_and_carries_no_fee() {
        let t = tx(
            &txid(2),
            Some(800_000),
            &[(THEM, 50_000)],
            &[(ME, 49_000)],
            1_000,
        );
        assert_eq!(net_delta(&t, &owned()), 49_000);
        let obs = active(&ctx(), &t, &owned()).unwrap();
        assert_eq!(obs.raw_sign, RawSign::ProviderPositive);
        assert!(
            obs.fees.is_none(),
            "somebody else paid to send this; the fee is not this wallet's"
        );
        assert_eq!(obs.description, "received 49000 sat");
    }

    #[test]
    fn a_zero_net_delta_is_legal_and_is_provider_positive() {
        // Someone else's input pays the fee; every satoshi of mine comes
        // straight back. money.md bans the SIGNED zero, not zero.
        let t = tx(
            &txid(3),
            Some(800_000),
            &[(ME, 10_000), (THEM, 500)],
            &[(ALSO_ME, 10_000)],
            500,
        );
        assert_eq!(net_delta(&t, &owned()), 0);
        assert_eq!(raw_sign(0), RawSign::ProviderPositive);

        let obs = active(&ctx(), &t, &owned()).unwrap();
        assert_eq!(obs.amount.to_string(), "0", "never \"-0\"");
        assert_eq!(obs.raw_sign, RawSign::ProviderPositive);
        let json = serde_json::to_value(&obs).unwrap();
        assert_eq!(json["amount"]["amount"], "0");
        assert_eq!(json["amount"]["asset"], "sat");
    }

    #[test]
    fn a_coinbase_input_spends_nothing_and_does_not_panic() {
        let t: Tx = serde_json::from_value(serde_json::json!({
            "txid": txid(4),
            "fee": 0,
            "status": {"confirmed": true, "block_height": 1, "block_time": 1_231_006_505i64},
            "vin": [{"is_coinbase": true}],
            "vout": [{"scriptpubkey_address": ME, "value": 5_000_000_000u64}],
        }))
        .unwrap();
        assert_eq!(net_delta(&t, &owned()), 5_000_000_000);
        assert!(!wallet_spent(&t, &owned()));
    }

    #[test]
    fn every_description_is_plain_text() {
        for delta in [-5_i128, 0, 5] {
            validate_plain_text(&describe(delta)).unwrap();
        }
    }

    // -----------------------------------------------------------------
    // Balances
    // -----------------------------------------------------------------

    #[test]
    fn unconfirmed_may_be_negative_and_unknown_is_null_never_zero() {
        let lines = balance_lines(&ctx(), Some(120_000), Some(-4_500)).unwrap();
        let json = serde_json::to_value(&lines).unwrap();
        assert_eq!(json[0]["category"], "confirmed");
        assert_eq!(json[0]["canonical_hint"], "confirmed");
        assert_eq!(json[0]["amount"]["amount"], "120000");
        assert_eq!(json[1]["category"], "unconfirmed");
        assert_eq!(
            json[1]["amount"]["amount"], "-4500",
            "spending unconfirmed change makes this legitimately negative"
        );

        let unknown = serde_json::to_value(balance_lines(&ctx(), None, None).unwrap()).unwrap();
        assert_eq!(unknown[0]["amount"], serde_json::Value::Null);
        assert_eq!(unknown[1]["amount"], serde_json::Value::Null);
    }

    #[test]
    fn stats_net_is_funded_minus_spent() {
        let s: AddressStats = serde_json::from_value(serde_json::json!({
            "chain_stats": {"funded_txo_sum": 10, "spent_txo_sum": 4, "tx_count": 2},
            "mempool_stats": {"funded_txo_sum": 0, "spent_txo_sum": 7, "tx_count": 1},
        }))
        .unwrap();
        assert_eq!(s.chain_stats.net(), 6);
        assert_eq!(s.mempool_stats.net(), -7);
    }

    // -----------------------------------------------------------------
    // Cursor
    // -----------------------------------------------------------------

    #[test]
    fn cursor_round_trips_including_the_mempool_suffix() {
        for text in [
            "0:",
            &format!("800000:{}", txid(1)),
            &format!("800000:{}:m:{}", txid(1), txid(2)),
            &format!("0::m:{}", txid(2)),
        ] {
            let c = Cursor::parse(text).unwrap();
            assert_eq!(c.encode(), *text, "{text} did not round-trip");
            assert_eq!(Cursor::parse(&c.encode()).unwrap(), c);
        }
    }

    #[test]
    fn the_confirmed_mark_survives_a_mempool_suffix() {
        let c = Cursor::parse(&format!("800000:{}:m:{}", txid(1), txid(2))).unwrap();
        assert_eq!(c.confirmed_mark(), (800_000, txid(1)));
        assert_eq!(c.mempool.as_deref(), Some(txid(2).as_str()));
    }

    #[test]
    fn a_cursor_this_adapter_never_issued_is_rejected() {
        for bad in [
            "",
            "800000",
            "not-a-height:abc",
            &format!("800000:{}", "z".repeat(64)),
            &format!("800000:{}:m:short", txid(1)),
        ] {
            assert!(Cursor::parse(bad).is_err(), "must be rejected: {bad:?}");
        }
    }

    #[test]
    fn the_confirmed_section_sorts_before_the_mempool_section() {
        let a = Key::Confirmed {
            height: u64::MAX,
            txid: txid(9),
        };
        let b = Key::Mempool { txid: txid(0) };
        assert!(a < b, "every confirmed item precedes every mempool item");
        let bottom = Cursor::parse("0:").unwrap();
        assert!(bottom < Cursor::parse(&format!("0:{}", txid(0))).unwrap());
        let mark = (800_000u64, txid(1));
        assert!(
            Key::Confirmed {
                height: 800_000,
                txid: txid(1)
            }
            .cursor(&mark)
                < b.cursor(&mark),
            "a mempool cursor sorts above the confirmed mark it carries"
        );
    }

    // -----------------------------------------------------------------
    // Time
    // -----------------------------------------------------------------

    #[test]
    fn unix_seconds_become_rfc3339_utc() {
        for (secs, expected) in [
            (0_i64, "1970-01-01T00:00:00Z"),
            (1_231_006_505, "2009-01-03T18:15:05Z"), // the genesis block
            (1_600_000_000, "2020-09-13T12:26:40Z"),
            (951_782_400, "2000-02-29T00:00:00Z"), // a leap day, in a leap century
            (1_767_225_599, "2025-12-31T23:59:59Z"),
        ] {
            assert_eq!(rfc3339_utc(secs).unwrap().as_str(), expected, "{secs}");
        }
    }

    // -----------------------------------------------------------------
    // Planning and paging
    // -----------------------------------------------------------------

    fn plan_for(
        txs: &BTreeMap<String, Tx>,
        mempool: &BTreeSet<String>,
        from: Option<&Cursor>,
    ) -> Plan {
        let chain = Chain { txs, mempool };
        plan(&ctx(), &chain, &owned(), from).unwrap()
    }

    /// Four transactions, three of them in ONE block. A page must be
    /// allowed to stop in the middle of that block.
    fn split_block_fixture() -> (BTreeMap<String, Tx>, BTreeSet<String>) {
        chain_of(vec![
            tx(&txid(1), Some(799_999), &[(THEM, 1_000)], &[(ME, 900)], 100),
            tx(
                &txid(2),
                Some(800_000),
                &[(THEM, 2_000)],
                &[(ME, 1_900)],
                100,
            ),
            tx(
                &txid(3),
                Some(800_000),
                &[(THEM, 3_000)],
                &[(ME, 2_900)],
                100,
            ),
            tx(
                &txid(4),
                Some(800_000),
                &[(THEM, 4_000)],
                &[(ME, 3_900)],
                100,
            ),
        ])
    }

    #[test]
    fn a_page_cuts_in_the_middle_of_a_block() {
        let (txs, mempool) = split_block_fixture();

        let full = plan_for(&txs, &mempool, None);
        assert_eq!(full.items.len(), 4);
        let one = serde_json::to_vec(&full.items[0].1).unwrap().len();

        // Room for two observations and no more: the cut lands between
        // txid(2) and txid(3), both of which are in block 800_000.
        let page = cut_page(plan_for(&txs, &mempool, None), None, one * 2 + 8).unwrap();
        assert_eq!(page.observations.len(), 2);
        assert_eq!(page.page_size_reduced_to, Some(2));
        let next = page.next.unwrap();
        assert_eq!(
            next.encode(),
            format!("800000:{}", txid(2)),
            "the cursor names the last EMITTED transaction, inside the block"
        );

        let rest = cut_page(
            plan_for(&txs, &mempool, Some(&next)),
            Some(&next),
            PAGE_BUDGET_BYTES,
        )
        .unwrap();
        assert_eq!(rest.observations.len(), 2);
        assert!(rest.next.is_none());
        assert_eq!(
            rest.observations[0].local_id,
            local_id("w", &txid(3)),
            "resumption picks up exactly where the block was split"
        );
    }

    #[test]
    fn cursors_increase_strictly_and_nothing_is_lost_across_a_split_block() {
        let (txs, mempool) = chain_of(vec![
            tx(&txid(1), Some(799_999), &[(THEM, 1_000)], &[(ME, 900)], 100),
            tx(
                &txid(2),
                Some(800_000),
                &[(THEM, 2_000)],
                &[(ME, 1_900)],
                100,
            ),
            tx(
                &txid(3),
                Some(800_000),
                &[(THEM, 3_000)],
                &[(ME, 2_900)],
                100,
            ),
            tx(
                &txid(4),
                Some(800_000),
                &[(THEM, 4_000)],
                &[(ME, 3_900)],
                100,
            ),
            tx(&txid(5), None, &[(THEM, 5_000)], &[(ME, 4_900)], 100),
            tx(&txid(6), None, &[(THEM, 6_000)], &[(ME, 5_900)], 100),
        ]);

        let one = serde_json::to_vec(&plan_for(&txs, &mempool, None).items[0].1)
            .unwrap()
            .len();

        let mut from: Option<Cursor> = None;
        let mut seen_ids: Vec<String> = Vec::new();
        let mut cursors: Vec<Cursor> = Vec::new();
        for _ in 0..10 {
            let page = cut_page(
                plan_for(&txs, &mempool, from.as_ref()),
                from.as_ref(),
                one + 8,
            )
            .unwrap();
            seen_ids.extend(page.observations.iter().map(|o| o.local_id.clone()));
            match page.next {
                None => break,
                Some(c) => {
                    if let Some(prev) = cursors.last() {
                        assert!(*prev < c, "cursor went backwards: {prev:?} then {c:?}");
                    }
                    // Round-trips through the wire on every page, exactly
                    // as the host would carry it.
                    assert_eq!(Cursor::parse(&c.encode()).unwrap(), c);
                    cursors.push(c.clone());
                    from = Some(c);
                }
            }
        }
        let expected: Vec<String> = (1..=6).map(|n| local_id("w", &txid(n))).collect();
        assert_eq!(
            seen_ids, expected,
            "every transaction, exactly once, in order, one per page"
        );
    }

    /// Section 2 -- everything this crawl's provider reports as
    /// unconfirmed -- is emitted regardless of the confirmed mark, which is
    /// what exempts it from the no-confirmed-tx-at-or-below-C invariant: a
    /// pending transaction has no height to compare against that mark.
    #[test]
    fn the_mempool_section_is_emitted_regardless_of_the_cursor() {
        let pending = txid(2);
        let (txs, mempool) = chain_of(vec![tx(
            &pending,
            None,
            &[(THEM, 2_000)],
            &[(ME, 1_900)],
            100,
        )]);
        let from = Cursor::parse(&format!("800000:{}", txid(1))).unwrap();

        let plan = plan_for(&txs, &mempool, Some(&from));
        assert_eq!(plan.items.len(), 1);
        assert!(
            matches!(&plan.items[0].0, Key::Mempool { txid } if *txid == pending),
            "it rides in the by-txid section, which is what exempts it from \
             the no-confirmed-tx-at-or-below-C invariant"
        );
        assert_eq!(plan.items[0].1.posting, Posting::Pending);
        assert_eq!(plan.items[0].1.amount.to_string(), "1900");
        assert_eq!(
            plan.items[0].0.cursor(&plan.high_water).encode(),
            format!("800000:{}:m:{}", txid(1), pending),
            "the confirmed high-water mark rides along, so a cursor persisted \
             mid-mempool still resumes confirmed reads"
        );
    }

    // -----------------------------------------------------------------
    // The frame limit and spec/observation.md section 6's two-step degrade
    // -----------------------------------------------------------------

    /// `block_hash` is a provider-supplied scalar with no bounded length,
    /// and it rides in `provider_extra`. Step 1 of the degrade replaces
    /// that field with a marker; everything else about the observation
    /// survives.
    #[test]
    fn an_oversized_observation_loses_its_provider_extra_and_nothing_else() {
        let mut big = tx(&txid(1), Some(800_000), &[(THEM, 1_000)], &[(ME, 900)], 100);
        big.status.block_hash = Some("f".repeat(100_000));
        let (txs, mempool) = chain_of(vec![big]);
        let page = cut_page(plan_for(&txs, &mempool, None), None, PAGE_BUDGET_BYTES).unwrap();

        assert_eq!(page.observations.len(), 1);
        let obs = &page.observations[0];
        let bytes = serde_json::to_vec(obs).unwrap().len();
        assert!(
            bytes <= MAX_OBSERVATION_BYTES,
            "{bytes} bytes on the wire, over MAX_OBSERVATION_BYTES \
             ({MAX_OBSERVATION_BYTES})"
        );
        assert_eq!(
            obs.provider_extra.len(),
            2,
            "the marker REPLACES provider_extra; it does not sit beside what \
             was discarded"
        );
        assert_eq!(
            obs.provider_extra["_truncated"],
            serde_json::Value::Bool(true)
        );
        assert!(obs.provider_extra["_original_bytes"].as_u64().unwrap() > 100_000);
        assert_eq!(obs.provenance.completeness, Completeness::Partial);
        assert_eq!(
            obs.amount.to_string(),
            "900",
            "provider_extra is the ONLY truncation target"
        );
    }

    /// Step 2: an observation step 1 could not save is OMITTED, reported in
    /// `degraded`, and the page keeps going. One pathological record must
    /// never brick a resource.
    #[test]
    fn an_observation_step_one_cannot_save_is_omitted_and_reported() {
        let small = |n: u32| {
            active(
                &ctx(),
                &tx(&txid(n), Some(800_000), &[(THEM, 1_000)], &[(ME, 900)], 100),
                &owned(),
            )
            .unwrap()
        };
        let mut huge = small(2);
        // Not `provider_extra`, so step 1 cannot rescue it.
        huge.description = "x".repeat(200_000);
        let plan = Plan {
            items: vec![
                (
                    Key::Confirmed {
                        height: 800_000,
                        txid: txid(1),
                    },
                    small(1),
                ),
                (
                    Key::Confirmed {
                        height: 800_000,
                        txid: txid(2),
                    },
                    huge,
                ),
                (
                    Key::Confirmed {
                        height: 800_000,
                        txid: txid(3),
                    },
                    small(3),
                ),
            ],
            high_water: (800_000, txid(3)),
        };

        let page = cut_page(plan, None, PAGE_BUDGET_BYTES).unwrap();
        let ids: Vec<&str> = page
            .observations
            .iter()
            .map(|o| o.local_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![local_id("w", &txid(1)), local_id("w", &txid(3))],
            "every other observation on the page is emitted regardless"
        );
        assert_eq!(
            page.degraded.len(),
            1,
            "the omitted record is reported, never silently dropped"
        );
        assert_eq!(
            page.degraded[0].local_id.as_deref(),
            Some(local_id("w", &txid(2)).as_str())
        );
        assert!(page.degraded[0].bytes > 200_000, "the REAL measured size");
        assert!(
            page.next.is_none(),
            "the plan drained; the resource is readable, not stuck on the record \
             it could not deliver"
        );
    }

    /// A page cut against a budget smaller than its first observation emits
    /// nothing -- and still names where to resume, or the crawl is lost.
    /// Admitting the first observation unconditionally is what lets two
    /// resources in one reply overflow one frame.
    #[test]
    fn a_page_never_spends_more_than_its_budget() {
        let (txs, mempool) = split_block_fixture();
        let page = cut_page(plan_for(&txs, &mempool, None), None, 10).unwrap();
        let spent: usize = page
            .observations
            .iter()
            .map(|o| serde_json::to_vec(o).unwrap().len())
            .sum();
        assert!(
            spent <= 10,
            "a page spent {spent} bytes of a 10-byte budget"
        );
        assert_eq!(
            page.next.map(|c| c.encode()),
            Some("0:".to_owned()),
            "nothing was emitted and the plan is not drained: resumption is from \
             where this page started"
        );
    }

    #[test]
    fn a_confirmed_transaction_at_or_below_the_cursor_is_not_reemitted() {
        let (txs, mempool) = split_block_fixture();
        let from = Cursor::parse(&format!("800000:{}", txid(3))).unwrap();
        let plan = plan_for(&txs, &mempool, Some(&from));
        let ids: Vec<&str> = plan
            .items
            .iter()
            .map(|(_, o)| o.local_id.as_str())
            .collect();
        assert_eq!(ids, vec![local_id("w", &txid(4))]);
    }

    #[test]
    fn the_mempool_section_resumes_after_its_own_txid() {
        let (txs, mempool) = chain_of(vec![
            tx(&txid(5), None, &[(THEM, 5_000)], &[(ME, 4_900)], 100),
            tx(&txid(6), None, &[(THEM, 6_000)], &[(ME, 5_900)], 100),
        ]);
        let from = Cursor::parse(&format!("0::m:{}", txid(5))).unwrap();
        let plan = plan_for(&txs, &mempool, Some(&from));
        let ids: Vec<&str> = plan
            .items
            .iter()
            .map(|(_, o)| o.local_id.as_str())
            .collect();
        assert_eq!(ids, vec![local_id("w", &txid(6))]);
    }

    #[test]
    fn local_id_is_namespaced_by_resource() {
        // Two of your own wallets in one transaction. Merged chains here
        // would let one wallet's records revise the other's.
        assert_ne!(local_id("cold", &txid(1)), local_id("hot", &txid(1)));
        assert_eq!(local_id("cold", &txid(1)), format!("cold:{}", txid(1)));
    }
}
