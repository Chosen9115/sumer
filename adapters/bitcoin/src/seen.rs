//! `seen.json`: the only thing that lets this adapter emit a tombstone,
//! and the only thing that lets it answer `stale`.
//!
//! One file per resource under `--state-dir`. It records what the previous
//! *completed* sync established: the wallet's balances, and every
//! transaction it knew about with the height it was at (or the mempool).
//!
//! Three rules this module exists to enforce:
//!
//! 1. **A missing, unreadable, unparseable, version-mismatched or
//!    hash-mismatched file is a FIRST RUN.** Never a partial parse, never
//!    a best-effort salvage. A partially-recovered state file would let
//!    the adapter conclude that transactions it simply failed to read are
//!    gone -- writing fiction into an append-only chain that never forgets
//!    it. Zero remembered transactions means zero tombstones, which is
//!    always safe: the worst case is that a real vanish is noticed one
//!    sync later.
//! 2. **Writes are atomic** (temp file + rename). A torn file would be
//!    unparseable, which rule 1 turns into a first run rather than a
//!    corrupt baseline.
//! 3. **Concurrent writers are last-writer-wins, deliberately.** Two
//!    adapter processes syncing the same wallet can lose one another's
//!    update. Under the positive-evidence rule that degrades to a MISSED
//!    tombstone, caught on the next sync -- never an invented one. Locking
//!    would buy a stronger guarantee than the failure mode needs.

use crate::map::{SeenTx, SeenTxs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use sumer_wire::Rfc3339;

/// Bumped whenever the on-disk shape changes. An older or newer number is
/// a first run, not a migration.
const SCHEMA: u32 = 1;

/// What the previous completed sync left behind.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// When the recorded answer was taken. `None` == first run: there is
    /// no prior answer, so a failed read reports `unavailable`, not
    /// `stale`.
    pub as_of: Option<Rfc3339>,
    pub confirmed: Option<i128>,
    pub unconfirmed: Option<i128>,
    pub txs: SeenTxs,
}

// The on-disk shape. Kept private: nothing outside this module should be
// able to construct a half-valid state file.

#[derive(Serialize, Deserialize)]
struct SeenFile {
    schema: u32,
    local_id_derivation: String,
    address_set_sha256: String,
    as_of: String,
    #[serde(default)]
    balances: BalancesFile,
    #[serde(default)]
    txs: BTreeMap<String, SeenTxFile>,
}

#[derive(Serialize, Deserialize, Default)]
struct BalancesFile {
    #[serde(default)]
    confirmed: Option<String>,
    #[serde(default)]
    unconfirmed: Option<String>,
}

#[derive(Serialize, Deserialize)]
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
        let first_run = |why: &str| -> Seen {
            eprintln!(
                "sumer-bitcoin-adapter: {}: {why}; treating this as a first run \
                 (zero tombstones)",
                path.display()
            );
            Seen::default()
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            // Absent is the ordinary first run and is not worth a line of
            // stderr on every fresh install.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Seen::default(),
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
        let Ok(as_of) = Rfc3339::new(file.as_of.clone()) else {
            return first_run(&format!("as_of {:?} is not RFC 3339", file.as_of));
        };

        let mut txs = SeenTxs::new();
        for (txid, entry) in file.txs {
            let Ok(delta) = entry.delta.parse::<i128>() else {
                return first_run(&format!(
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
            return first_run("a recorded balance is not an integer");
        };

        Seen {
            as_of: Some(as_of),
            confirmed,
            unconfirmed,
            txs,
        }
    }

    /// Writes one resource's state, atomically. A no-op without a
    /// `--state-dir`.
    pub fn save(
        &self,
        resource_id: &str,
        address_hash: &str,
        as_of: &Rfc3339,
        seen: &Seen,
    ) -> io::Result<()> {
        let (Some(dir), Some(path)) = (self.dir.as_ref(), self.path(resource_id)) else {
            return Ok(());
        };
        let file = SeenFile {
            schema: SCHEMA,
            local_id_derivation: crate::map::LOCAL_ID_DERIVATION.to_owned(),
            address_set_sha256: address_hash.to_owned(),
            as_of: as_of.as_str().to_owned(),
            balances: BalancesFile {
                confirmed: seen.confirmed.map(|v| v.to_string()),
                unconfirmed: seen.unconfirmed.map(|v| v.to_string()),
            },
            txs: seen
                .txs
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
                .collect(),
        };
        std::fs::create_dir_all(dir)?;
        write_atomic(&path, &serde_json::to_vec_pretty(&file)?)
    }
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
        let next = Seen {
            as_of: Some(as_of.clone()),
            confirmed: Some(0),
            unconfirmed: Some(5_000),
            txs: map::next_seen(
                &Chain {
                    txs: &txs,
                    mempool: &mempool,
                    gone: &none,
                },
                &owned(),
                &seen0.txs,
            ),
        };
        store.save("w", hash, &as_of, &next).unwrap();

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
        let next = Seen {
            as_of: Some(as_of.clone()),
            confirmed: Some(5_000),
            unconfirmed: Some(0),
            txs: map::next_seen(
                &Chain {
                    txs: &txs,
                    mempool: &none,
                    gone: &none,
                },
                &owned(),
                &seen1.txs,
            ),
        };
        store.save("w", hash, &as_of, &next).unwrap();

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
        let next = Seen {
            txs: map::next_seen(
                &Chain {
                    txs: &BTreeMap::new(),
                    mempool: &none,
                    gone: &gone,
                },
                &owned(),
                &seen2.txs,
            ),
            ..seen2
        };
        assert!(
            next.txs.is_empty(),
            "a tombstoned transaction stops being tracked"
        );
        store.save("w", hash, &as_of, &next).unwrap();

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
                "address_set_sha256": "h", "as_of": "2026-01-01T00:00:00Z",
                "txs": {"x": {"delta": "1"}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 1, "local_id_derivation": "something-else",
                "address_set_sha256": "h", "as_of": "2026-01-01T00:00:00Z",
                "txs": {"x": {"delta": "1"}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 1, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "A DIFFERENT WALLET", "as_of": "2026-01-01T00:00:00Z",
                "txs": {"x": {"delta": "1"}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 1, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "h", "as_of": "2026-01-01T00:00:00Z",
                "txs": {"x": {"delta": "not a number"}}
            })
            .to_string(),
        ] {
            std::fs::write(&path, &bad).unwrap();
            let seen = store.load("w", "h");
            assert!(seen.as_of.is_none(), "first run: {bad}");
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
        let seen = Seen {
            as_of: Some(as_of.clone()),
            confirmed: Some(2_100_000_000_000_000),
            unconfirmed: Some(-1_234),
            txs: [(
                a.clone(),
                SeenTx {
                    height: Some(1),
                    delta: -9,
                },
            )]
            .into_iter()
            .collect(),
        };
        store.save("w", "h", &as_of, &seen).unwrap();
        let back = store.load("w", "h");
        assert_eq!(
            back.as_of.map(|t| t.as_str().to_owned()),
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

    #[test]
    fn no_state_dir_means_no_state() {
        let store = Store::new(None);
        let as_of = Rfc3339::new("2026-09-07T00:00:00Z").unwrap();
        store.save("w", "h", &as_of, &Seen::default()).unwrap();
        assert!(store.load("w", "h").as_of.is_none());
    }
}
