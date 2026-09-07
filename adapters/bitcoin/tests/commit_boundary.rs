//! The commit boundary, exhaustively, over REQUEST SEQUENCES.
//!
//! **This oracle reads bytes on stdout and bytes on disk.** It never names
//! `Commit`, `Outcome` or `apply`, and it must not learn to: three
//! successive fixes to this corner were each locally correct against a test
//! that knew the mechanism, and each one exposed the next. Rules stated
//! over the wire and the state file survive a restructuring that was never
//! anticipated; rules stated over the machinery are re-written alongside it
//! and prove nothing.
//!
//! A case is a SEQUENCE of requests, not a request. The defects this exists
//! to catch all live between two of them: a crawl that spans pages, a
//! retry at the last delivered cursor, a reply refused after another
//! resource in the same batch already earned its baseline.
//!
//! Six rules, and rule 4 is the one that matters most:
//!
//! 1. every frame fits `MAX_FRAME_BYTES`;
//! 2. a request whose reply did not go out leaves the state byte-identical;
//! 3. a txid removed from a baseline was retracted in a delivered frame;
//! 4. a baseline txid the corpus answers 404 for MUST be retracted in a
//!    delivered frame, and MUST then be gone from disk;
//! 5. every requested id appears in `statuses` exactly once;
//! 6. an uninjected sequence that drains a crawl commits a HISTORY write.
//!
//! Rules 1, 2, 3 and 5 are safety: they say what must not happen, and an
//! adapter that answered nothing at all would satisfy every one of them.
//! Rules 4 and 6 are liveness, and they are why this file is not the
//! previous three rounds again. Delete the retraction path from
//! `wallet::sync` and rule 4 -- and only rule 4 -- goes red.

use crate::map::{self, PAGE_BUDGET_BYTES};
use crate::tests::{adapter_with, paged_corpus, tmp_root, wallets_json, write_at};
use crate::{write_reply, Adapter};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use sumer_wire::{Rfc3339, MAX_FRAME_BYTES, OP_BALANCES_READ, OP_HISTORY_READ};

// ---------------------------------------------------------------------
// The corpus: built once, shared, never written to by a case
// ---------------------------------------------------------------------

const A_DRAINS: &str = "bc1qdrains0000";
const A_SPANS: &str = "bc1qspans00000";
const A_RETRACT: &str = "bc1qretract000";
const A_LIMITED: &str = "bc1qlimited000";
const A_BIG503: &str = "bc1qbig5030000";
const A_OVERFLOW: &str = "bc1qoverflow00";

/// Enough transactions that the crawl cannot be served in one page.
const SPAN_COUNT: usize = 1_200;
/// Each answers 503 with a 4 KiB body -- `provider_detail` evidence, the
/// one part of a status entry no reservation can size in advance. Enough
/// of them together overflow a frame, which is how the oversize refusal in
/// this matrix is a REAL refusal rather than a simulated one.
const OVERFLOW_WALLETS: usize = 300;

/// `run0`'s pinned clock. A history write that reached disk during a
/// sequence carries this; the seeding run carries `SEED_STAMP`.
const RUN0_STAMP: &str = "2026-01-01T00:00:00Z";
const SEED_STAMP: &str = "2025-12-31T00:00:00Z";

/// The txid `retracts` remembers and the corpus answers 404 for. Rule 4 is
/// derived from this fixture fact, not from anything the adapter does.
fn gone_txid() -> String {
    "f".repeat(64)
}

fn live_txid() -> String {
    format!("{:064x}", 0xe0_u32)
}

fn confirmed_tx(txid: &str, height: u64, addr: &str) -> serde_json::Value {
    json!({
        "txid": txid,
        "fee": 100,
        "status": {
            "confirmed": true, "block_height": height,
            "block_hash": format!("{height:064x}"), "block_time": 1_600_000_000i64
        },
        "vin": [{"prevout": {"scriptpubkey_address": "bc1qthem00000", "value": 1_000}}],
        "vout": [{"scriptpubkey_address": addr, "value": 900}],
    })
}

fn stats(confirmed: u64) -> String {
    json!({
        "chain_stats": {"funded_txo_sum": confirmed, "spent_txo_sum": 0, "tx_count": 1},
        "mempool_stats": {"funded_txo_sum": 0, "spent_txo_sum": 0, "tx_count": 0},
    })
    .to_string()
}

struct Fixture {
    corpus: PathBuf,
    config: PathBuf,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(build_fixture)
}

fn build_fixture() -> Fixture {
    let root = tmp_root("commit-boundary-corpus");
    let run0 = root.join("corpus/run0");
    std::fs::create_dir_all(&run0).unwrap();
    write_at(&root, "corpus/run0/now", "1767225600");

    // Drains in one page.
    write_at(
        &root,
        &format!("corpus/run0/address_{A_DRAINS}.json"),
        &stats(2_700),
    );
    let drains: Vec<serde_json::Value> = (0..3)
        .map(|n| confirmed_tx(&format!("{:064x}", 0xd0 + n), 800_000 + n, A_DRAINS))
        .collect();
    write_at(
        &root,
        &format!("corpus/run0/address_{A_DRAINS}_txs_chain.json"),
        &serde_json::Value::Array(drains).to_string(),
    );
    write_at(
        &root,
        &format!("corpus/run0/address_{A_DRAINS}_txs_mempool.json"),
        "[]",
    );

    // Spans pages. `paged_corpus` chains Esplora's 25-per-page listing.
    paged_corpus(&run0, A_SPANS, SPAN_COUNT);
    write_at(
        &root,
        &format!("corpus/run0/address_{A_SPANS}.json"),
        &stats(1_000),
    );

    // Retracts: one live transaction, and one the probe answers 404 for.
    write_at(
        &root,
        &format!("corpus/run0/address_{A_RETRACT}.json"),
        &stats(900),
    );
    write_at(
        &root,
        &format!("corpus/run0/address_{A_RETRACT}_txs_chain.json"),
        &json!([confirmed_tx(&live_txid(), 800_000, A_RETRACT)]).to_string(),
    );
    write_at(
        &root,
        &format!("corpus/run0/address_{A_RETRACT}_txs_mempool.json"),
        "[]",
    );
    write_at(
        &root,
        &format!("corpus/run0/tx_{}.status", gone_txid()),
        "404\nTransaction not found",
    );

    // Run 1 is the SEEDING run: the same wallet with that transaction
    // still in the listing, so the baseline a sequence dismantles is one
    // this adapter itself wrote.
    write_at(&root, "corpus/run1/now", "1767139200");
    write_at(
        &root,
        &format!("corpus/run1/address_{A_RETRACT}.json"),
        &stats(1_800),
    );
    write_at(
        &root,
        &format!("corpus/run1/address_{A_RETRACT}_txs_chain.json"),
        &json!([
            confirmed_tx(&live_txid(), 800_000, A_RETRACT),
            confirmed_tx(&gone_txid(), 799_999, A_RETRACT),
        ])
        .to_string(),
    );
    write_at(
        &root,
        &format!("corpus/run1/address_{A_RETRACT}_txs_mempool.json"),
        "[]",
    );

    // 429: everything after it in the batch is `not_fetched`.
    for name in ["", "_txs_chain"] {
        write_at(
            &root,
            &format!("corpus/run0/address_{A_LIMITED}{name}.status"),
            "429\nslow down",
        );
    }
    // 503 with a 4 KiB body, twice over: one wallet on its own, and the
    // 300 that together will not fit a frame.
    let big = format!("503\n{}", "x".repeat(4_096));
    for addr in [A_BIG503, A_OVERFLOW] {
        for name in ["", "_txs_chain"] {
            write_at(
                &root,
                &format!("corpus/run0/address_{addr}{name}.status"),
                &big,
            );
        }
    }

    let mut wallets = vec![
        json!({"resource_id": "drains", "addresses": [A_DRAINS]}),
        json!({"resource_id": "spans", "addresses": [A_SPANS]}),
        json!({"resource_id": "retracts", "addresses": [A_RETRACT]}),
        json!({"resource_id": "limited", "addresses": [A_LIMITED]}),
        json!({"resource_id": "big503", "addresses": [A_BIG503]}),
        // The malformed-cursor and window kinds get resources of their
        // own: sharing one with another kind would make every pair
        // containing both a duplicate-id request, and test `no_repeats`
        // instead of what those kinds are for.
        json!({"resource_id": "cursed", "addresses": [A_DRAINS]}),
        json!({"resource_id": "windowed", "addresses": [A_DRAINS]}),
    ];
    wallets.extend(
        (0..OVERFLOW_WALLETS)
            .map(|n| json!({"resource_id": format!("o{n:04}"), "addresses": [A_OVERFLOW]})),
    );

    Fixture {
        config: wallets_json(&root, serde_json::Value::Array(wallets)),
        corpus: root.join("corpus"),
    }
}

// ---------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A crawl that drains in one page.
    Drains,
    /// A crawl that does not.
    Spans,
    /// A wallet whose baseline holds a PROBED-404 txid.
    Retracts,
    /// A `resource_id` this adapter has no wallet for.
    Unknown,
    /// A cursor this adapter did not mint: an ENVELOPE error, which is the
    /// exact shape that used to settle another resource's commit.
    Cursed,
    /// A `window` page request: the other envelope error.
    Windowed,
    /// 429. Everything after it in the batch is `not_fetched`.
    Limited,
    /// 503 with a 4 KiB body.
    Big503,
}

const KINDS: [Kind; 8] = [
    Kind::Drains,
    Kind::Spans,
    Kind::Retracts,
    Kind::Unknown,
    Kind::Cursed,
    Kind::Windowed,
    Kind::Limited,
    Kind::Big503,
];

impl Kind {
    fn resource_id(self) -> &'static str {
        match self {
            Kind::Drains => "drains",
            Kind::Spans => "spans",
            Kind::Retracts => "retracts",
            Kind::Unknown => "nobody",
            Kind::Cursed => "cursed",
            Kind::Windowed => "windowed",
            Kind::Limited => "limited",
            Kind::Big503 => "big503",
        }
    }

    /// This kind's entry in the FIRST `history.read`. Every later page is
    /// a cursor the previous reply minted -- never one this file invents.
    fn first_page(self) -> serde_json::Value {
        match self {
            Kind::Cursed => json!({
                "resource_id": "cursed",
                "page": {"kind": "cursor", "cursor": "not a cursor this adapter minted"}
            }),
            Kind::Windowed => json!({
                "resource_id": "windowed",
                "page": {
                    "kind": "window", "resource_id": "windowed",
                    "start": "2026-01-01T00:00:00Z", "end": "2026-02-01T00:00:00Z"
                }
            }),
            other => json!({"resource_id": other.resource_id()}),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Injection {
    None,
    /// The reply does not fit a frame, so `write_reply` answers `err`
    /// instead. Small, and it FITS -- which is what round 3 mistook for
    /// delivery.
    Oversize,
    /// stdout dies mid-frame.
    StdoutError,
    /// The frame goes out and the process dies before the commit: written,
    /// dropped, and the sequence continues against a FRESH adapter. That
    /// is what death between the two means, and it needs no seam in main.
    DieAfterWrite,
    /// The rename that publishes the state file cannot happen.
    StateWriteFails,
    /// The advisory lock cannot be taken.
    LockUnavailable,
}

const INJECTIONS: [Injection; 6] = [
    Injection::None,
    Injection::Oversize,
    Injection::StdoutError,
    Injection::DieAfterWrite,
    Injection::StateWriteFails,
    Injection::LockUnavailable,
];

/// Mid-frame: every reply this matrix produces is longer than this.
const STDOUT_DIES_AT: usize = 64;
/// `spans` takes three; anything past this is a crawl that is not draining.
const MAX_PAGES: usize = 20;

// ---------------------------------------------------------------------
// Injection mechanics
// ---------------------------------------------------------------------

/// A writer that fails after `left` bytes, like a host that closed the
/// pipe.
struct Dies<'a> {
    out: &'a mut Vec<u8>,
    left: usize,
}

impl Write for Dies<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout closed"));
        }
        let n = buf.len().min(self.left);
        self.left -= n;
        self.out.extend_from_slice(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Puts a DIRECTORY where a file has to be, and restores whatever was
/// there on drop.
///
/// A directory is the one obstruction a process cannot walk through
/// whatever it is running as: `open` on it fails, and so does `rename`
/// onto it. Permission bits would not survive a CI container running as
/// root, and this check must not quietly stop checking there.
struct Obstruct {
    path: PathBuf,
    saved: Option<Vec<u8>>,
}

impl Obstruct {
    fn at(path: PathBuf) -> Obstruct {
        let saved = std::fs::read(&path).ok();
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir_all(&path).unwrap();
        Obstruct { path, saved }
    }
}

impl Drop for Obstruct {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
        if let Some(bytes) = &self.saved {
            let _ = std::fs::write(&self.path, bytes);
        }
    }
}

// ---------------------------------------------------------------------
// The trace: bytes on stdout, bytes on disk
// ---------------------------------------------------------------------

/// One directory's contents. `None` for an entry that is not a readable
/// file, so an obstruction is a state like any other.
type Snapshot = BTreeMap<String, Option<Vec<u8>>>;

fn snapshot(dir: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        out.insert(
            entry.file_name().to_string_lossy().into_owned(),
            std::fs::read(entry.path()).ok(),
        );
    }
    out
}

/// A snapshot as a human reads it. The comparison stays on bytes.
fn show(snapshot: &Snapshot) -> String {
    snapshot
        .iter()
        .map(|(name, bytes)| match bytes {
            Some(b) => format!("{name}={}", String::from_utf8_lossy(b)),
            None => format!("{name}=<not a readable file>"),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// One request, as an observer with a pipe and a directory listing sees it.
struct Step {
    requested: Vec<String>,
    written: Vec<u8>,
    /// The frame reached stdout in full.
    complete: bool,
    /// ...and it was THIS reply, not the `err` substituted for it.
    delivered: bool,
    before: Snapshot,
    after: Snapshot,
}

impl Step {
    fn frame(&self) -> Option<serde_json::Value> {
        if !self.complete {
            return None;
        }
        serde_json::from_slice(self.written.strip_suffix(b"\n")?).ok()
    }

    /// The `ok` body, or `None` for an `err` frame or no frame at all.
    fn ok_body(&self) -> Option<serde_json::Value> {
        self.frame()?.get("ok").cloned()
    }
}

struct Trace {
    state: PathBuf,
    steps: Vec<Step>,
    before_txids: BTreeMap<String, BTreeSet<String>>,
    after_txids: BTreeMap<String, BTreeSet<String>>,
    /// Resources that reported a drained crawl in a DELIVERED frame.
    drained: BTreeSet<String>,
    pages: usize,
}

impl Trace {
    /// Did any frame the host actually received carry a tombstone for this
    /// `local_id`?
    fn retracted_on_the_wire(&self, resource_id: &str, txid: &str) -> bool {
        let local_id = format!("{resource_id}:{txid}");
        self.steps
            .iter()
            .filter(|s| s.delivered)
            .filter_map(Step::ok_body)
            .any(|ok| {
                ok["observations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|o| o["local_id"] == local_id.as_str() && o["state"] == "tombstoned")
            })
    }
}

/// The txids one resource's state file holds, or an empty set if it holds
/// nothing readable.
fn baseline(state: &Path, resource_id: &str) -> BTreeSet<String> {
    let Ok(raw) = std::fs::read_to_string(state.join(format!("{resource_id}.json"))) else {
        return BTreeSet::new();
    };
    let Ok(file) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return BTreeSet::new();
    };
    file["history"]["txs"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(txid, _)| txid.clone())
        .collect()
}

fn history_stamp(state: &Path, resource_id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(state.join(format!("{resource_id}.json"))).ok()?;
    let file: serde_json::Value = serde_json::from_str(&raw).ok()?;
    file["history"]["as_of"].as_str().map(str::to_owned)
}

// ---------------------------------------------------------------------
// Driving one sequence
// ---------------------------------------------------------------------

fn unique_tag() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// Builds the frame, answers it, writes it under `inj`, and applies the
/// state writes iff that reply is what went out.
///
/// This is `main`'s loop body. It is spelled out here because the
/// injections are things that happen BETWEEN its steps, and a helper that
/// hid them would hide exactly what is being tested.
fn execute(
    adapter: &Adapter,
    state: &Path,
    id: u64,
    op: &str,
    params: serde_json::Value,
    requested: Vec<String>,
    inj: Injection,
) -> Step {
    let frame = json!({"id": id, "op": op, "params": params}).to_string();
    let (reply, commits) = adapter
        .handle(&frame)
        .expect("a framed request is answered");

    let blocked: Vec<Obstruct> = match inj {
        Injection::LockUnavailable => requested
            .iter()
            .map(|r| Obstruct::at(state.join(format!("{r}.lock"))))
            .collect(),
        Injection::StateWriteFails => requested
            .iter()
            .map(|r| Obstruct::at(state.join(format!("{r}.json"))))
            .collect(),
        _ => Vec::new(),
    };
    let before = snapshot(state);

    let mut written: Vec<u8> = Vec::new();
    let outcome = if inj == Injection::StdoutError {
        write_reply(
            &mut Dies {
                out: &mut written,
                left: STDOUT_DIES_AT,
            },
            &reply,
        )
    } else {
        write_reply(&mut written, &reply)
    };
    let (complete, delivered) = match outcome {
        Ok(d) => (true, d),
        Err(_) => (false, false),
    };
    if delivered && inj != Injection::DieAfterWrite {
        for commit in commits {
            commit.apply(&adapter.store);
        }
    }

    let after = snapshot(state);
    drop(blocked);
    Step {
        requested,
        written,
        complete,
        delivered,
        before,
        after,
    }
}

fn ids_of(resources: &[serde_json::Value]) -> Vec<String> {
    resources
        .iter()
        .map(|r| r["resource_id"].as_str().unwrap().to_owned())
        .collect()
}

fn run(kinds: &[Kind], inj: Injection, at: usize) -> Trace {
    let fx = fixture();
    let state = tmp_root(&format!("cb-{}", unique_tag()));
    let mut adapter = adapter_with(&fx.config, &fx.corpus, 0, &state);

    // Precondition: `retracts` remembers the txid the corpus answers 404
    // for, established by an earlier adapter lifetime over run 1.
    if kinds.contains(&Kind::Retracts) {
        adapter_with(&fx.config, &fx.corpus, 1, &state)
            .deliver(
                &json!({"id": 0, "op": OP_HISTORY_READ,
                        "params": {"resources": [{"resource_id": "retracts"}]}})
                .to_string(),
            )
            .unwrap();
    }
    let ids: Vec<String> = kinds
        .iter()
        .map(|k| k.resource_id().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let before_txids = ids
        .iter()
        .map(|r| (r.clone(), baseline(&state, r)))
        .collect();

    // Request 0 is a batched `balances.read`. It is never the injection
    // point -- "page k" starts at the first history page -- and it is what
    // makes rule 6's "not merely a balances one" mean something.
    let asked: Vec<String> = kinds.iter().map(|k| k.resource_id().to_owned()).collect();
    let mut steps = vec![execute(
        &adapter,
        &state,
        1,
        OP_BALANCES_READ,
        json!({"resource_ids": asked}),
        asked,
        Injection::None,
    )];

    let mut resources: Vec<serde_json::Value> = kinds.iter().map(|k| k.first_page()).collect();
    let mut drained = BTreeSet::new();
    let mut pages = 0;
    for page in 1..=MAX_PAGES {
        if resources.is_empty() {
            break;
        }
        pages = page;
        let here = page == at;
        let inj = if here { inj } else { Injection::None };
        let mut asked = resources.clone();
        if inj == Injection::Oversize {
            asked.extend((0..OVERFLOW_WALLETS).map(|n| json!({"resource_id": format!("o{n:04}")})));
        }
        let requested = ids_of(&asked);
        let step = execute(
            &adapter,
            &state,
            u64::try_from(page).unwrap() + 1,
            OP_HISTORY_READ,
            json!({"resources": asked}),
            requested,
            inj,
        );

        // The next round follows the cursors this reply MINTED, and only
        // those. A drained crawl -- `fetched` with `page.next: null` -- is
        // what rules 4 and 6 read as "this resource finished".
        resources.clear();
        if let Some(ok) = step.ok_body() {
            for entry in ok["statuses"].as_array().into_iter().flatten() {
                let (rid, next) = (&entry["resource_id"], &entry["page"]["next"]);
                if !next.is_null() {
                    resources.push(json!({"resource_id": rid, "page": next}));
                } else if step.delivered
                    && !entry["page"].is_null()
                    && !entry["outcome"]["fetched"].is_null()
                {
                    drained.insert(rid.as_str().unwrap().to_owned());
                }
            }
        }
        let stop = !step.complete;
        steps.push(step);
        if inj == Injection::DieAfterWrite {
            adapter = adapter_with(&fx.config, &fx.corpus, 0, &state);
        }
        // stdout died: `main` breaks its read loop and the process ends.
        if stop {
            break;
        }
    }
    assert!(pages < MAX_PAGES, "a crawl that is not draining");

    let after_txids = ids
        .iter()
        .map(|r| (r.clone(), baseline(&state, r)))
        .collect();
    Trace {
        state,
        steps,
        before_txids,
        after_txids,
        drained,
        pages,
    }
}

// ---------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------

#[derive(Default)]
struct Tally {
    rule2: usize,
    rule3: usize,
    rule4: usize,
    rule5: usize,
    rule6: usize,
    refusals: usize,
    stdout_deaths: usize,
    multi_page: usize,
}

fn judge(case: &str, kinds: &[Kind], inj: Injection, t: &Trace, tally: &mut Tally) {
    for (n, step) in t.steps.iter().enumerate() {
        let frame = step
            .written
            .strip_suffix(b"\n")
            .unwrap_or(step.written.as_slice());

        // 1. An oversized frame is a fatal kill with no resync.
        assert!(
            frame.len() <= MAX_FRAME_BYTES,
            "{case} request {n}: {} bytes on stdout, over MAX_FRAME_BYTES ({MAX_FRAME_BYTES})",
            frame.len()
        );

        // 2. If this reply is not what went out, the state did not move --
        //    blanket, over the whole directory. An envelope error is small
        //    and FITS a frame, which is exactly why "the write succeeded"
        //    is not the same question as "this reply was delivered".
        let refused = !step.complete || step.frame().is_some_and(|f| f.get("err").is_some());
        if refused {
            // Compared as BYTES, printed as text: a state file is JSON, and
            // a diff nobody can read is a diff nobody acts on.
            assert!(
                step.before == step.after,
                "{case} request {n}: what reached stdout carried none of this reply's \
                 observations, and the state moved anyway. A baseline that records an \
                 undelivered retraction is one no later sync re-derives.\n\
                 before: {}\nafter:  {}",
                show(&step.before),
                show(&step.after),
            );
            tally.rule2 += 1;
            if step.complete {
                tally.refusals += 1;
            } else {
                tally.stdout_deaths += 1;
            }
        }

        // 5. Every requested resource_id appears in statuses exactly once.
        if let Some(ok) = step.ok_body() {
            let mut got: Vec<String> = ok["statuses"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| e["resource_id"].as_str().unwrap().to_owned())
                .collect();
            let mut want = step.requested.clone();
            got.sort();
            want.sort();
            assert_eq!(got, want, "{case} request {n}: statuses, exactly once each");
            tally.rule5 += 1;
        }
    }

    // 3. Nothing leaves a baseline without having been retracted on the
    //    wire first. A txid in no baseline is never probed again.
    for (resource_id, before) in &t.before_txids {
        let after = t.after_txids.get(resource_id).cloned().unwrap_or_default();
        for txid in before.difference(&after) {
            assert!(
                t.retracted_on_the_wire(resource_id, txid),
                "{case}: {resource_id}:{txid} left the baseline and no delivered frame \
                 retracted it. Nothing probes a txid no baseline holds, so that \
                 retraction is suppressed permanently."
            );
            tally.rule3 += 1;
        }
    }

    if inj != Injection::None {
        return;
    }

    // 4. LIVENESS, FIXTURE-DERIVED. The corpus answers 404 for this txid
    //    and the baseline held it, so the crawl that drained MUST have
    //    retracted it, and MUST have forgotten it.
    if kinds.contains(&Kind::Retracts) && t.drained.contains("retracts") {
        let txid = gone_txid();
        assert!(
            t.retracted_on_the_wire("retracts", &txid),
            "{case}: the corpus answers 404 for retracts:{txid} and it was in the \
             baseline, yet the crawl drained without retracting it. A retraction that \
             is never emitted is never re-derived."
        );
        assert!(
            !t.after_txids["retracts"].contains(&txid),
            "{case}: retracts:{txid} was retracted on the wire and is still in the \
             baseline; the next sync will probe and retract it again forever."
        );
        tally.rule4 += 1;
    }

    // 6. A drained crawl commits a HISTORY write -- stamped by this run,
    //    not left at whatever the seeding run wrote.
    for resource_id in &t.drained {
        assert_eq!(
            history_stamp(&t.state, resource_id).as_deref(),
            Some(RUN0_STAMP),
            "{case}: {resource_id} drained its crawl in a delivered frame and no \
             history write from this run reached disk (a balances-only commit is not \
             this rule being satisfied)"
        );
        assert_ne!(
            history_stamp(&t.state, resource_id).as_deref(),
            Some(SEED_STAMP)
        );
        tally.rule6 += 1;
    }
    if t.pages > 1 {
        tally.multi_page += 1;
    }
}

#[test]
fn the_commit_boundary_holds_over_every_sequence() {
    let mut sequences: Vec<Vec<Kind>> = KINDS.iter().map(|k| vec![*k]).collect();
    for a in KINDS {
        for b in KINDS {
            sequences.push(vec![a, b]);
        }
    }

    let mut tally = Tally::default();
    let mut cases = 0;
    for kinds in &sequences {
        for inj in INJECTIONS {
            // `none` has no page to happen at.
            let points: &[usize] = if inj == Injection::None {
                &[0]
            } else {
                &[1, 2]
            };
            for at in points {
                let case = format!("{kinds:?} + {inj:?} at page {at}");
                let trace = run(kinds, inj, *at);
                judge(&case, kinds, inj, &trace, &mut tally);
                std::fs::remove_dir_all(&trace.state).unwrap();
                cases += 1;
            }
        }
    }

    // The matrix has to have EXERCISED each rule, or a green run means
    // only that nothing ran. The previous design's failure was rules that
    // doing nothing satisfied; this is the check for that failure mode
    // applied to the matrix itself.
    assert!(cases >= 700, "{cases} cases");
    assert!(tally.rule2 > 0, "no request was ever refused");
    assert!(tally.rule3 > 0, "no txid ever left a baseline");
    assert!(tally.rule4 > 0, "the liveness rule never fired");
    assert!(tally.rule5 > 0, "no ok reply was ever judged");
    assert!(tally.rule6 > 0, "no crawl ever committed a history write");
    assert!(tally.refusals > 0, "no reply was ever refused for size");
    assert!(tally.stdout_deaths > 0, "stdout never died mid-frame");
    assert!(tally.multi_page > 0, "no crawl ever spanned pages");
    eprintln!(
        "commit boundary: {cases} cases; refusals {}, stdout deaths {}, multi-page {}, \
         rules fired 2:{} 3:{} 4:{} 5:{} 6:{}",
        tally.refusals,
        tally.stdout_deaths,
        tally.multi_page,
        tally.rule2,
        tally.rule3,
        tally.rule4,
        tally.rule5,
        tally.rule6,
    );
}

// ---------------------------------------------------------------------
// Outside the matrix
// ---------------------------------------------------------------------

/// Rule 4's mechanism, executable on its own.
///
/// `--source` is bounded at 256 bytes, so this cannot be reached from
/// outside any more -- which is the point: the bound is what makes it
/// unreachable, and this is what stops the bound from having to be
/// re-argued the next time a field is added to `ObservationWire`.
///
/// A tombstone the page OMITTED was never emitted. Subtracting it from
/// what the crawl retracts is what keeps its txid in the baseline, and a
/// txid in a baseline is the only thing that ever gets probed again.
#[test]
fn a_tombstone_omitted_for_size_is_not_retracted() {
    let txid = "f".repeat(64);
    let ctx = map::Ctx {
        resource_id: "w".to_owned(),
        adapter_id: "sumer-bitcoin".to_owned(),
        // The unbounded field as it was: `Source::provider_id()` is the
        // `--source` string, and it rides in every tombstone's provenance.
        provider_id: format!("https://{}.invalid/api", "p".repeat(100_000)),
        observed_at: Rfc3339::new("2026-01-01T00:00:00Z").unwrap(),
    };
    let plan = map::Plan {
        items: vec![(
            map::Key::Mempool { txid: txid.clone() },
            map::tombstone(&ctx, &txid, Some(800_000), -1).unwrap(),
        )],
        high_water: (800_000, "a".repeat(64)),
    };

    let page = map::cut_page(plan, None, PAGE_BUDGET_BYTES).unwrap();
    assert!(
        page.observations.is_empty(),
        "step 1 cannot save it: the oversized field is not provider_extra"
    );
    assert!(page.degraded.is_some(), "an omission is reported");
    assert!(
        page.omitted.contains(&txid),
        "the omitted TOMBSTONE names its txid, so the crawl can subtract it"
    );

    let gone: BTreeSet<String> = [txid].into_iter().collect();
    let retracted: Vec<&String> = gone.difference(&page.omitted).collect();
    assert!(
        retracted.is_empty(),
        "a tombstone the host never received must not be recorded as retracted; \
         the txid has to stay in the baseline or nothing ever probes it again"
    );
}
