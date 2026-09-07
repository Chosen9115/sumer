//! The store itself: opening a profile database, and the row types the
//! sweep and the CLI read and write.
//!
//! Every read that reconstructs an observation goes back through
//! [`sumer_wire::Observation::stamp`], the same constructor the wire path
//! uses -- there is no second way to build one, so a stored observation
//! and a freshly-read one are the same value with the same invariants.

use rusqlite::{Connection, OptionalExtension, Transaction};
use sumer_money::{Amount, AssetId};
use sumer_wire::{
    Balance, CanonicalHint, Completeness, Observation, ObservationState, Posting, Provenance,
    ProvenanceWire, RawSign, Rfc3339, Staleness,
};

use crate::error::{Result, StoreError};
use crate::profile::Profile;
use crate::schema;

/// An open profile database.
pub struct Store {
    conn: Connection,
    profile: Profile,
}

impl Store {
    /// Creates a profile and its schema. Idempotent on the directory;
    /// refuses to overwrite an existing database.
    pub fn init(profile: &Profile) -> Result<Store> {
        profile.create_dir()?;
        let existed = profile.db_path().exists();
        let conn = Connection::open(profile.db_path())?;
        set_db_mode(profile)?;
        configure(&conn)?;
        if existed && schema::version(&conn)? != 0 {
            return Store::from_open(conn, profile.clone());
        }
        schema::create(&conn)?;
        Ok(Store {
            conn,
            profile: profile.clone(),
        })
    }

    /// Opens an existing profile.
    ///
    /// # Errors
    /// [`StoreError::NoProfile`] if there is no database,
    /// [`StoreError::SchemaVersion`] if it was written by another schema.
    pub fn open(profile: &Profile) -> Result<Store> {
        if !profile.db_path().exists() {
            return Err(StoreError::NoProfile(profile.dir().to_path_buf()));
        }
        let conn = Connection::open(profile.db_path())?;
        configure(&conn)?;
        Store::from_open(conn, profile.clone())
    }

    fn from_open(conn: Connection, profile: Profile) -> Result<Store> {
        let found = schema::version(&conn)?;
        if found != schema::USER_VERSION {
            return Err(StoreError::SchemaVersion {
                path: profile.db_path(),
                found,
                expected: schema::USER_VERSION,
            });
        }
        Ok(Store { conn, profile })
    }

    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    #[must_use]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Begins the one transaction a final page commits inside.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        Ok(self.conn.transaction()?)
    }
}

fn configure(conn: &Connection) -> Result<()> {
    // WAL so a read-only `history` is not blocked by a running refresh;
    // FULL synchronous because the crash boundary this whole design rests
    // on is "committed means committed".
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn set_db_mode(profile: &Profile) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if profile.db_path().exists() {
            std::fs::set_permissions(profile.db_path(), std::fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = profile;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AdapterRow {
    pub adapter_id: String,
    pub argv: Vec<String>,
    pub local_id_derivation: Option<String>,
    pub needs_reauth: bool,
}

#[derive(Debug, Clone)]
pub struct ResourceRow {
    pub adapter_id: String,
    pub resource_id: String,
    pub kind: String,
    pub label: String,
    pub fingerprint: Option<String>,
    pub last_provider_id: Option<String>,
}

/// One stored observation: the wire record, plus the host-side columns
/// that say which sweep put it there and under what software rules.
#[derive(Debug, Clone)]
pub struct StoredObservation {
    pub observation_id: i64,
    pub revision: u64,
    pub crawl_id: i64,
    pub last_seen_crawl: i64,
    pub derivation: String,
    pub fingerprint: Option<String>,
    pub content_hash: String,
    pub observation: Observation,
}

#[derive(Debug, Clone)]
pub struct RetractionRow {
    pub adapter_id: String,
    pub local_id: String,
    pub revision: u64,
    pub reason: String,
    pub crawl_id: i64,
    pub retracted_at: String,
}

#[derive(Debug, Clone)]
pub struct CrawlRow {
    pub crawl_id: i64,
    pub adapter_id: String,
    pub resource_id: String,
    pub started_at: String,
    pub start_page: Option<String>,
    pub next_page: Option<String>,
    pub local_id_derivation: String,
    pub fingerprint: Option<String>,
    pub drained: bool,
    pub complete: bool,
    pub disqualified_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BalanceRow {
    pub balance_id: i64,
    pub adapter_id: String,
    pub resource_id: String,
    pub category: String,
    pub canonical_hint: Option<String>,
    /// `None` = the adapter looked and does not know. Never zero.
    pub amount: Option<Amount>,
    pub provider_id: String,
    pub received_at: String,
    pub staleness: Staleness,
    pub outcome: String,
}

#[derive(Debug, Clone)]
pub struct DiscrepancyRow {
    pub adapter_id: String,
    pub resource_id: String,
    pub kind: String,
    pub crawl_id: i64,
    pub detail: String,
    pub noted_at: String,
}

// ---------------------------------------------------------------------
// Adapters and resources
// ---------------------------------------------------------------------

pub fn upsert_adapter(conn: &Connection, adapter_id: &str, argv: &[String]) -> Result<()> {
    conn.execute(
        "INSERT INTO adapter (adapter_id, argv) VALUES (?1, ?2)
         ON CONFLICT(adapter_id) DO UPDATE SET argv = excluded.argv",
        rusqlite::params![adapter_id, serde_json::to_string(argv)?],
    )?;
    Ok(())
}

pub fn set_adapter_derivation(conn: &Connection, adapter_id: &str, derivation: &str) -> Result<()> {
    conn.execute(
        "UPDATE adapter SET local_id_derivation = ?2 WHERE adapter_id = ?1",
        rusqlite::params![adapter_id, derivation],
    )?;
    Ok(())
}

pub fn set_needs_reauth(conn: &Connection, adapter_id: &str, needs: bool) -> Result<()> {
    conn.execute(
        "UPDATE adapter SET needs_reauth = ?2 WHERE adapter_id = ?1",
        rusqlite::params![adapter_id, i64::from(needs)],
    )?;
    Ok(())
}

pub fn adapters(conn: &Connection) -> Result<Vec<AdapterRow>> {
    let mut stmt = conn.prepare(
        "SELECT adapter_id, argv, local_id_derivation, needs_reauth
         FROM adapter ORDER BY adapter_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (adapter_id, argv, local_id_derivation, needs_reauth) = row?;
        out.push(AdapterRow {
            adapter_id,
            argv: serde_json::from_str(&argv)?,
            local_id_derivation,
            needs_reauth: needs_reauth != 0,
        });
    }
    Ok(out)
}

pub fn upsert_resource(
    conn: &Connection,
    adapter_id: &str,
    resource_id: &str,
    kind: &str,
    label: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO resource (adapter_id, resource_id, kind, label)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(adapter_id, resource_id)
         DO UPDATE SET kind = excluded.kind, label = excluded.label",
        rusqlite::params![adapter_id, resource_id, kind, label],
    )?;
    Ok(())
}

pub fn resource(
    conn: &Connection,
    adapter_id: &str,
    resource_id: &str,
) -> Result<Option<ResourceRow>> {
    Ok(conn
        .query_row(
            "SELECT adapter_id, resource_id, kind, label, fingerprint, last_provider_id
             FROM resource WHERE adapter_id = ?1 AND resource_id = ?2",
            rusqlite::params![adapter_id, resource_id],
            resource_from_row,
        )
        .optional()?)
}

pub fn resources(conn: &Connection, adapter_id: Option<&str>) -> Result<Vec<ResourceRow>> {
    let mut stmt = conn.prepare(
        "SELECT adapter_id, resource_id, kind, label, fingerprint, last_provider_id
         FROM resource
         WHERE ?1 IS NULL OR adapter_id = ?1
         ORDER BY adapter_id, resource_id",
    )?;
    let rows = stmt.query_map(rusqlite::params![adapter_id], resource_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn resource_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ResourceRow> {
    Ok(ResourceRow {
        adapter_id: row.get(0)?,
        resource_id: row.get(1)?,
        kind: row.get(2)?,
        label: row.get(3)?,
        fingerprint: row.get(4)?,
        last_provider_id: row.get(5)?,
    })
}

/// `provider_id` is `None` for a sweep that must not adopt the vantage it
/// read from -- a disqualified one, which derived no retractions and so
/// reconciled nothing. Adopting it there consumes §8.2's vantage exemption
/// without ever recording the `vantage_changed` the exemption exists to
/// pair with, and the next sweep retracts on a move nobody was told about.
pub fn set_resource_definition(
    conn: &Connection,
    adapter_id: &str,
    resource_id: &str,
    fingerprint: &str,
    provider_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE resource SET fingerprint = ?3,
                last_provider_id = COALESCE(?4, last_provider_id)
         WHERE adapter_id = ?1 AND resource_id = ?2",
        rusqlite::params![adapter_id, resource_id, fingerprint, provider_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Crawls
// ---------------------------------------------------------------------

/// Opens a crawl, **retiring every earlier crawl's resume point for this
/// resource**.
///
/// A resume point is only ever read by the next `--resume`, and the crawl
/// being opened here is that next attempt: it either resumed from the
/// cursor it is about to supersede, or it is starting fresh from the
/// beginning of history, and neither leaves anything for an older crawl to
/// continue. Without this the old cursor stays eligible **forever** -- a
/// crawl that committed cursor C and crashed is never finished, so a
/// successful resume that drains from C leaves crawl 1 still `drained = 0`
/// with `next_page = C`, and every later `--resume` selects that same stale
/// C again, for ever. `next_page` is cleared rather than the row: the
/// crawl, its disqualifier and its observations are the audit trail.
///
/// **What the clear costs, stated honestly.** On a `--resume` the point the
/// old crawl stopped at survives as this crawl's `start_page`. On a plain
/// refresh it does not: `start_page` is `None`, so a refresh that dies
/// before its first page commits leaves `--resume` with nothing, and the
/// next run starts from the beginning of history. That is an availability
/// cost and never a correctness one -- a sweep from `page: None` satisfies
/// gate condition (1), where a resumed one does not, so the fallback is
/// strictly stronger evidence than the cursor it lost. The clear and the
/// insert commit together for the same reason: a crash between them would
/// leave the resource with no cursor and no crawl to explain why.
#[allow(clippy::too_many_arguments)]
pub fn open_crawl(
    conn: &Connection,
    adapter_id: &str,
    resource_id: &str,
    started_at: &str,
    start_page: Option<&str>,
    derivation: &str,
    fingerprint: Option<&str>,
) -> Result<i64> {
    let txn = conn.unchecked_transaction()?;
    txn.execute(
        "UPDATE crawl SET next_page = NULL
         WHERE adapter_id = ?1 AND resource_id = ?2 AND next_page IS NOT NULL",
        rusqlite::params![adapter_id, resource_id],
    )?;
    txn.execute(
        "INSERT INTO crawl
           (adapter_id, resource_id, started_at, start_page, next_page,
            local_id_derivation, fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)",
        rusqlite::params![
            adapter_id,
            resource_id,
            started_at,
            start_page,
            derivation,
            fingerprint
        ],
    )?;
    let crawl_id = txn.last_insert_rowid();
    txn.commit()?;
    Ok(crawl_id)
}

pub fn set_crawl_cursor(conn: &Connection, crawl_id: i64, next_page: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE crawl SET next_page = ?2 WHERE crawl_id = ?1",
        rusqlite::params![crawl_id, next_page],
    )?;
    Ok(())
}

pub fn finish_crawl(
    conn: &Connection,
    crawl_id: i64,
    drained: bool,
    complete: bool,
    disqualified_reason: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE crawl
         SET drained = ?2, complete = ?3, disqualified_reason = ?4,
             next_page = CASE WHEN ?2 = 1 THEN NULL ELSE next_page END
         WHERE crawl_id = ?1",
        rusqlite::params![
            crawl_id,
            i64::from(drained),
            i64::from(complete),
            disqualified_reason
        ],
    )?;
    Ok(())
}

/// The crawl a `--resume` would continue: the most recent one for this
/// resource that never drained and still holds a cursor.
pub fn resumable_crawl(
    conn: &Connection,
    adapter_id: &str,
    resource_id: &str,
) -> Result<Option<CrawlRow>> {
    Ok(conn
        .query_row(
            "SELECT crawl_id, adapter_id, resource_id, started_at, start_page, next_page,
                    local_id_derivation, fingerprint, drained, complete, disqualified_reason
             FROM crawl
             WHERE adapter_id = ?1 AND resource_id = ?2 AND drained = 0 AND next_page IS NOT NULL
             ORDER BY crawl_id DESC LIMIT 1",
            rusqlite::params![adapter_id, resource_id],
            crawl_from_row,
        )
        .optional()?)
}

pub fn crawls(conn: &Connection, limit: i64) -> Result<Vec<CrawlRow>> {
    let mut stmt = conn.prepare(
        "SELECT crawl_id, adapter_id, resource_id, started_at, start_page, next_page,
                local_id_derivation, fingerprint, drained, complete, disqualified_reason
         FROM crawl ORDER BY crawl_id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], crawl_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn crawl_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CrawlRow> {
    Ok(CrawlRow {
        crawl_id: row.get(0)?,
        adapter_id: row.get(1)?,
        resource_id: row.get(2)?,
        started_at: row.get(3)?,
        start_page: row.get(4)?,
        next_page: row.get(5)?,
        local_id_derivation: row.get(6)?,
        fingerprint: row.get(7)?,
        drained: row.get::<_, i64>(8)? != 0,
        complete: row.get::<_, i64>(9)? != 0,
        disqualified_reason: row.get(10)?,
    })
}

// ---------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------

const OBSERVATION_COLUMNS: &str = "
    observation_id, revision, crawl_id, last_seen_crawl, derivation, fingerprint,
    content_hash, resource_id, local_id, provider_id, supersedes_provider_id, state,
    tombstone_reason, surface, posting, amount_asset, amount, fees_asset, fees,
    raw_sign, description, provider_extra, prov_adapter_id, prov_provider_id,
    prov_surface, observed_at, effective_at, completeness, received_at, staleness";

/// Every stored observation for one adapter, **in stored order**. This is
/// the order the fold is replayed in (frozen contract): insertion order,
/// which is the order the host actually received them.
///
/// **Adapter-wide, not per-resource, because that is the key the revision
/// counter runs on.** `spec/observation.md` §3 assigns `revision` per
/// `(adapter_id, local_id)` and `schema.rs` enforces
/// `UNIQUE (adapter_id, local_id, revision)`. A fold loaded per
/// `(adapter_id, resource_id)` splits one chain in two whenever a
/// `local_id` is reported under a second `resource_id`: the record is
/// handed revision 1 again, the UNIQUE constraint rejects the insert, and
/// the record **can never revive** -- every later sweep repeats the same
/// collision. §8.4 requires revival, so the fold is loaded on the key the
/// spec keys.
pub fn observations_for_adapter(
    conn: &Connection,
    adapter_id: &str,
) -> Result<Vec<StoredObservation>> {
    let sql = format!(
        "SELECT {OBSERVATION_COLUMNS} FROM observation
         WHERE adapter_id = ?1
         ORDER BY observation_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![adapter_id], |row| {
        Ok(RawObservation::from_row(row))
    })?;
    collect_observations(rows)
}

/// One chain, in stored order.
pub fn chain(
    conn: &Connection,
    adapter_id: &str,
    local_id: &str,
) -> Result<Vec<StoredObservation>> {
    let sql = format!(
        "SELECT {OBSERVATION_COLUMNS} FROM observation
         WHERE adapter_id = ?1 AND local_id = ?2
         ORDER BY observation_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![adapter_id, local_id], |row| {
        Ok(RawObservation::from_row(row))
    })?;
    collect_observations(rows)
}

fn collect_observations<I>(rows: I) -> Result<Vec<StoredObservation>>
where
    I: Iterator<Item = rusqlite::Result<Result<RawObservation>>>,
{
    let mut out = Vec::new();
    for row in rows {
        out.push(row??.into_stored()?);
    }
    Ok(out)
}

/// The raw column values, before they are turned back into a typed
/// observation. Split out so a `query_map` closure (which can only return
/// `rusqlite::Error`) never has to carry a money or JSON failure.
struct RawObservation {
    observation_id: i64,
    revision: i64,
    crawl_id: i64,
    last_seen_crawl: i64,
    derivation: String,
    fingerprint: Option<String>,
    content_hash: String,
    resource_id: String,
    local_id: String,
    provider_id: Option<String>,
    supersedes_provider_id: Option<String>,
    state: String,
    tombstone_reason: Option<String>,
    surface: String,
    posting: String,
    amount_asset: String,
    amount: String,
    fees_asset: Option<String>,
    fees: Option<String>,
    raw_sign: String,
    description: String,
    provider_extra: String,
    prov_adapter_id: String,
    prov_provider_id: String,
    prov_surface: String,
    observed_at: String,
    effective_at: Option<String>,
    completeness: String,
    received_at: String,
    staleness: String,
}

impl RawObservation {
    fn from_row(row: &rusqlite::Row<'_>) -> Result<RawObservation> {
        Ok(RawObservation {
            observation_id: row.get(0)?,
            revision: row.get(1)?,
            crawl_id: row.get(2)?,
            last_seen_crawl: row.get(3)?,
            derivation: row.get(4)?,
            fingerprint: row.get(5)?,
            content_hash: row.get(6)?,
            resource_id: row.get(7)?,
            local_id: row.get(8)?,
            provider_id: row.get(9)?,
            supersedes_provider_id: row.get(10)?,
            state: row.get(11)?,
            tombstone_reason: row.get(12)?,
            surface: row.get(13)?,
            posting: row.get(14)?,
            amount_asset: row.get(15)?,
            amount: row.get(16)?,
            fees_asset: row.get(17)?,
            fees: row.get(18)?,
            raw_sign: row.get(19)?,
            description: row.get(20)?,
            provider_extra: row.get(21)?,
            prov_adapter_id: row.get(22)?,
            prov_provider_id: row.get(23)?,
            prov_surface: row.get(24)?,
            observed_at: row.get(25)?,
            effective_at: row.get(26)?,
            completeness: row.get(27)?,
            received_at: row.get(28)?,
            staleness: row.get(29)?,
        })
    }

    fn into_stored(self) -> Result<StoredObservation> {
        let provenance = Provenance::stamp(
            ProvenanceWire {
                adapter_id: self.prov_adapter_id,
                provider_id: self.prov_provider_id,
                surface: self.prov_surface,
                observed_at: timestamp(&self.observed_at)?,
                effective_at: self.effective_at.as_deref().map(timestamp).transpose()?,
                completeness: from_spelling::<Completeness>(&self.completeness)?,
            },
            timestamp(&self.received_at)?,
            from_spelling::<Staleness>(&self.staleness)?,
        );
        let observation = Observation {
            resource_id: self.resource_id,
            local_id: self.local_id,
            provider_id: self.provider_id,
            supersedes_provider_id: self.supersedes_provider_id,
            state: from_spelling::<ObservationState>(&self.state)?,
            tombstone_reason: self.tombstone_reason,
            surface: self.surface,
            posting: from_spelling::<Posting>(&self.posting)?,
            amount: money(&self.amount_asset, &self.amount)?,
            fees: match (self.fees_asset, self.fees) {
                (Some(asset), Some(text)) => Some(money(&asset, &text)?),
                _ => None,
            },
            raw_sign: from_spelling::<RawSign>(&self.raw_sign)?,
            description: self.description,
            provider_extra: serde_json::from_str(&self.provider_extra)?,
            provenance,
        };
        Ok(StoredObservation {
            observation_id: self.observation_id,
            revision: u64::try_from(self.revision)
                .map_err(|_| StoreError::CorruptRow(format!("revision {}", self.revision)))?,
            crawl_id: self.crawl_id,
            last_seen_crawl: self.last_seen_crawl,
            derivation: self.derivation,
            fingerprint: self.fingerprint,
            content_hash: self.content_hash,
            observation,
        })
    }
}

fn timestamp(text: &str) -> Result<Rfc3339> {
    Rfc3339::new(text).map_err(|e| StoreError::CorruptRow(format!("timestamp {text:?}: {e}")))
}

fn money(asset: &str, text: &str) -> Result<Amount> {
    Ok(Amount::parse(AssetId::new(asset)?, text)?)
}

/// Parses one of the small wire enums back from the spelling
/// [`spelling`] wrote.
fn from_spelling<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_owned()))
        .map_err(|e| StoreError::CorruptRow(format!("enum {text:?}: {e}")))
}

/// The wire spelling of one of the small enums, for storage. These are
/// unit variants with `rename_all = "snake_case"`, so this is the same
/// string that crosses the wire.
fn spelling<T: serde::Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(StoreError::CorruptRow(format!(
            "expected a string spelling, got {other}"
        ))),
    }
}

/// Appends one observation. Callers pass the revision the shared
/// [`sumer_host::fold::Fold`] assigned -- revision assignment has exactly
/// one implementation and this is not it.
pub struct Append<'a> {
    pub adapter_id: &'a str,
    pub revision: u64,
    pub crawl_id: i64,
    pub derivation: &'a str,
    pub fingerprint: Option<&'a str>,
    pub content_hash: &'a str,
    pub observation: &'a Observation,
}

pub fn append_observation(conn: &Connection, append: &Append<'_>) -> Result<()> {
    let o = append.observation;
    conn.execute(
        "INSERT INTO observation (
            adapter_id, resource_id, local_id, revision, crawl_id, last_seen_crawl,
            derivation, fingerprint, content_hash, provider_id, supersedes_provider_id,
            state, tombstone_reason, surface, posting, amount_asset, amount,
            fees_asset, fees, raw_sign, description, provider_extra,
            prov_adapter_id, prov_provider_id, prov_surface, observed_at, effective_at,
            completeness, received_at, staleness
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29
         )",
        rusqlite::params![
            append.adapter_id,
            o.resource_id,
            o.local_id,
            i64::try_from(append.revision).unwrap_or(i64::MAX),
            append.crawl_id,
            append.derivation,
            append.fingerprint,
            append.content_hash,
            o.provider_id,
            o.supersedes_provider_id,
            spelling(&o.state)?,
            o.tombstone_reason,
            o.surface,
            spelling(&o.posting)?,
            o.amount.asset().as_str(),
            o.amount.to_string(),
            o.fees.as_ref().map(|f| f.asset().as_str().to_owned()),
            o.fees.as_ref().map(ToString::to_string),
            spelling(&o.raw_sign)?,
            o.description,
            serde_json::Value::Object(o.provider_extra.clone()).to_string(),
            o.provenance.adapter_id,
            o.provenance.provider_id,
            o.provenance.surface,
            o.provenance.observed_at.as_str(),
            o.provenance.effective_at.as_ref().map(Rfc3339::as_str),
            spelling(&o.provenance.completeness)?,
            o.provenance.received_at.as_str(),
            spelling(&o.provenance.staleness)?,
        ],
    )?;
    Ok(())
}

/// Records that this crawl saw a record it had already stored byte-equal
/// (by `content_hash`). Nothing is appended -- the chain is unchanged --
/// but the store now knows the record is still there, **and under which
/// software rules it was last re-emitted**.
///
/// Updating `derivation`/`fingerprint` here matters for the reason a
/// LATER sweep picks. A record that survived an address change was
/// genuinely re-emitted under the new address set; leaving the old
/// fingerprint on it would make its eventual, ordinary disappearance
/// report `resource_definition_changed` -- blaming a software change that
/// happened three sweeps ago for a record the provider simply stopped
/// reporting.
///
/// **`revision` names the row, and it is the FOLD's head, not the last row
/// inserted.** The two differ: `sumer_host::fold` orders a chain by
/// `(received_at, surface, arrival_index)`, so two observations for one key
/// arriving on one page with surfaces `z` then `a` take revisions 1 and 2
/// while the head is revision 1. Stamping "the highest `observation_id`"
/// then updated a row the fold does not call the head, leaving the real
/// head carrying a stale fingerprint -- and its ordinary disappearance
/// three sweeps later reporting `resource_definition_changed` where
/// `spec/observation.md` §8.3 requires `absent_from_complete_sweep`, or the
/// reverse.
pub fn stamp_seen(
    conn: &Connection,
    adapter_id: &str,
    local_id: &str,
    revision: u64,
    crawl_id: i64,
    derivation: &str,
    fingerprint: Option<&str>,
) -> Result<()> {
    // `UNIQUE (adapter_id, local_id, revision)` makes this exactly one row.
    conn.execute(
        "UPDATE observation
         SET last_seen_crawl = ?4, derivation = ?5, fingerprint = ?6
         WHERE adapter_id = ?1 AND local_id = ?2 AND revision = ?3",
        rusqlite::params![
            adapter_id,
            local_id,
            i64::try_from(revision).unwrap_or(i64::MAX),
            crawl_id,
            derivation,
            fingerprint
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Retractions
// ---------------------------------------------------------------------

pub fn append_retraction(
    conn: &Connection,
    adapter_id: &str,
    local_id: &str,
    revision: u64,
    reason: &str,
    crawl_id: i64,
    retracted_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO retraction (adapter_id, local_id, revision, reason, crawl_id, retracted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            adapter_id,
            local_id,
            i64::try_from(revision).unwrap_or(i64::MAX),
            reason,
            crawl_id,
            retracted_at
        ],
    )?;
    Ok(())
}

/// The highest retraction revision per `(adapter_id, local_id)`. A record
/// is live iff its chain head's revision exceeds this.
pub fn retraction_high_water(
    conn: &Connection,
    adapter_id: &str,
) -> Result<std::collections::HashMap<String, u64>> {
    let mut stmt = conn.prepare(
        "SELECT local_id, MAX(revision) FROM retraction WHERE adapter_id = ?1 GROUP BY local_id",
    )?;
    let rows = stmt.query_map(rusqlite::params![adapter_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (local_id, revision) = row?;
        out.insert(
            local_id,
            u64::try_from(revision)
                .map_err(|_| StoreError::CorruptRow(format!("retraction revision {revision}")))?,
        );
    }
    Ok(out)
}

pub fn retractions_for(
    conn: &Connection,
    adapter_id: &str,
    local_id: &str,
) -> Result<Vec<RetractionRow>> {
    let mut stmt = conn.prepare(
        "SELECT adapter_id, local_id, revision, reason, crawl_id, retracted_at
         FROM retraction WHERE adapter_id = ?1 AND local_id = ?2 ORDER BY retraction_id",
    )?;
    let rows = stmt.query_map(rusqlite::params![adapter_id, local_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (adapter_id, local_id, revision, reason, crawl_id, retracted_at) = row?;
        out.push(RetractionRow {
            adapter_id,
            local_id,
            revision: u64::try_from(revision)
                .map_err(|_| StoreError::CorruptRow(format!("retraction revision {revision}")))?,
            reason,
            crawl_id,
            retracted_at,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Balances
// ---------------------------------------------------------------------

pub fn append_balance(
    conn: &Connection,
    adapter_id: &str,
    balance: &Balance,
    outcome: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO balance (
            adapter_id, resource_id, category, canonical_hint, amount_asset, amount,
            prov_provider_id, prov_surface, observed_at, effective_at, completeness,
            received_at, staleness, outcome
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        rusqlite::params![
            adapter_id,
            balance.resource_id,
            balance.category,
            balance
                .canonical_hint
                .as_ref()
                .map(spelling::<CanonicalHint>)
                .transpose()?,
            balance
                .amount
                .as_ref()
                .map(|a| a.asset().as_str().to_owned()),
            balance.amount.as_ref().map(ToString::to_string),
            balance.provenance.provider_id,
            balance.provenance.surface,
            balance.provenance.observed_at.as_str(),
            balance
                .provenance
                .effective_at
                .as_ref()
                .map(Rfc3339::as_str),
            spelling(&balance.provenance.completeness)?,
            balance.provenance.received_at.as_str(),
            spelling(&balance.provenance.staleness)?,
            outcome,
        ],
    )?;
    Ok(())
}

/// Every balance line ever recorded for one resource, oldest first.
/// Rendering walks backwards through this: a failed read must never erase
/// the last figure, so the printer needs the history, not just the head.
pub fn balance_history(
    conn: &Connection,
    adapter_id: &str,
    resource_id: &str,
) -> Result<Vec<BalanceRow>> {
    let mut stmt = conn.prepare(
        "SELECT balance_id, adapter_id, resource_id, category, canonical_hint,
                amount_asset, amount, prov_provider_id, received_at, staleness, outcome
         FROM balance WHERE adapter_id = ?1 AND resource_id = ?2 ORDER BY balance_id",
    )?;
    let rows = stmt.query_map(rusqlite::params![adapter_id, resource_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            balance_id,
            adapter_id,
            resource_id,
            category,
            canonical_hint,
            amount_asset,
            amount,
            provider_id,
            received_at,
            staleness,
            outcome,
        ) = row?;
        out.push(BalanceRow {
            balance_id,
            adapter_id,
            resource_id,
            category,
            canonical_hint,
            amount: match (amount_asset, amount) {
                (Some(asset), Some(text)) => Some(money(&asset, &text)?),
                _ => None,
            },
            provider_id,
            received_at,
            staleness: from_spelling::<Staleness>(&staleness)?,
            outcome,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Discrepancies
// ---------------------------------------------------------------------

pub fn append_discrepancy(
    conn: &Connection,
    adapter_id: &str,
    resource_id: &str,
    kind: &str,
    crawl_id: i64,
    detail: &str,
    noted_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO discrepancy (adapter_id, resource_id, kind, crawl_id, detail, noted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![adapter_id, resource_id, kind, crawl_id, detail, noted_at],
    )?;
    Ok(())
}

pub fn discrepancies(conn: &Connection, limit: i64) -> Result<Vec<DiscrepancyRow>> {
    let mut stmt = conn.prepare(
        "SELECT adapter_id, resource_id, kind, crawl_id, detail, noted_at
         FROM discrepancy ORDER BY discrepancy_id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![limit], |row| {
        Ok(DiscrepancyRow {
            adapter_id: row.get(0)?,
            resource_id: row.get(1)?,
            kind: row.get(2)?,
            crawl_id: row.get(3)?,
            detail: row.get(4)?,
            noted_at: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
