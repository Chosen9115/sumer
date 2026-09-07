//! The two restart cases: a real `sumer refresh` process, really
//! SIGKILLed, at the two moments that matter.
//!
//! `tests/kill_host.py` sits between the host and the fake adapter and
//! sends the signal, so the host dies the way a host really dies -- no
//! shutdown hook, no destructor, no flush. What is asserted afterwards is
//! only what is on disk.
//!
//! The invariant both cases circle is ONE TRANSACTION on the final page.
//! Last-page inserts, `last_seen_crawl` stamps, the cursor write, the
//! retraction derivation, the adapter/resource metadata updates and the
//! crawl's open -> drained transition all commit together. Split any of
//! them and a crash can leave a crawl recorded DRAINED whose retractions
//! were never derived -- and every later resume sees a finished sweep and
//! never derives them. There is no repair path for that; the only defence
//! is that it cannot be represented.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;
use std::process::Command;

use serde_json::json;
use support::{drained_status, fixture, obs, page, repo_root, store, Run, Scratch};

fn binary(name: &str) -> PathBuf {
    let mut dir = std::env::current_exe().expect("a test binary has a path");
    dir.pop();
    dir.pop();
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn build_sumer() -> PathBuf {
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", "sumer-store"])
        .current_dir(repo_root())
        .status()
        .expect("cargo runs");
    assert!(status.success(), "cargo build -p sumer-store failed");
    binary("sumer")
}

/// The argv the profile stores for its adapter: the killer wrapper, the
/// fixture, and the run. A `count` no conversation ever reaches means
/// "proxy everything, kill nothing".
fn killer_argv(mode: &str, count: u32, fixture: &std::path::Path, run: u32) -> Vec<String> {
    vec![
        "python3".to_owned(),
        repo_root()
            .join("core/store/tests/kill_host.py")
            .to_string_lossy()
            .into_owned(),
        mode.to_owned(),
        count.to_string(),
        fixture.to_string_lossy().into_owned(),
        run.to_string(),
    ]
}

const NEVER: u32 = 9999;

struct Harness {
    sumer: PathBuf,
    profile: PathBuf,
    scratch: Scratch,
}

impl Harness {
    fn new(tag: &str) -> Harness {
        let scratch = Scratch::new(tag);
        let sumer = build_sumer();
        let profile = scratch.path().join("profile");
        Harness {
            sumer,
            profile,
            scratch,
        }
    }

    fn point_at(&self, argv: &[String]) {
        let store = store(&self.scratch);
        sumer_store::store::upsert_adapter(store.conn(), support::ADAPTER_ID, argv).unwrap();
    }

    fn refresh(&self) -> std::process::Output {
        Command::new(&self.sumer)
            .arg("--profile")
            .arg(&self.profile)
            .arg("refresh")
            .output()
            .expect("sumer runs")
    }

    fn store(&self) -> sumer_store::Store {
        sumer_store::Store::open(&sumer_store::Profile::new(&self.profile)).unwrap()
    }
}

/// SIGKILL between pages 1 and 2.
///
/// The kill point is deterministic rather than timed: the wrapper refuses
/// to forward the SIXTH request (hello, resources.list, status.read,
/// balances.read, history page 1, history page 2) and kills the host
/// instead. The host only issues request six once it has committed
/// everything page 1 earned, so "page 1 is durable" is an interlock, not a
/// hope.
///
/// After it: page 1 committed, the cursor is at its `next`, the crawl is
/// abandoned open, and NOTHING is retracted.
#[test]
fn a_kill_between_pages_keeps_page_one_and_its_cursor() {
    if !support::python3_available() {
        eprintln!("python3 not found on PATH -- skipping");
        return;
    }
    let harness = Harness::new("kill-between");
    let two_pages = || {
        Run::new(vec![
            page(
                None,
                vec![obs("keep", "10.00"), obs("drop", "20.00")],
                drained_status(Some(json!({"kind": "cursor", "cursor": "p2"}))),
            ),
            page(
                Some(json!({"kind": "cursor", "cursor": "p2"})),
                vec![obs("second-page", "30.00")],
                drained_status(None),
            ),
        ])
    };
    let fixture = fixture(&harness.scratch, vec![two_pages(), two_pages()]);

    // Run 0, unharmed: three live records, a complete sweep.
    harness.point_at(&killer_argv("before_request", NEVER, &fixture, 0));
    let clean = harness.refresh();
    assert!(clean.status.success(), "the priming refresh must succeed");
    assert_eq!(support::live_ids(&harness.store()).len(), 3);

    // Run 1, killed before page 2 is asked for.
    harness.point_at(&killer_argv("before_request", 6, &fixture, 1));
    let killed = harness.refresh();
    assert!(
        !killed.status.success(),
        "the host was SIGKILLed; it cannot have exited cleanly"
    );

    let store = harness.store();
    let crawls = sumer_store::store::crawls(store.conn(), 10).unwrap();
    let abandoned = crawls.first().expect("the killed crawl is on disk");
    assert!(!abandoned.drained, "an abandoned crawl is not drained");
    assert!(!abandoned.complete, "and it is certainly not complete");
    assert!(
        abandoned.start_page.is_none(),
        "it began at the start of history, like every refresh"
    );
    assert_eq!(
        abandoned
            .next_page
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok()),
        Some(json!({"kind": "cursor", "cursor": "p2"})),
        "page 1 committed WITH its cursor, so --resume knows exactly where to pick up"
    );
    assert_eq!(
        support::retractions(&store),
        Vec::<(String, String)>::new(),
        "a crawl that never drained retracts nothing"
    );
    assert_eq!(
        support::live_ids(&store).len(),
        3,
        "and it takes nothing away either"
    );
}

/// SIGKILL after the last page's reply is received and BEFORE the commit.
///
/// Nothing from that page persisted, the crawl is abandoned, nothing is
/// retracted, and the next refresh starts fresh and finishes the job.
///
/// **What this does and does not pin down.** There is no wire event
/// between "the host received the final reply" and "the host committed",
/// so the wrapper cannot interlock this the way the between-pages case is
/// interlocked -- it kills the instant the reply is flushed and the kill
/// lands somewhere in that window. What is asserted is therefore the
/// POSTCONDITION over the whole window: whichever moment the signal
/// arrived, the disk never holds part of the final page. Nothing partial,
/// no retraction without its inserts, no crawl marked drained without the
/// derivation that a drained crawl is supposed to have run.
///
/// The window is made wide on purpose so the interesting end of it is
/// really exercised: the final page carries 1,500 observations, so the
/// host has to SHA-256 and insert all of them inside one `synchronous =
/// FULL` transaction. The alternative was a hook in production code that
/// exists only for this test, which is a worse thing to ship than a
/// documented window.
#[test]
fn a_kill_before_the_final_commit_persists_nothing_from_that_page() {
    if !support::python3_available() {
        eprintln!("python3 not found on PATH -- skipping");
        return;
    }
    let harness = Harness::new("kill-precommit");

    let bulk: Vec<serde_json::Value> = (0..1500)
        .map(|n| obs(&format!("bulk-{n:04}"), "1.00"))
        .collect();
    let fixture = fixture(
        &harness.scratch,
        vec![
            // Run 0: two ordinary live records.
            Run::new(vec![page(
                None,
                vec![obs("keep", "10.00"), obs("drop", "20.00")],
                drained_status(None),
            )]),
            // Run 1: one page, 1500 records, and `drop` omitted -- so a
            // committed transaction would BOTH insert 1500 rows and retract
            // `drop`. Neither may survive.
            Run::new(vec![page(None, bulk, drained_status(None))]),
            // Run 2: the same as run 0. The next refresh starts fresh.
            Run::new(vec![page(
                None,
                vec![obs("keep", "10.00"), obs("drop", "20.00")],
                drained_status(None),
            )]),
        ],
    );

    harness.point_at(&killer_argv("after_reply", NEVER, &fixture, 0));
    assert!(harness.refresh().status.success());
    assert_eq!(support::live_ids(&harness.store()).len(), 2);

    // Replies: hello, resources.list, status.read, balances.read, history.
    harness.point_at(&killer_argv("after_reply", 5, &fixture, 1));
    let killed = harness.refresh();
    assert!(
        !killed.status.success(),
        "the host was SIGKILLed mid-transaction"
    );

    let store = harness.store();
    let bulk_rows: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM observation WHERE local_id LIKE 'bulk-%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        bulk_rows, 0,
        "NOTHING from the final page persisted -- the whole page is one transaction"
    );
    assert_eq!(
        support::retractions(&store),
        Vec::<(String, String)>::new(),
        "and the retraction that transaction would have derived is not there either"
    );
    let crawls = sumer_store::store::crawls(store.conn(), 10).unwrap();
    let abandoned = crawls.first().expect("the killed crawl is on disk");
    assert!(
        !abandoned.drained && !abandoned.complete,
        "a crawl recorded DRAINED whose retractions were never derived is the one \
         state this design must not be able to reach"
    );
    let mut live = support::live_ids(&store);
    live.sort();
    assert_eq!(live, vec!["drop".to_owned(), "keep".to_owned()]);

    // The next refresh starts fresh -- `page: None`, no cursor inherited --
    // and finishes normally.
    harness.point_at(&killer_argv("after_reply", NEVER, &fixture, 2));
    let recovered = harness.refresh();
    assert!(recovered.status.success());
    let store = harness.store();
    let latest = sumer_store::store::crawls(store.conn(), 1).unwrap();
    let latest = latest.first().expect("a new crawl");
    assert!(latest.start_page.is_none(), "the next refresh starts fresh");
    assert!(latest.complete, "and completes");
    assert_eq!(support::retractions(&store), Vec::<(String, String)>::new());
}
