//! `seen.json`: the only thing that lets this adapter emit a tombstone,
//! and the only thing that lets it answer `stale`.
//!
//! One file per resource under `--state-dir`. It records what the previous
//! *completed* sync established: the wallet's balances, and every
//! transaction it knew about with the height it was at (or the mempool).
//!
//! Four rules this module exists to enforce:
//!
//! 1. **A missing, unreadable, unparseable, version-mismatched or
//!    hash-mismatched file is a FIRST RUN.** Never a partial parse, never
//!    a best-effort salvage -- and *partial* includes a syntactically
//!    valid file with a section half there: a dated `history` with no
//!    `txs` is a first run, not a completed crawl that remembers nothing.
//!    A section absent ENTIRELY is a different thing and stays legal --
//!    it says nothing of that kind was ever recorded, which is a first
//!    run for that half by construction.
//!
//!    A partially-recovered state file would let the adapter conclude
//!    that transactions it simply failed to read are gone -- writing
//!    fiction into an append-only chain that never forgets it. Zero
//!    remembered transactions means zero tombstones, which is
//!    always safe: the worst case is that a real vanish is noticed one
//!    sync later.
//! 2. **Writes are atomic** (temp file + rename). A torn file would be
//!    unparseable, which rule 1 turns into a first run rather than a
//!    corrupt baseline.
//! 3. **Concurrent writers merge; they never overwrite wholesale.** Two
//!    adapter processes syncing the same wallet used to be
//!    last-writer-wins, on the theory that a lost update degrades to a
//!    missed tombstone. It does not. B records a transaction that arrived
//!    after A's crawl began; A then writes its own snapshot, which does not
//!    contain it; the txid is now in nobody's baseline, so nothing ever
//!    probes it and no tombstone is ever emitted for it. That loss is
//!    PERMANENT, and it is the one failure mode the positive-evidence rule
//!    cannot absorb. So a write keeps every txid on disk that it did not
//!    itself prove gone, and the read-modify-write runs under an advisory
//!    lock on `<resource_id>.lock` -- held for a file read and a rename,
//!    never across a network fetch, and released by the kernel if the
//!    process dies, so there is no lock to go stale. A lock that cannot
//!    be taken FAILS THE WRITE. Proceeding unlocked would put both
//!    writers back on the same baseline and let the second rename erase
//!    the first's additions, which is the permanent silence this rule
//!    exists to prevent; the sync still reports every observation it
//!    read, and the next one retries the baseline.
//! 4. **Balances and history are stamped separately.** A history read
//!    observes no balance, so it may not restamp one: a `stale { as_of }`
//!    answer carries the instant the figure it is reporting was actually
//!    observed, never the instant something else was.

use crate::map::{SeenTx, SeenTxs};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use sumer_wire::Rfc3339;

/// Bumped whenever the on-disk shape changes. An older or newer number is
/// a first run, not a migration.
///
/// 2: `as_of` split into `balances.as_of` and `history.as_of`. A schema-1
/// file has one timestamp covering both, and there is no way to tell which
/// read set it, so it is a first run rather than a guess.
const SCHEMA: u32 = 2;

/// What the previous completed sync left behind.
///
/// **Two independently-stamped halves.** `balances_as_of` is when the
/// balances were observed and `history_as_of` is when the transaction
/// baseline was; nothing but a balance read moves the first and nothing but
/// a history read moves the second. They used to be one field, which meant
/// a history read restamped balances it never fetched -- and the next
/// failed balance read then reported yesterday's amounts under today's
/// date, which is a false freshness claim about money.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// `None` == no balance has ever been recorded: a failed read reports
    /// `unavailable`, not `stale`.
    pub balances_as_of: Option<Rfc3339>,
    pub confirmed: Option<i128>,
    pub unconfirmed: Option<i128>,
    /// `None` == no completed crawl: no tombstones, and no `stale` answer.
    pub history_as_of: Option<Rfc3339>,
    pub txs: SeenTxs,
}

// The on-disk shape. Kept private: nothing outside this module should be
// able to construct a half-valid state file.

#[derive(Serialize, Deserialize)]
struct SeenFile {
    schema: u32,
    local_id_derivation: String,
    address_set_sha256: String,
    #[serde(default)]
    balances: BalancesFile,
    #[serde(default)]
    history: HistoryFile,
}

#[derive(Serialize, Deserialize, Default)]
struct BalancesFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    as_of: Option<String>,
    /// `null` is a recorded UNKNOWN, which is exactly what an absent one
    /// degrades to -- so unlike `history.txs` below, there is no shape
    /// here that reads as more knowledge than the file holds.
    #[serde(default)]
    confirmed: Option<String>,
    #[serde(default)]
    unconfirmed: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct HistoryFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    as_of: Option<String>,
    /// Required, not defaulted: `{"as_of": ...}` with no `txs` would
    /// otherwise load as a completed crawl that remembers no
    /// transactions, and every one it forgot would be a tombstone nobody
    /// ever emits.
    txs: BTreeMap<String, SeenTxFile>,
}

#[derive(Serialize, Deserialize, Clone)]
struct SeenTxFile {
    /// Absent == the transaction was in the mempool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    height: Option<u64>,
    /// Decimal string, like every other amount in this system: what the
    /// transaction was worth to this wallet when it was last seen. A
    /// tombstone carries this figure, so a retraction states the amount it
    /// retracts instead of a fabricated zero.
    delta: String,
}

/// Where per-resource state lives. `None` means no `--state-dir` was
/// given: every sync is then a first run and nothing is written.
#[derive(Debug, Clone)]
pub struct Store {
    dir: Option<PathBuf>,
}

impl Store {
    #[must_use]
    pub fn new(dir: Option<PathBuf>) -> Store {
        if dir.is_none() {
            eprintln!(
                "sumer-bitcoin-adapter: no --state-dir; every sync is a first run, \
                 so no tombstone will ever be emitted and a failed read reports \
                 `unavailable` rather than `stale`"
            );
        }
        Store { dir }
    }

    fn path(&self, resource_id: &str) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|d| d.join(format!("{resource_id}.json")))
    }

    /// Loads one resource's state, degrading to a first run on ANY doubt.
    /// `address_hash` is the SHA-256 of the wallet's address set: a
    /// different one means this file describes a different wallet.
    #[must_use]
    pub fn load(&self, resource_id: &str, address_hash: &str) -> Seen {
        let Some(path) = self.path(resource_id) else {
            return Seen::default();
        };
        let Some(file) = read_file(&path, address_hash) else {
            return Seen::default();
        };
        decode(&path, file).unwrap_or_default()
    }

    /// Records the balances a balance read observed, and NOTHING else: the
    /// transaction baseline and its own timestamp are left exactly as they
    /// are on disk.
    pub fn save_balances(
        &self,
        resource_id: &str,
        address_hash: &str,
        as_of: &Rfc3339,
        confirmed: Option<i128>,
        unconfirmed: Option<i128>,
    ) -> io::Result<()> {
        self.update(resource_id, address_hash, |file| {
            file.balances = BalancesFile {
                as_of: Some(as_of.as_str().to_owned()),
                confirmed: confirmed.map(|v| v.to_string()),
                unconfirmed: unconfirmed.map(|v| v.to_string()),
            };
        })
    }

    /// Records the transaction baseline a completed crawl established, and
    /// NOTHING else -- in particular not the balances' timestamp, which
    /// this read did not observe.
    ///
    /// `retracted` is what THIS crawl EMITTED a tombstone for -- not what
    /// it found gone. A tombstone the page omitted for size was never
    /// reported, so its txid is not in here and stays in the baseline for
    /// the next crawl to probe again.
    ///
    /// Everything else already on disk survives: another process may have
    /// recorded a transaction this crawl started too early to see, and
    /// dropping it would leave the txid in nobody's baseline, never probed
    /// again, permanently.
    pub fn save_history(
        &self,
        resource_id: &str,
        address_hash: &str,
        as_of: &Rfc3339,
        txs: &SeenTxs,
        retracted: &BTreeSet<String>,
    ) -> io::Result<()> {
        self.update(resource_id, address_hash, |file| {
            let mut merged: BTreeMap<String, SeenTxFile> = txs
                .iter()
                .map(|(txid, s)| {
                    (
                        txid.clone(),
                        SeenTxFile {
                            height: s.height,
                            delta: s.delta.to_string(),
                        },
                    )
                })
                .collect();
            for (txid, entry) in std::mem::take(&mut file.history.txs) {
                if !merged.contains_key(&txid) && !retracted.contains(&txid) {
                    merged.insert(txid, entry);
                }
            }
            file.history = HistoryFile {
                as_of: Some(as_of.as_str().to_owned()),
                txs: merged,
            };
        })
    }

    /// Read, modify one section, write -- under an advisory lock, so two
    /// processes cannot interleave the read and the write and lose each
    /// other's half. No network call happens inside it: the lock is held
    /// for a file read and a rename.
    fn update(
        &self,
        resource_id: &str,
        address_hash: &str,
        edit: impl FnOnce(&mut SeenFile),
    ) -> io::Result<()> {
        let (Some(dir), Some(path)) = (self.dir.as_ref(), self.path(resource_id)) else {
            return Ok(());
        };
        std::fs::create_dir_all(dir)?;
        let _guard = lock(&dir.join(format!("{resource_id}.lock")))?;
        let mut file = read_file(&path, address_hash).unwrap_or_else(|| SeenFile {
            schema: SCHEMA,
            local_id_derivation: crate::map::LOCAL_ID_DERIVATION.to_owned(),
            address_set_sha256: address_hash.to_owned(),
            balances: BalancesFile::default(),
            history: HistoryFile::default(),
        });
        edit(&mut file);
        write_atomic(&path, &serde_json::to_vec_pretty(&file)?)
    }
}

/// The exclusive advisory lock a read-modify-write runs under. It is held
/// by the returned open file and released when that is dropped -- or by the
/// kernel if the process dies, which is why this is a `flock` on a file
/// rather than a lock file with a pid in it: there is no stale lock that
/// can wedge a wallet.
///
/// A filesystem that cannot take it (some network mounts) FAILS THE
/// WRITE. The fallback used to be an unlocked read-modify-write, on the
/// grounds that the merge is what makes the guarantee and the lock only
/// narrows the window -- which is wrong: without the lock both writers
/// read the same baseline, each merges its own additions into it, and the
/// second rename erases the first's. That is the permanent silence the
/// merge exists to prevent, restored in full. Atomicity here is necessary,
/// not merely a narrower window.
///
/// The cost is bounded and the right way round: a sync that cannot take
/// the lock still reports every observation it read, and only declines to
/// move the baseline, so the next sync re-derives it. A lost update is
/// acceptable; a silently lost retraction is not.
fn lock(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .and_then(|file| file.lock().map(|()| file))
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "{}: could not take the state lock ({e}); refusing to update the \
                     baseline unlocked, because a second writer would erase this \
                     one's additions",
                    path.display()
                ),
            )
        })
}

/// Reads the file at `path` if it is a state file for THIS wallet, and
/// `None` -- a first run -- on any doubt at all.
fn read_file(path: &Path, address_hash: &str) -> Option<SeenFile> {
    let first_run = |why: &str| -> Option<SeenFile> {
        eprintln!(
            "sumer-bitcoin-adapter: {}: {why}; treating this as a first run \
             (zero tombstones)",
            path.display()
        );
        None
    };
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        // Absent is the ordinary first run and is not worth a line of
        // stderr on every fresh install.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => return first_run(&format!("unreadable ({e})")),
    };
    let file: SeenFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        Err(e) => return first_run(&format!("unparseable ({e})")),
    };
    if file.schema != SCHEMA {
        return first_run(&format!("schema {} is not {SCHEMA}", file.schema));
    }
    if file.local_id_derivation != crate::map::LOCAL_ID_DERIVATION {
        return first_run(&format!(
            "local_id_derivation {:?} is not {:?}",
            file.local_id_derivation,
            crate::map::LOCAL_ID_DERIVATION
        ));
    }
    if file.address_set_sha256 != address_hash {
        // ADR 0004: the address set changed. For tombstone purposes
        // this is a different wallet and a first run. It ALSO means a
        // host-held cursor is stale -- adding an address puts history
        // below that cursor, which `exact` forbids re-emitting and
        // this adapter has no channel to invalidate. PR 4's cursor
        // persistence must invalidate on this hash.
        return first_run("the address set changed");
    }
    Some(file)
}

/// The on-disk shape as this adapter uses it. `None` on any unreadable
/// field: a partially-recovered baseline is what rule 1 forbids.
fn decode(path: &Path, file: SeenFile) -> Option<Seen> {
    let first_run = |why: String| -> Option<Seen> {
        eprintln!(
            "sumer-bitcoin-adapter: {}: {why}; treating this as a first run \
             (zero tombstones)",
            path.display()
        );
        None
    };
    let stamp = |v: Option<String>| match v {
        None => Ok(None),
        Some(s) => Rfc3339::new(s.clone()).map(Some).map_err(|_| s),
    };
    let (balances_as_of, history_as_of) =
        match (stamp(file.balances.as_of), stamp(file.history.as_of)) {
            (Ok(b), Ok(h)) => (b, h),
            (Err(bad), _) | (_, Err(bad)) => {
                return first_run(format!("as_of {bad:?} is not RFC 3339"))
            }
        };

    let mut txs = SeenTxs::new();
    for (txid, entry) in file.history.txs {
        let Ok(delta) = entry.delta.parse::<i128>() else {
            return first_run(format!(
                "tx {txid}: delta {:?} is not an integer",
                entry.delta
            ));
        };
        txs.insert(
            txid,
            SeenTx {
                height: entry.height,
                delta,
            },
        );
    }
    let parse_balance = |v: &Option<String>| -> Result<Option<i128>, ()> {
        match v {
            None => Ok(None),
            Some(s) => s.parse::<i128>().map(Some).map_err(|_| ()),
        }
    };
    let (Ok(confirmed), Ok(unconfirmed)) = (
        parse_balance(&file.balances.confirmed),
        parse_balance(&file.balances.unconfirmed),
    ) else {
        return first_run("a recorded balance is not an integer".to_owned());
    };

    Some(Seen {
        balances_as_of,
        confirmed,
        unconfirmed,
        history_as_of,
        txs,
    })
}

/// Temp file in the same directory, then rename. Same directory matters:
/// `rename` is only atomic within one filesystem.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::map::{self, Chain, Ctx, Cursor, Tx};
    use std::collections::{BTreeMap, BTreeSet};
    use sumer_wire::ObservationState;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sumer-btc-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx() -> Ctx {
        Ctx {
            resource_id: "w".to_owned(),
            adapter_id: "sumer-bitcoin".to_owned(),
            provider_id: "test".to_owned(),
            observed_at: Rfc3339::new("2026-09-07T00:00:00Z").unwrap(),
        }
    }

    fn owned() -> BTreeSet<String> {
        ["bc1qme".to_owned()].into_iter().collect()
    }

    fn tx(txid: &str, height: Option<u64>, to_me: u64) -> Tx {
        let status = match height {
            Some(h) => serde_json::json!({
                "confirmed": true, "block_height": h,
                "block_hash": "0".repeat(64), "block_time": 1_600_000_000i64
            }),
            None => serde_json::json!({"confirmed": false}),
        };
        serde_json::from_value(serde_json::json!({
            "txid": txid,
            "fee": 200,
            "status": status,
            "vin": [],
            "vout": [{"scriptpubkey_address": "bc1qme", "value": to_me}],
        }))
        .unwrap()
    }

    fn plan_of(
        txs: &BTreeMap<String, Tx>,
        mempool: &BTreeSet<String>,
        gone: &BTreeSet<String>,
        seen: &SeenTxs,
        from: Option<&Cursor>,
    ) -> Vec<(String, ObservationState, Option<String>)> {
        let chain = Chain { txs, mempool, gone };
        map::plan(&ctx(), &chain, &owned(), seen, from)
            .unwrap()
            .items
            .into_iter()
            .map(|(_, o)| (o.local_id, o.state, o.tombstone_reason))
            .collect()
    }

    /// The full lifecycle the contract names: a transaction appears in the
    /// mempool, confirms, is proved gone by a direct probe, and comes back.
    #[test]
    fn appear_confirm_vanish_revive() {
        let a = "a".repeat(64);
        let dir = tmpdir("lifecycle");
        let store = Store::new(Some(dir.clone()));
        let hash = "hash-of-the-address-set";
        let as_of = Rfc3339::new("2026-09-07T00:00:00Z").unwrap();

        // 1. APPEAR: unconfirmed, first run, no state on disk.
        let seen0 = store.load("w", hash);
        assert!(seen0.txs.is_empty(), "first run remembers nothing");
        let txs: BTreeMap<String, Tx> = [(a.clone(), tx(&a, None, 5_000))].into_iter().collect();
        let mempool: BTreeSet<String> = [a.clone()].into_iter().collect();
        let none = BTreeSet::new();
        let emitted = plan_of(&txs, &mempool, &none, &seen0.txs, None);
        assert_eq!(
            emitted,
            vec![("w:".to_owned() + &a, ObservationState::Active, None)]
        );
        let next = map::next_seen(
            &Chain {
                txs: &txs,
                mempool: &mempool,
                gone: &none,
            },
            &owned(),
            &seen0.txs,
        );
        store
            .save_balances("w", hash, &as_of, Some(0), Some(5_000))
            .unwrap();
        store.save_history("w", hash, &as_of, &next, &none).unwrap();

        // 2. CONFIRM: the tracked mempool entry is re-emitted at its new
        // state -- as a revision of the same local_id, not a new record.
        let seen1 = store.load("w", hash);
        assert_eq!(seen1.txs.get(&a).unwrap().height, None);
        assert_eq!(seen1.unconfirmed, Some(5_000));
        let txs: BTreeMap<String, Tx> = [(a.clone(), tx(&a, Some(800_000), 5_000))]
            .into_iter()
            .collect();
        let emitted = plan_of(&txs, &none, &none, &seen1.txs, None);
        assert_eq!(
            emitted,
            vec![("w:".to_owned() + &a, ObservationState::Active, None)]
        );
        let next = map::next_seen(
            &Chain {
                txs: &txs,
                mempool: &none,
                gone: &none,
            },
            &owned(),
            &seen1.txs,
        );
        store
            .save_balances("w", hash, &as_of, Some(5_000), Some(0))
            .unwrap();
        store.save_history("w", hash, &as_of, &next, &none).unwrap();

        // 3. VANISH: absent from every listing AND a direct probe 404s.
        let seen2 = store.load("w", hash);
        assert_eq!(seen2.txs.get(&a).unwrap().height, Some(800_000));
        let gone: BTreeSet<String> = [a.clone()].into_iter().collect();
        let emitted = plan_of(&BTreeMap::new(), &none, &gone, &seen2.txs, None);
        assert_eq!(
            emitted,
            vec![(
                "w:".to_owned() + &a,
                ObservationState::Tombstoned,
                Some("reorged_out".to_owned())
            )],
            "a previously-confirmed transaction proved gone is reorged_out"
        );
        let next = map::next_seen(
            &Chain {
                txs: &BTreeMap::new(),
                mempool: &none,
                gone: &gone,
            },
            &owned(),
            &seen2.txs,
        );
        assert!(
            next.is_empty(),
            "a tombstoned transaction stops being tracked"
        );
        store.save_history("w", hash, &as_of, &next, &gone).unwrap();
        assert!(
            store.load("w", hash).txs.is_empty(),
            "and the merge does not bring it back: this write PROVED it gone"
        );

        // 4. REVIVE: re-mined. Tombstone is not terminal.
        let seen3 = store.load("w", hash);
        let txs: BTreeMap<String, Tx> = [(a.clone(), tx(&a, Some(800_001), 5_000))]
            .into_iter()
            .collect();
        let emitted = plan_of(&txs, &none, &none, &seen3.txs, None);
        assert_eq!(
            emitted,
            vec![("w:".to_owned() + &a, ObservationState::Active, None)],
            "a re-mined transaction is active again on the same local_id"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Absence from a listing, on its own, is NEVER evidence.
    #[test]
    fn absence_without_a_probe_is_not_a_tombstone() {
        let a = "b".repeat(64);
        let seen: SeenTxs = [(
            a.clone(),
            SeenTx {
                height: Some(700_000),
                delta: 1,
            },
        )]
        .into_iter()
        .collect();
        let none = BTreeSet::new();
        let emitted = plan_of(&BTreeMap::new(), &none, &none, &seen, None);
        assert!(
            emitted.is_empty(),
            "no probe, no tombstone -- and nothing invented in its place"
        );
    }

    #[test]
    fn a_dropped_mempool_transaction_is_not_a_reorg() {
        let a = "c".repeat(64);
        let seen: SeenTxs = [(
            a.clone(),
            SeenTx {
                height: None,
                delta: -42,
            },
        )]
        .into_iter()
        .collect();
        let gone: BTreeSet<String> = [a.clone()].into_iter().collect();
        let none = BTreeSet::new();
        let chain = Chain {
            txs: &BTreeMap::new(),
            mempool: &none,
            gone: &gone,
        };
        let items = map::plan(&ctx(), &chain, &owned(), &seen, None)
            .unwrap()
            .items;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].1.tombstone_reason.as_deref(),
            Some("dropped_from_mempool")
        );
        assert_eq!(
            items[0].1.amount.to_string(),
            "-42",
            "the tombstone carries the amount it retracts, not a fabricated zero"
        );
    }

    #[test]
    fn corrupt_state_is_a_first_run_never_a_partial_parse() {
        let dir = tmpdir("corrupt");
        let store = Store::new(Some(dir.clone()));
        let path = dir.join("w.json");

        for bad in [
            "{".to_owned(),
            serde_json::json!({
                "schema": 99, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "h",
                "history": {"as_of": "2026-01-01T00:00:00Z", "txs": {"x": {"delta": "1"}}}
            })
            .to_string(),
            // Schema 1: one `as_of` covering both halves, and no way to
            // tell which read set it.
            serde_json::json!({
                "schema": 1, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "h", "as_of": "2026-01-01T00:00:00Z",
                "txs": {"x": {"delta": "1"}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 2, "local_id_derivation": "something-else",
                "address_set_sha256": "h",
                "history": {"as_of": "2026-01-01T00:00:00Z", "txs": {"x": {"delta": "1"}}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 2, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "A DIFFERENT WALLET",
                "history": {"as_of": "2026-01-01T00:00:00Z", "txs": {"x": {"delta": "1"}}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 2, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "h",
                "history": {"as_of": "2026-01-01T00:00:00Z", "txs": {"x": {"delta": "no"}}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 2, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "h",
                "balances": {"as_of": "the day before yesterday", "confirmed": "1"}
            })
            .to_string(),
            // A section that is present but INCOMPLETE. Atomic rename does
            // not produce these, but rule 1 is a claim about every file
            // this adapter will ever read, not only the ones it wrote: a
            // dated history with no `txs` would otherwise load as a
            // baseline that remembers nothing, which is a partial parse
            // wearing a completed crawl's timestamp.
            serde_json::json!({
                "schema": 2, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "h",
                "history": {"as_of": "2026-01-01T00:00:00Z"}
            })
            .to_string(),
        ] {
            std::fs::write(&path, &bad).unwrap();
            let seen = store.load("w", "h");
            assert!(seen.history_as_of.is_none(), "first run: {bad}");
            assert!(seen.balances_as_of.is_none(), "first run: {bad}");
            assert!(seen.confirmed.is_none(), "no partial parse: {bad}");
            assert!(seen.txs.is_empty(), "no partial parse: {bad}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_round_trips() {
        let dir = tmpdir("roundtrip");
        let store = Store::new(Some(dir.clone()));
        let as_of = Rfc3339::new("2026-09-07T11:22:33Z").unwrap();
        let a = "d".repeat(64);
        let txs: SeenTxs = [(
            a.clone(),
            SeenTx {
                height: Some(1),
                delta: -9,
            },
        )]
        .into_iter()
        .collect();
        store
            .save_balances("w", "h", &as_of, Some(2_100_000_000_000_000), Some(-1_234))
            .unwrap();
        store
            .save_history("w", "h", &as_of, &txs, &BTreeSet::new())
            .unwrap();
        let back = store.load("w", "h");
        assert_eq!(
            back.balances_as_of.map(|t| t.as_str().to_owned()),
            Some("2026-09-07T11:22:33Z".to_owned())
        );
        assert_eq!(
            back.history_as_of.map(|t| t.as_str().to_owned()),
            Some("2026-09-07T11:22:33Z".to_owned())
        );
        assert_eq!(back.confirmed, Some(2_100_000_000_000_000));
        assert_eq!(
            back.unconfirmed,
            Some(-1_234),
            "a negative unconfirmed balance survives"
        );
        assert_eq!(back.txs.get(&a).unwrap().delta, -9);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Two adapter processes syncing the same wallet. B records a
    /// transaction A's crawl predates; A then writes its own snapshot.
    /// A wholesale overwrite drops B's txid, and since nothing tracks it
    /// any more, nothing ever probes it: the loss is PERMANENT, not the
    /// missed-tombstone-caught-next-sync ADR 0004 5 claims.
    #[test]
    fn a_concurrent_writer_cannot_erase_a_txid_it_never_saw() {
        let dir = tmpdir("concurrent");
        let store = Store::new(Some(dir.clone()));
        let t1 = Rfc3339::new("2026-09-07T00:00:00Z").unwrap();
        let t2 = Rfc3339::new("2026-09-07T00:00:05Z").unwrap();
        let theirs = "f".repeat(64);

        // Process A loaded this baseline, then began a long crawl.
        assert!(store.load("w", "h").txs.is_empty());

        // Process B finished a sync in the meantime and recorded a
        // transaction that arrived after A's crawl had started.
        let b_txs: SeenTxs = [(
            theirs.clone(),
            SeenTx {
                height: None,
                delta: 900,
            },
        )]
        .into_iter()
        .collect();
        store
            .save_history("w", "h", &t1, &b_txs, &BTreeSet::new())
            .unwrap();

        // A now writes what ITS crawl saw -- which does not include it.
        store
            .save_history("w", "h", &t2, &SeenTxs::new(), &BTreeSet::new())
            .unwrap();

        assert!(
            store.load("w", "h").txs.contains_key(&theirs),
            "a txid another process recorded is gone from the baseline, so no \
             future sync will ever probe it: that is a permanent loss, not a \
             missed tombstone"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A state update that cannot take its lock writes NOTHING. The
    /// fallback used to proceed unlocked, which puts two writers back on
    /// the same baseline: each reads it, each adds its own txids, and the
    /// second rename erases the first's additions -- exactly the permanent
    /// silence the merge exists to prevent. Losing an update is
    /// acceptable, silently losing a retraction is not.
    #[test]
    fn a_state_update_that_cannot_lock_writes_nothing() {
        let dir = tmpdir("lockfail");
        let store = Store::new(Some(dir.clone()));
        // A directory where the lock file belongs: it cannot be opened for
        // writing, so the exclusive lock cannot be taken.
        std::fs::create_dir_all(dir.join("w.lock")).unwrap();
        let as_of = Rfc3339::new("2026-09-07T00:00:00Z").unwrap();
        let a = "e".repeat(64);
        let txs: SeenTxs = [(
            a,
            SeenTx {
                height: Some(1),
                delta: 7,
            },
        )]
        .into_iter()
        .collect();

        let err = store
            .save_history("w", "h", &as_of, &txs, &BTreeSet::new())
            .unwrap_err();
        assert!(
            format!("{err}").contains("lock"),
            "the failure must name the lock it could not take: {err}"
        );
        assert!(
            !dir.join("w.json").exists(),
            "a sync that cannot take the lock commits no baseline"
        );
        assert!(store.load("w", "h").txs.is_empty());

        store
            .save_balances("w", "h", &as_of, Some(1), Some(2))
            .unwrap_err();
        assert!(!dir.join("w.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_state_dir_means_no_state() {
        let store = Store::new(None);
        let as_of = Rfc3339::new("2026-09-07T00:00:00Z").unwrap();
        store
            .save_balances("w", "h", &as_of, Some(1), Some(2))
            .unwrap();
        store
            .save_history("w", "h", &as_of, &SeenTxs::new(), &BTreeSet::new())
            .unwrap();
        let back = store.load("w", "h");
        assert!(back.balances_as_of.is_none());
        assert!(back.history_as_of.is_none());
    }
}
