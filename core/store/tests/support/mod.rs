//! Shared scaffolding for the store's integration tests.
//!
//! The scripted adversary is `adapters/fake/fake_adapter.py` -- the same
//! interpreter the conformance suite drives, with the scenario written
//! here in Rust beside the assertions that read it rather than in a
//! checked-in fixture file. A fixture that lives in another directory from
//! its expectations is a fixture that drifts.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use sumer_host::AdapterHandle;
use sumer_store::store::Store;
use sumer_store::sweep::{SweepOptions, SweepReport};
use sumer_store::Profile;

pub const ADAPTER_ID: &str = "fake-adapter";
pub const RESOURCE_ID: &str = "acct";
pub const DERIVATION: &str = "fixture-literal@1";

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory that removes itself. `std::env::temp_dir` plus a
/// unique name: no dependency, and a leaked directory on a hard kill is a
/// test-only cost.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "sumer-store-test-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        Scratch { dir }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn profile(&self) -> Profile {
        Profile::new(self.dir.join("profile"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

/// `<repo>/`: `CARGO_MANIFEST_DIR` is `<repo>/core/store`.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("core/store has two ancestors")
        .to_path_buf()
}

/// The default provenance every observation in a run inherits. The
/// `adapter_id` here MUST be the connection's: the fold keys chains by it.
fn provenance_defaults() -> Value {
    json!({
        "adapter_id": ADAPTER_ID,
        "provider_id": "p1",
        "surface": "s",
        "observed_at": "2026-01-01T00:00:00Z",
        "completeness": "complete"
    })
}

/// One history observation, spelled the way `ObservationWire` needs it.
pub fn obs(local_id: &str, amount: &str) -> Value {
    json!({
        "resource_id": RESOURCE_ID,
        "local_id": local_id,
        "state": "active",
        "surface": "s",
        "posting": "posted",
        "amount": {"asset": "usd", "amount": amount},
        "raw_sign": "provider_positive",
        "description": local_id
    })
}

/// The same, with an explicit `observed_at` -- what the `history_start`
/// exemption compares against.
pub fn obs_at(local_id: &str, amount: &str, observed_at: &str) -> Value {
    let mut observation = obs(local_id, amount);
    observation["provenance"] = json!({"observed_at": observed_at});
    observation
}

/// One balance line.
pub fn balance(category: &str, amount: Option<&str>) -> Value {
    json!({
        "resource_id": RESOURCE_ID,
        "category": category,
        "amount": amount.map(|a| json!({"asset": "usd", "amount": a})),
        "provenance": provenance_defaults()
    })
}

/// One `history.read` rule: reply `ok` with these observations and this
/// status entry. `when` is `None` for "match the next request whatever it
/// asks", or `Some(page)` to require that the request carried exactly that
/// page -- which is how the resume test proves the stored cursor really
/// went out on the wire and not merely into a log line.
pub fn page(when: Option<Value>, observations: Vec<Value>, status: Value) -> Value {
    let mut rule = json!({
        "do": [{
            "op": "reply_ok",
            "body": {"observations": observations, "statuses": [status]}
        }]
    });
    if let Some(page) = when {
        rule["when"] = json!({"resources": [{"resource_id": RESOURCE_ID, "page": page}]});
    } else {
        rule["when"] = json!({});
    }
    rule
}

/// A status entry for a page: fetched, exact, and `next` -- `None` for the
/// last page of a drained sweep.
pub fn drained_status(next: Option<Value>) -> Value {
    json!({
        "resource_id": RESOURCE_ID,
        "page": {"cursor_resumable": "exact", "next": next}
    })
}

/// One element of `script.runs`.
pub struct Run {
    pub derivation: String,
    /// The `adapter_id` this run's HELLO announces. Defaults to
    /// [`ADAPTER_ID`], the id the store has on file; a run that announces
    /// anything else is a connection that is not the adapter we recorded.
    pub hello_adapter_id: String,
    /// Answer `status.read` with an `err` envelope instead of a reply.
    pub status_err: bool,
    pub history: Vec<Value>,
    pub statuses: Value,
    pub balances: Vec<Value>,
    pub resource_extra: Value,
    pub provider_id: String,
    pub balance_statuses: Option<Value>,
}

impl Run {
    pub fn new(history: Vec<Value>) -> Run {
        Run {
            derivation: DERIVATION.to_owned(),
            hello_adapter_id: ADAPTER_ID.to_owned(),
            status_err: false,
            history,
            statuses: json!([{"resource_id": RESOURCE_ID}]),
            balances: Vec::new(),
            resource_extra: json!({}),
            provider_id: "p1".to_owned(),
            balance_statuses: None,
        }
    }

    /// The `statuses` of the `balances.read` reply -- what the host derives
    /// each balance line's STALENESS from.
    pub fn balance_statuses(mut self, statuses: Value) -> Run {
        self.balance_statuses = Some(statuses);
        self
    }

    /// The VANTAGE this run reads from -- `ResourceDescriptor.provider_id`.
    pub fn provider_id(mut self, provider_id: &str) -> Run {
        self.provider_id = provider_id.to_owned();
        self
    }

    /// What this run's HELLO calls itself.
    pub fn announces(mut self, adapter_id: &str) -> Run {
        self.hello_adapter_id = adapter_id.to_owned();
        self
    }

    /// `status.read` fails outright -- the read that happens before
    /// `balances.read` and can therefore return before any balance is
    /// recorded.
    pub fn status_read_err(mut self) -> Run {
        self.status_err = true;
        self
    }

    pub fn derivation(mut self, derivation: &str) -> Run {
        self.derivation = derivation.to_owned();
        self
    }

    pub fn statuses(mut self, statuses: Value) -> Run {
        self.statuses = statuses;
        self
    }

    pub fn balances(mut self, balances: Vec<Value>) -> Run {
        self.balances = balances;
        self
    }

    /// Moves the resource DEFINITION -- what `resource_fingerprint`
    /// hashes. A Bitcoin wallet's address set lives here.
    pub fn resource_extra(mut self, extra: Value) -> Run {
        self.resource_extra = extra;
        self
    }

    fn build(self) -> Value {
        json!({
            "label": "store test",
            "hello": {
                "protocol": "1",
                "adapter_id": self.hello_adapter_id,
                "adapter_version": "0.1.0",
                "capabilities": ["resources.list", "balances.read", "history.read", "status.read"],
                "local_id_derivation": self.derivation,
                "max_in_flight": 1
            },
            "provenance": provenance_defaults(),
            "on": {
                "resources.list": [{"do": [{"op": "reply_ok", "body": {"resources": [{
                    "resource_id": RESOURCE_ID,
                    "provider_id": self.provider_id,
                    "kind": "bank_checking",
                    "label": "Checking",
                    "provider_extra": self.resource_extra
                }]}}]}],
                "status.read": [{"do": [if self.status_err {
                    json!({"op": "reply_err", "code": "unavailable", "message": "status.read is down"})
                } else {
                    json!({"op": "reply_ok", "body": {"statuses": self.statuses}})
                }]}],
                "balances.read": [{"do": [{"op": "reply_ok", "body": {
                    "observations": self.balances,
                    "statuses": self.balance_statuses
                        .unwrap_or_else(|| json!([{"resource_id": RESOURCE_ID}]))
                }}]}],
                "history.read": self.history
            }
        })
    }
}

/// Writes a fixture holding these runs and returns its path.
pub fn fixture(scratch: &Scratch, runs: Vec<Run>) -> PathBuf {
    let document = json!({
        "case": "store-test",
        "script": {"runs": runs.into_iter().map(Run::build).collect::<Vec<_>>()}
    });
    let path = scratch.path().join("fixture.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    path
}

/// Opens (creating on first call) the profile store for this scratch dir,
/// with the fake adapter registered.
pub fn store(scratch: &Scratch) -> Store {
    let profile = scratch.profile();
    let store = Store::init(&profile).unwrap();
    sumer_store::store::upsert_adapter(store.conn(), ADAPTER_ID, &["python3".to_owned()]).unwrap();
    store
}

/// Spawns the fake adapter on one run of a fixture, **recording the
/// connection** so a test can assert on the wire itself.
pub async fn connect(fixture: &Path, run: usize) -> AdapterHandle {
    AdapterHandle::spawn_recorded(
        vec![
            "python3".to_owned(),
            repo_root()
                .join("adapters/fake/fake_adapter.py")
                .to_string_lossy()
                .into_owned(),
        ],
        [
            (
                "SUMER_FIXTURE".to_owned(),
                fixture.to_string_lossy().into_owned(),
            ),
            ("SUMER_FIXTURE_RUN".to_owned(), run.to_string()),
        ],
    )
    .await
    .expect("the fake adapter starts and says hello")
}

/// One full refresh over one run of a fixture, returning both the sweep
/// reports and the raw transcript of the connection that produced them.
pub async fn refresh_run(
    store: &mut Store,
    fixture: &Path,
    run: usize,
    options: SweepOptions,
) -> (Vec<SweepReport>, Vec<sumer_host::Exchange>) {
    let handle = connect(fixture, run).await;
    let reports = sumer_store::refresh::refresh_adapter(store, &handle, ADAPTER_ID, options)
        .await
        .expect("the refresh completes")
        .sweeps;
    let transcript = handle.transcript();
    let _ = handle.close().await;
    (reports, transcript)
}

/// One full refresh over one run of a fixture, returning whatever it
/// returned -- for the refreshes that are supposed to FAIL.
pub async fn refresh_run_result(
    store: &mut Store,
    fixture: &Path,
    run: usize,
) -> sumer_store::error::Result<()> {
    let handle = connect(fixture, run).await;
    let result =
        sumer_store::refresh::refresh_adapter(store, &handle, ADAPTER_ID, SweepOptions::default())
            .await;
    let _ = handle.close().await;
    result.map(|_| ())
}

/// The one sweep report a single-resource fixture produces.
pub fn only(reports: Vec<SweepReport>) -> SweepReport {
    assert_eq!(reports.len(), 1, "one resource, one sweep report");
    reports.into_iter().next().expect("one report")
}

/// Every retraction in the profile, as `(local_id, reason)`.
pub fn retractions(store: &Store) -> Vec<(String, String)> {
    let mut stmt = store
        .conn()
        .prepare("SELECT local_id, reason FROM retraction ORDER BY retraction_id")
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// The live `local_id`s of the test resource, as `history` lists them.
pub fn live_ids(store: &Store) -> Vec<String> {
    sumer_store::sweep::live_records(store, ADAPTER_ID, RESOURCE_ID)
        .unwrap()
        .into_iter()
        .map(|(local_id, _, _)| local_id)
        .collect()
}

/// The discrepancy kinds recorded so far, oldest first.
pub fn discrepancy_kinds(store: &Store) -> Vec<String> {
    let mut kinds: Vec<String> = sumer_store::store::discrepancies(store.conn(), 100)
        .unwrap()
        .into_iter()
        .map(|row| row.kind)
        .collect();
    kinds.reverse();
    kinds
}

/// How many rows one chain holds -- the append-only proof that a sweep's
/// observations persisted even when it retracted nothing.
pub fn chain_len(store: &Store, local_id: &str) -> usize {
    sumer_store::store::chain(store.conn(), ADAPTER_ID, local_id)
        .unwrap()
        .len()
}
