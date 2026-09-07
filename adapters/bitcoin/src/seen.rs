//! `seen.json`: a per-resource **balance cache**, and nothing else.
//!
//! One file per resource under `--state-dir`. It records the figures the
//! last successful `balances.read` observed and when, so that a later
//! failed read can answer `stale { as_of }` instead of `unavailable`.
//!
//! **Nothing here is unrecoverable.** Losing this file, failing to write
//! it, or writing it for a reply the host never received all cost the same
//! thing: one `unavailable` where a `stale` was possible, until the next
//! successful balance read re-establishes it. That is the whole reason
//! there is no delivery gate around the write and no lock around the
//! read-modify-write -- see ADR 0004 decision 7. The file used to also
//! carry a transaction baseline, which existed so the adapter could
//! announce a disappearance; that write WAS unrecoverable, and it is gone
//! along with the retraction it served.
//!
//! Two rules this module still enforces:
//!
//! 1. **A missing, unreadable, unparseable, version-mismatched or
//!    hash-mismatched file is a FIRST RUN.** Never a partial parse, never
//!    a best-effort salvage. A half-read cache would report a figure this
//!    adapter cannot stand behind, under a date it did not observe.
//! 2. **Writes are atomic** (temp file + rename). A torn file would be
//!    unparseable, which rule 1 turns into a first run rather than a
//!    corrupt cache.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use sumer_wire::Rfc3339;

/// Bumped whenever the on-disk shape changes. An older or newer number is
/// a first run, not a migration.
///
/// 3: the `history` section is gone. A schema-2 file carries a transaction
/// baseline this adapter no longer reads, and its `balances` section could
/// be salvaged -- but rule 1 is "no partial parse", and a first run here
/// costs one `unavailable`.
const SCHEMA: u32 = 3;

/// What the previous successful balance read left behind.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// `None` == no balance has ever been recorded: a failed read reports
    /// `unavailable`, not `stale`.
    pub balances_as_of: Option<Rfc3339>,
    pub confirmed: Option<i128>,
    pub unconfirmed: Option<i128>,
}

// The on-disk shape. Kept private: nothing outside this module should be
// able to construct a half-valid state file.

#[derive(Serialize, Deserialize)]
struct SeenFile {
    schema: u32,
    address_set_sha256: String,
    #[serde(default)]
    balances: BalancesFile,
}

#[derive(Serialize, Deserialize, Default)]
struct BalancesFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    as_of: Option<String>,
    /// `null` is a recorded UNKNOWN, which is exactly what an absent one
    /// degrades to.
    #[serde(default)]
    confirmed: Option<String>,
    #[serde(default)]
    unconfirmed: Option<String>,
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
                "sumer-bitcoin-adapter: no --state-dir; a failed balance read will \
                 report `unavailable` rather than `stale`"
            );
        }
        Store { dir }
    }

    fn path(&self, resource_id: &str) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|d| d.join(format!("{resource_id}.json")))
    }

    /// Loads one resource's cached balances, degrading to a first run on
    /// ANY doubt. `address_hash` is the SHA-256 of the wallet's address
    /// set: a different one means these figures are a different wallet's.
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

    /// Records the balances a balance read observed.
    ///
    /// A failure here is logged by the caller and survived: the next
    /// failed read reports `unavailable` instead of `stale`, and the next
    /// successful one writes the cache again.
    pub fn save_balances(
        &self,
        resource_id: &str,
        address_hash: &str,
        as_of: &Rfc3339,
        confirmed: Option<i128>,
        unconfirmed: Option<i128>,
    ) -> io::Result<()> {
        let (Some(dir), Some(path)) = (self.dir.as_ref(), self.path(resource_id)) else {
            return Ok(());
        };
        std::fs::create_dir_all(dir)?;
        let file = SeenFile {
            schema: SCHEMA,
            address_set_sha256: address_hash.to_owned(),
            balances: BalancesFile {
                as_of: Some(as_of.as_str().to_owned()),
                confirmed: confirmed.map(|v| v.to_string()),
                unconfirmed: unconfirmed.map(|v| v.to_string()),
            },
        };
        write_atomic(&path, &serde_json::to_vec_pretty(&file)?)
    }
}

/// Reads the file at `path` if it is a state file for THIS wallet, and
/// `None` -- a first run -- on any doubt at all.
fn read_file(path: &Path, address_hash: &str) -> Option<SeenFile> {
    let first_run = |why: &str| -> Option<SeenFile> {
        eprintln!(
            "sumer-bitcoin-adapter: {}: {why}; treating this as a first run \
             (a failed balance read reports `unavailable`)",
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
    if file.address_set_sha256 != address_hash {
        // The address set changed, so these figures are a different
        // wallet's balances and reporting them as this one's `stale`
        // answer would be a false statement about money.
        return first_run("the address set changed");
    }
    Some(file)
}

/// The on-disk shape as this adapter uses it. `None` on any unreadable
/// field: a partially-recovered cache is what rule 1 forbids.
fn decode(path: &Path, file: SeenFile) -> Option<Seen> {
    let first_run = |why: String| -> Option<Seen> {
        eprintln!(
            "sumer-bitcoin-adapter: {}: {why}; treating this as a first run \
             (a failed balance read reports `unavailable`)",
            path.display()
        );
        None
    };
    let balances_as_of = match file.balances.as_of {
        None => None,
        Some(s) => match Rfc3339::new(s.clone()) {
            Ok(t) => Some(t),
            Err(_) => return first_run(format!("as_of {s:?} is not RFC 3339")),
        },
    };
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

    #[test]
    fn corrupt_state_is_a_first_run_never_a_partial_parse() {
        let dir = tmpdir("corrupt");
        let store = Store::new(Some(dir.clone()));
        let path = dir.join("w.json");

        for bad in [
            "{".to_owned(),
            serde_json::json!({
                "schema": 99, "address_set_sha256": "h",
                "balances": {"as_of": "2026-01-01T00:00:00Z", "confirmed": "1"}
            })
            .to_string(),
            // Schema 2: the shape that carried a transaction baseline. Its
            // balances section could be salvaged; rule 1 says it is not.
            serde_json::json!({
                "schema": 2, "local_id_derivation": "btc-txid@1",
                "address_set_sha256": "h",
                "balances": {"as_of": "2026-01-01T00:00:00Z", "confirmed": "1"},
                "history": {"as_of": "2026-01-01T00:00:00Z", "txs": {}}
            })
            .to_string(),
            serde_json::json!({
                "schema": 3, "address_set_sha256": "A DIFFERENT WALLET",
                "balances": {"as_of": "2026-01-01T00:00:00Z", "confirmed": "1"}
            })
            .to_string(),
            serde_json::json!({
                "schema": 3, "address_set_sha256": "h",
                "balances": {"as_of": "the day before yesterday", "confirmed": "1"}
            })
            .to_string(),
            serde_json::json!({
                "schema": 3, "address_set_sha256": "h",
                "balances": {"as_of": "2026-01-01T00:00:00Z", "confirmed": "not a number"}
            })
            .to_string(),
        ] {
            std::fs::write(&path, &bad).unwrap();
            let seen = store.load("w", "h");
            assert!(seen.balances_as_of.is_none(), "first run: {bad}");
            assert!(seen.confirmed.is_none(), "no partial parse: {bad}");
            assert!(seen.unconfirmed.is_none(), "no partial parse: {bad}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_round_trips() {
        let dir = tmpdir("roundtrip");
        let store = Store::new(Some(dir.clone()));
        let as_of = Rfc3339::new("2026-09-07T11:22:33Z").unwrap();
        store
            .save_balances("w", "h", &as_of, Some(2_100_000_000_000_000), Some(-1_234))
            .unwrap();
        let back = store.load("w", "h");
        assert_eq!(
            back.balances_as_of.map(|t| t.as_str().to_owned()),
            Some("2026-09-07T11:22:33Z".to_owned())
        );
        assert_eq!(back.confirmed, Some(2_100_000_000_000_000));
        assert_eq!(
            back.unconfirmed,
            Some(-1_234),
            "a negative unconfirmed balance survives"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A cache write that cannot happen is survived, and leaves no
    /// half-written file behind. It costs one `unavailable`.
    #[test]
    fn a_write_that_cannot_happen_leaves_no_file() {
        let dir = tmpdir("writefail");
        let store = Store::new(Some(dir.clone()));
        // A directory where the state file belongs: `rename` onto it
        // fails, whatever this process is running as.
        std::fs::create_dir_all(dir.join("w.json")).unwrap();
        let as_of = Rfc3339::new("2026-09-07T00:00:00Z").unwrap();
        store
            .save_balances("w", "h", &as_of, Some(1), Some(2))
            .unwrap_err();
        assert!(store.load("w", "h").balances_as_of.is_none());
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("w.tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the temp file is cleaned up rather than left behind: {leftovers:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_state_dir_means_no_state() {
        let store = Store::new(None);
        let as_of = Rfc3339::new("2026-09-07T00:00:00Z").unwrap();
        store
            .save_balances("w", "h", &as_of, Some(1), Some(2))
            .unwrap();
        assert!(store.load("w", "h").balances_as_of.is_none());
    }
}
