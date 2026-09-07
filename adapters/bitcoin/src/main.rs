//! `sumer-bitcoin-adapter`: a watch-only Bitcoin adapter over Esplora.
//!
//! One process, JSON Lines on stdin/stdout, `spec/wire.md` 1. Serial
//! (`max_in_flight: 1`): a sync is blocking HTTP and there is nothing for
//! a runtime to overlap.
//!
//! Two rules this file exists to keep:
//!
//! - **Nothing reaches stdout before the hello reply.** This process
//!   writes to stdout in exactly one place ([`write_reply`]), reached only
//!   from the request loop. Every diagnostic goes to stderr.
//! - **stdin EOF is the end.** The read loop exits on a zero-length read
//!   and `main` returns; work in flight is dropped, because its reply has
//!   nowhere to go (`spec/wire.md` 7).

mod map;
mod seen;
mod source;
mod wallet;

use map::{Ctx, Cursor, PAGE_BUDGET_BYTES};
use source::{FetchError, Source};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Display;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use sumer_wire::{
    BalancesReadParams, BalancesReadReply, CursorResumable, ErrorBody, HelloParams, HelloReply,
    HistoryReadParams, HistoryReadReply, PageReply, PageRequest, ProviderDetail, ReadOutcome,
    Reply, Request, RequestId, ResourceDescriptor, ResourceStatus, ResourcesListParams,
    ResourcesListReply, Rfc3339, StatusReadParams, StatusReadReply, WireErrorCode, MAX_FRAME_BYTES,
    OP_BALANCES_READ, OP_HELLO, OP_HISTORY_READ, OP_RESOURCES_LIST, OP_STATUS_READ,
};
use wallet::Wallet;

const ADAPTER_ID: &str = "sumer-bitcoin";
const PROTOCOL: &str = "1";

/// Blockstream's public Esplora. One protocol, three deployments: point
/// `--source` at mempool.space's `/api`, or at your own esplora/electrs,
/// and no code changes. Never at a service that wants an xpub.
const DEFAULT_SOURCE: &str = "https://blockstream.info/api";

/// The longest `--source` this adapter accepts. Over it is a usage error
/// and exit 2, exactly like `wallet.rs`'s `resource_id` bound.
///
/// **Rejected, never truncated.** `--source` becomes
/// `provenance.provider_id` on every observation this adapter emits and
/// every resource descriptor it lists; a truncated provider identity is a
/// falsified provenance, which is a worse answer than refusing to start.
const MAX_SOURCE_BYTES: usize = 256;

const USAGE: &str = "\
usage: sumer-bitcoin-adapter --wallets <file.json> [options]

  --wallets <path>     wallet definitions (required); see README.md
  --source <target>    https://host/api  (an Esplora deployment), or
                       file:<dir>        (a recorded corpus)
                       default: https://blockstream.info/api
  --state-dir <dir>    where the balance cache lives. Without it a failed
                       balance read answers `unavailable`, never `stale`.
  --record <dir>       write every HTTP response into <dir> as a replayable
                       corpus (HTTP source only)
";

fn main() -> std::process::ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("sumer-bitcoin-adapter: {e}\n\n{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    let wallets = match wallet::load(&options.wallets) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("sumer-bitcoin-adapter: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let adapter = Adapter {
        wallets,
        source: options.source,
        store: seen::Store::new(options.state_dir),
        crawls: RefCell::new(HashMap::new()),
    };

    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            // stdin EOF: the defined end of a connection. Exit.
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("sumer-bitcoin-adapter: stdin: {e}");
                break;
            }
        }
        let frame = line.trim_end_matches(['\n', '\r']);
        if frame.is_empty() {
            continue;
        }
        if let Some(reply) = adapter.handle(frame) {
            if let Err(e) = write_reply(&mut out, &reply) {
                eprintln!("sumer-bitcoin-adapter: stdout: {e}");
                break;
            }
        }
    }
    std::process::ExitCode::SUCCESS
}

/// The only place this process writes to stdout, and the only place the
/// frame ceiling is *enforced* rather than budgeted for.
///
/// A frame over `MAX_FRAME_BYTES` is a fatal kill with no resync
/// (`spec/wire.md` 2), so the one thing this function must never do is
/// write one. [`Budget`] upstream is what keeps a reply inside the ceiling;
/// this is the proof, and it answers an over-sized reply with an `err`
/// addressed to the same `id` -- something the host can read and act on --
/// instead of a connection it has to kill.
///
/// Reaching it means the request asked for more than a frame can carry:
/// hundreds of resources whose STATUS entries alone overflow it, which no
/// paging mechanism in this protocol can shed (every requested
/// `resource_id` appears in `statuses` exactly once). Fewer resources per
/// request is the answer, and the error says so.
///
fn write_reply(out: &mut impl Write, reply: &Reply<serde_json::Value>) -> io::Result<()> {
    let mut line = serde_json::to_string(reply)?;
    if line.len() > MAX_FRAME_BYTES {
        let (Reply::Ok { id, .. } | Reply::Err { id, .. }) = reply;
        eprintln!(
            "sumer-bitcoin-adapter: a {}-byte reply does not fit MAX_FRAME_BYTES \
             ({MAX_FRAME_BYTES}); answering err instead of writing a fatal frame",
            line.len()
        );
        let err: Reply<serde_json::Value> = Reply::err(
            *id,
            internal(format!(
                "this reply is {} bytes and MAX_FRAME_BYTES is {MAX_FRAME_BYTES}; \
                 ask for fewer resources per request",
                line.len()
            )),
        );
        line = serde_json::to_string(&err)?;
    }
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

// ---------------------------------------------------------------------
// The reply's byte budget
// ---------------------------------------------------------------------

/// The bytes of a reply that are neither a status entry nor an
/// observation: `{"id":18446744073709551615,"ok":{"observations":[],
/// "statuses":[]}}` is 66 of them, plus one comma per element. 512 is
/// slack, deliberately -- the accounting here does not have to be exact,
/// because [`write_reply`] is what makes the ceiling true.
const ENVELOPE_BYTES: usize = 512;

/// What a status entry can still grow by after it has been measured: the
/// `page` object it does not carry yet (`{"cursor_resumable":"exact",
/// "next":{"kind":"cursor","cursor":"<20>:<64>:m:<64>"},
/// "page_size_reduced_to":4294967295}`), and the byte between
/// `"page_empty":false` and `"page_empty":true`.
const PAGE_REPLY_BYTES: usize = 320;

/// One reply's remaining byte budget.
///
/// **`MAX_FRAME_BYTES` bounds the whole REPLY, not one resource's share of
/// it.** A reply carries every requested resource's observations, its
/// statuses and their `provider_detail` evidence in one frame, so a budget
/// spent per resource is not a frame limit at all: two resources with
/// ordinary transactions overflowed it, and an oversized frame is a fatal
/// kill with no resync (`spec/wire.md` 2). This is that budget, spent once
/// across the whole reply.
struct Budget(usize);

/// What one status entry costs before any provider evidence is attached to
/// it: the entry as it will be serialized, plus the `page` object it does
/// not carry yet.
fn status_bytes(resource_id: &str) -> usize {
    serde_json::to_vec(&status(
        resource_id,
        ReadOutcome::Fetched { page_empty: true },
    ))
    .map_or(0, |v| v.len())
    .saturating_add(PAGE_REPLY_BYTES)
}

impl Budget {
    fn new() -> Budget {
        Budget(MAX_FRAME_BYTES.saturating_sub(ENVELOPE_BYTES))
    }

    /// Reserves the mandatory status entry of EVERY requested resource,
    /// before a single observation is admitted.
    ///
    /// An entry per requested `resource_id` is mandatory and appears
    /// exactly once (`spec/observation.md` 6), so what the statuses cost
    /// is knowable before paging begins. Charging
    /// each one only when its turn came was the bug: the resources at the
    /// front of a batch spent the bytes the resources behind them were
    /// always going to need, and one real wallet followed by 6,100
    /// unknown resources put 1,061,875 bytes on the wire.
    fn reserve<'a>(&mut self, resource_ids: impl Iterator<Item = &'a str>) {
        for resource_id in resource_ids {
            self.0 = self.0.saturating_sub(status_bytes(resource_id));
        }
    }

    /// Charges what an entry costs BEYOND its reservation, and hands it
    /// back. The excess is `provider_detail` evidence -- a provider's
    /// error body, which no reservation can predict the size of -- and an
    /// entry carrying one never carries a `page`, so the reservation's
    /// [`PAGE_REPLY_BYTES`] pays for the first 320 bytes of it.
    fn charge(&mut self, entry: ResourceStatus) -> ResourceStatus {
        let cost = serde_json::to_vec(&entry).map_or(0, |v| v.len());
        self.0 = self
            .0
            .saturating_sub(cost.saturating_sub(status_bytes(&entry.resource_id)));
        entry
    }

    fn spend(&mut self, bytes: usize) {
        self.0 = self.0.saturating_sub(bytes);
    }

    /// What one page of observations may spend: never more than this reply
    /// has left, and never more than a page's own cap (ADR 0004 3).
    fn page(&self) -> usize {
        self.0.min(PAGE_BUDGET_BYTES)
    }
}

// ---------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------

struct Options {
    wallets: PathBuf,
    source: Source,
    state_dir: Option<PathBuf>,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Options, String> {
        let (mut wallets, mut state_dir, mut record) = (None, None, None);
        let mut target = DEFAULT_SOURCE.to_owned();
        let mut args = args.peekable();
        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{flag}: missing its value"))
            };
            match flag.as_str() {
                "--wallets" => wallets = Some(PathBuf::from(value()?)),
                "--source" => target = value()?,
                "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
                "--record" => record = Some(PathBuf::from(value()?)),
                "-h" | "--help" => return Err("help requested".to_owned()),
                other => return Err(format!("unknown argument {other:?}")),
            }
        }
        let wallets = wallets.ok_or("--wallets is required")?;
        if target.len() > MAX_SOURCE_BYTES {
            return Err(format!(
                "--source is {} bytes and the limit is {MAX_SOURCE_BYTES}",
                target.len()
            ));
        }
        let source = match target.strip_prefix("file:") {
            Some(dir) => {
                if record.is_some() {
                    return Err(
                        "--record needs an HTTP --source; there is nothing to record from a corpus"
                            .to_owned(),
                    );
                }
                Source::replay(Path::new(dir), fixture_run())
            }
            None => Source::http(target, record),
        };
        Ok(Options {
            wallets,
            source,
            state_dir,
        })
    }
}

/// `SUMER_FIXTURE_RUN` (`spec/wire.md` 11) selects which run subdirectory
/// of a corpus this process replays, so a two-phase scenario -- write
/// state, then fail a fetch -- is two adapter lifetimes over two corpora.
fn fixture_run() -> u64 {
    std::env::var("SUMER_FIXTURE_RUN")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------

struct Adapter {
    wallets: Vec<Wallet>,
    source: Source,
    store: seen::Store,
    /// One point-in-time crawl per resource, alive from the first page
    /// request until the crawl drains or the process exits. `RefCell`
    /// rather than a lock because this adapter is serial by declaration
    /// (`max_in_flight: 1`) and single-threaded by construction.
    ///
    /// This is not persistence. The host spawns one adapter process and
    /// multiplexes many requests over it, so a snapshot held between two
    /// pages of one crawl is work already done inside one connection,
    /// not state carried across them.
    crawls: RefCell<HashMap<String, Crawl>>,
}

/// One crawl's snapshot: everything a page of it is planned from.
struct Crawl {
    /// The instant the crawl was taken. Every page of it reports this as
    /// `observed_at`, because that is when the provider was observed --
    /// pages two and three did not observe anything.
    observed_at: Rfc3339,
    chain: wallet::ChainData,
}

fn invalid(e: impl Display) -> ErrorBody {
    ErrorBody::new(WireErrorCode::InvalidRequest, e.to_string())
}

fn internal(e: impl Display) -> ErrorBody {
    ErrorBody::new(WireErrorCode::Internal, e.to_string())
}

/// A status entry with every field this adapter never populates left
/// absent by construction: `credential_expires_at` and
/// `strong_auth_expires_at` do not exist for a watch-only wallet -- there
/// is no credential and no authentication session.
///
/// `degraded` starts empty and is filled in by [`map::cut_page`] with one
/// entry per record a page had to drop for size. It used to be documented here as
/// unreachable, on the grounds that this adapter's `provider_extra` is a
/// fixed handful of scalars -- which was wrong: `block_hash` is one of
/// them, it comes from the provider, and a provider scalar has no length
/// this adapter gets to assume.
fn status(resource_id: &str, outcome: ReadOutcome) -> ResourceStatus {
    ResourceStatus {
        resource_id: resource_id.to_owned(),
        outcome,
        degraded: Vec::new(),
        provider_detail: None,
        page: None,
        credential_expires_at: None,
        strong_auth_expires_at: None,
        history_start: None,
    }
}

fn with_detail(mut s: ResourceStatus, detail: &ProviderDetail) -> ResourceStatus {
    s.provider_detail = Some(detail.clone());
    s
}

/// A `resource_id` this adapter was never configured with.
///
/// `not_fetched`, not `gone`: `gone` claims the resource used to exist and
/// no longer does, which this adapter cannot know -- a wallet it has no
/// config line for is one it never had.
fn unknown_resource(resource_id: &str) -> ResourceStatus {
    with_detail(
        status(resource_id, ReadOutcome::NotFetched),
        &ProviderDetail {
            code: "unknown_resource".to_owned(),
            message: "no wallet with this resource_id is configured".to_owned(),
            raw: serde_json::Value::Null,
        },
    )
}

/// Rejects a request that names the same `resource_id` twice.
///
/// Every requested `resource_id` appears in `statuses` EXACTLY ONCE
/// (`spec/observation.md` 6). A repeat has no conforming answer -- one
/// entry drops a request the host made, two break the rule -- so the
/// request cannot be processed as a whole, which is what an envelope error
/// is for.
///
/// The offending id stays OUT of `err.detail`: `spec/wire.md` 8 says an
/// `err` payload naming a resource is the signal a status outcome was the
/// right channel, and here it is not -- the fault is in the shape of the
/// request, not a fact about any resource.
fn no_repeats<'a>(resource_ids: impl Iterator<Item = &'a str>) -> Result<(), ErrorBody> {
    let mut seen = BTreeSet::new();
    for resource_id in resource_ids {
        if !seen.insert(resource_id) {
            return Err(ErrorBody::new(
                WireErrorCode::InvalidRequest,
                "a resource_id is named more than once in this request; every requested \
                 resource_id appears in statuses exactly once, so a repeat has no answer",
            ));
        }
    }
    Ok(())
}

impl Adapter {
    fn handle(&self, frame: &str) -> Option<Reply<serde_json::Value>> {
        let value: serde_json::Value = match serde_json::from_str(frame) {
            Ok(v) => v,
            Err(e) => {
                // No id, no addressee. The host owns framing; a frame this
                // adapter cannot parse is not something it can answer.
                eprintln!("sumer-bitcoin-adapter: unparseable frame: {e}");
                return None;
            }
        };
        let id = value.get("id").and_then(serde_json::Value::as_u64);
        let request: Request = match serde_json::from_value(value) {
            Ok(r) => r,
            Err(e) => return Some(Reply::err(RequestId(id?), invalid(e))),
        };
        let id = request.id;
        let params = if request.params.is_null() {
            serde_json::json!({})
        } else {
            request.params
        };
        let result = match request.op.as_str() {
            OP_HELLO => hello(&params),
            OP_RESOURCES_LIST => self.resources_list(&params),
            OP_BALANCES_READ => self.balances_read(&params),
            OP_HISTORY_READ => self.history_read(&params),
            OP_STATUS_READ => self.status_read(&params),
            other => Err(ErrorBody::new(
                WireErrorCode::Unsupported,
                "operation not supported by this adapter",
            )
            .with_detail(serde_json::json!({"op": other}))),
        };
        Some(match result {
            Ok(body) => Reply::ok(id, body),
            Err(err) => Reply::err(id, err),
        })
    }

    fn lookup(&self, resource_id: &str) -> Option<&Wallet> {
        self.wallets.iter().find(|w| w.resource_id == resource_id)
    }

    fn ctx(&self, w: &Wallet, observed_at: &Rfc3339) -> Ctx {
        Ctx {
            resource_id: w.resource_id.clone(),
            adapter_id: ADAPTER_ID.to_owned(),
            provider_id: self.source.provider_id(),
            observed_at: observed_at.clone(),
        }
    }

    fn observed_at(&self) -> Result<Rfc3339, ErrorBody> {
        map::rfc3339_utc(self.source.now()).map_err(|e| internal(format!("clock: byte {}", e.at)))
    }

    fn resources_list(&self, params: &serde_json::Value) -> Result<serde_json::Value, ErrorBody> {
        let _: ResourcesListParams = serde_json::from_value(params.clone()).map_err(invalid)?;
        let resources = self
            .wallets
            .iter()
            .map(|w| {
                let mut extra = serde_json::Map::new();
                // The COUNT and the fingerprint, never the addresses: a
                // resource listing is not the place to hand a wallet's
                // address set to anything that reads a log.
                extra.insert("address_count".to_owned(), w.addresses.len().into());
                extra.insert(
                    "address_set_sha256".to_owned(),
                    w.address_hash.clone().into(),
                );
                ResourceDescriptor {
                    resource_id: w.resource_id.clone(),
                    provider_id: self.source.provider_id(),
                    kind: "bitcoin_wallet".to_owned(),
                    label: w.label.clone(),
                    provider_extra: Some(extra),
                }
            })
            .collect();
        serde_json::to_value(ResourcesListReply { resources }).map_err(internal)
    }

    fn balances_read(&self, params: &serde_json::Value) -> Result<serde_json::Value, ErrorBody> {
        let params: BalancesReadParams = serde_json::from_value(params.clone()).map_err(invalid)?;
        no_repeats(params.resource_ids.iter().map(String::as_str))?;
        let observed_at = self.observed_at()?;
        let mut observations = Vec::new();
        let mut statuses = Vec::new();
        let mut abandoned = false;

        for resource_id in &params.resource_ids {
            let Some(w) = self.lookup(resource_id) else {
                statuses.push(unknown_resource(resource_id));
                continue;
            };
            let ctx = self.ctx(w, &observed_at);
            // Two lines always, whatever happened. `amount: null` is
            // UNKNOWN and is never rendered "0": a balance that could not
            // be read is not a balance of nothing.
            let (mut confirmed, mut unconfirmed) = (None, None);
            let outcome = if abandoned {
                status(resource_id, ReadOutcome::NotFetched)
            } else {
                match wallet::balances(&self.source, w) {
                    Ok((c, u)) => {
                        confirmed = Some(c);
                        unconfirmed = Some(u);
                        // The cache is written HERE, as the figures are
                        // read, and is not gated on the reply going out:
                        // everything it holds is re-derived by the next
                        // successful read, so a write for a reply the host
                        // never received costs nothing (ADR 0004 7). A
                        // failure is logged and survived -- the next failed
                        // read then answers `unavailable` instead of
                        // `stale`.
                        if let Err(e) = self.store.save_balances(
                            &w.resource_id,
                            &w.address_hash,
                            &observed_at,
                            confirmed,
                            unconfirmed,
                        ) {
                            eprintln!(
                                "sumer-bitcoin-adapter: could not cache {}'s balances: {e}",
                                w.resource_id
                            );
                        }
                        status(resource_id, ReadOutcome::Fetched { page_empty: false })
                    }
                    Err(FetchError::RateLimited {
                        retry_after_ms,
                        detail,
                    }) => {
                        // Everything after this in the batch is
                        // `not_fetched`: hammering a provider that just
                        // said stop is not a read strategy.
                        abandoned = true;
                        with_detail(
                            status(resource_id, ReadOutcome::RateLimited { retry_after_ms }),
                            &detail,
                        )
                    }
                    Err(FetchError::Unavailable { detail }) => {
                        let recorded = self.store.load(&w.resource_id, &w.address_hash);
                        // The BALANCES' own timestamp. A history read
                        // observes no balance and never stamps one, so this
                        // is when the figures below were actually seen.
                        match recorded.balances_as_of.clone() {
                            Some(as_of) => {
                                confirmed = recorded.confirmed;
                                unconfirmed = recorded.unconfirmed;
                                with_detail(
                                    status(resource_id, ReadOutcome::Stale { as_of }),
                                    &detail,
                                )
                            }
                            None => {
                                with_detail(status(resource_id, ReadOutcome::Unavailable), &detail)
                            }
                        }
                    }
                }
            };
            observations
                .extend(map::balance_lines(&ctx, confirmed, unconfirmed).map_err(internal)?);
            statuses.push(outcome);
        }
        serde_json::to_value(BalancesReadReply {
            observations,
            statuses,
        })
        .map_err(internal)
    }

    fn history_read(&self, params: &serde_json::Value) -> Result<serde_json::Value, ErrorBody> {
        let params: HistoryReadParams = serde_json::from_value(params.clone()).map_err(invalid)?;
        no_repeats(params.resources.iter().map(|q| q.resource_id.as_str()))?;
        let mut observations = Vec::new();
        let mut statuses = Vec::new();
        let mut abandoned = false;
        // ONE budget for the whole reply, not one per resource -- and
        // every resource's mandatory status entry is reserved out of it
        // before any of them is allowed to spend a byte on observations.
        let mut budget = Budget::new();
        budget.reserve(params.resources.iter().map(|q| q.resource_id.as_str()));

        for query in &params.resources {
            let resource_id = &query.resource_id;
            // A cursor this adapter did not mint, or a window it does not
            // serve, is a request it cannot process AS A WHOLE -- an
            // envelope error, and deliberately without a `resource_id` in
            // the detail (spec/wire.md 8: wanting one there is the signal
            // that a status outcome was the right channel, and neither of
            // these is a fact about a resource).
            let from = match &query.page {
                None => None,
                Some(PageRequest::Cursor { cursor }) => {
                    Some(Cursor::parse(cursor).map_err(invalid)?)
                }
                Some(PageRequest::Window { .. }) => {
                    return Err(ErrorBody::new(
                        WireErrorCode::InvalidRequest,
                        "this adapter serves cursor pages only; Bitcoin history is ordered by \
                         block, not by wall-clock time",
                    )
                    .with_detail(serde_json::json!({"page_kind": "window"})))
                }
            };
            let Some(w) = self.lookup(resource_id) else {
                statuses.push(budget.charge(unknown_resource(resource_id)));
                continue;
            };
            if abandoned {
                statuses.push(budget.charge(status(resource_id, ReadOutcome::NotFetched)));
                continue;
            }

            // A request with no `page` starts a NEW crawl (Ruling A8:
            // absent means "from the start of available history"); a
            // cursor continues the crawl already in hand, and falls back
            // to a fresh one if this process has no memory of it.
            let start_a_crawl = from.is_none() || !self.crawls.borrow().contains_key(resource_id);
            if start_a_crawl {
                let observed_at = self.observed_at()?;
                match wallet::sync(&self.source, w) {
                    Ok(chain) => {
                        self.crawls
                            .borrow_mut()
                            .insert(resource_id.clone(), Crawl { observed_at, chain });
                    }
                    // ANY fetch failure suppresses the diff ENTIRELY: no
                    // observations, and no `page` either -- claiming a
                    // resume point for a read that did not happen would be
                    // fiction.
                    Err(FetchError::RateLimited {
                        retry_after_ms,
                        detail,
                    }) => {
                        abandoned = true;
                        statuses.push(budget.charge(with_detail(
                            status(resource_id, ReadOutcome::RateLimited { retry_after_ms }),
                            &detail,
                        )));
                        continue;
                    }
                    // `unavailable`, never `stale`: nothing is cached
                    // about a history, so there is no prior answer to be
                    // stale about. The diff is suppressed entirely and no
                    // `page` is claimed -- a read that did not happen names
                    // no resume point.
                    Err(FetchError::Unavailable { detail }) => {
                        statuses.push(budget.charge(with_detail(
                            status(resource_id, ReadOutcome::Unavailable),
                            &detail,
                        )));
                        continue;
                    }
                }
            }

            // The status entry is mandatory for every requested resource,
            // so the reply pays for it before it pays for any of this
            // resource's observations.
            let mut entry = budget.charge(status(
                resource_id,
                ReadOutcome::Fetched { page_empty: true },
            ));

            // Every page of one crawl is served from the SAME snapshot --
            // the same transactions, the same remembered state, the same
            // `observed_at`. Re-reading live data between pages would let
            // a transaction arriving mid-pagination shift every later
            // page, duplicating, skipping or reordering observations while
            // the cursor advanced over a dataset that moved underneath it.
            let page = {
                let crawls = self.crawls.borrow();
                let Some(crawl) = crawls.get(resource_id) else {
                    return Err(internal("the crawl snapshot vanished before it was read"));
                };
                let chain = map::Chain {
                    txs: &crawl.chain.txs,
                    mempool: &crawl.chain.mempool,
                };
                let ctx = self.ctx(w, &crawl.observed_at);
                let plan = map::plan(&ctx, &chain, &w.owned, from.as_ref()).map_err(internal)?;
                map::cut_page(plan, from.as_ref(), budget.page()).map_err(internal)?
            };
            budget.spend(page.bytes);

            // A drained crawl is a spent snapshot, and nothing else: this
            // adapter writes no history state, so there is nothing here to
            // commit and nothing to gate on delivery.
            if page.next.is_none() {
                self.crawls.borrow_mut().remove(resource_id);
            }

            entry.outcome = ReadOutcome::Fetched {
                page_empty: page.observations.is_empty(),
            };
            entry.degraded = page.degraded;
            entry.page = Some(PageReply {
                cursor_resumable: CursorResumable::Exact,
                next: page
                    .next
                    .map(|c| PageRequest::Cursor { cursor: c.encode() }),
                window_capped_to: None,
                page_size_reduced_to: page.page_size_reduced_to,
            });
            observations.extend(page.observations);
            statuses.push(entry);
        }
        serde_json::to_value(HistoryReadReply {
            observations,
            statuses,
        })
        .map_err(internal)
    }

    /// Reachability, one address per wallet. No `stale` here: a cached
    /// answer says nothing about whether the provider is reachable NOW,
    /// which is the only question this op asks.
    fn status_read(&self, params: &serde_json::Value) -> Result<serde_json::Value, ErrorBody> {
        let params: StatusReadParams = serde_json::from_value(params.clone()).map_err(invalid)?;
        no_repeats(params.resource_ids.iter().map(String::as_str))?;
        let mut statuses = Vec::new();
        let mut abandoned = false;
        for resource_id in &params.resource_ids {
            let Some(w) = self.lookup(resource_id) else {
                statuses.push(unknown_resource(resource_id));
                continue;
            };
            let Some(address) = w.addresses.first() else {
                statuses.push(unknown_resource(resource_id));
                continue;
            };
            if abandoned {
                statuses.push(status(resource_id, ReadOutcome::NotFetched));
                continue;
            }
            statuses.push(match self.source.address_stats(address) {
                // `status.read` carries no observations at all, so its
                // page is empty by definition.
                Ok(_) => status(resource_id, ReadOutcome::Fetched { page_empty: true }),
                Err(FetchError::RateLimited {
                    retry_after_ms,
                    detail,
                }) => {
                    abandoned = true;
                    with_detail(
                        status(resource_id, ReadOutcome::RateLimited { retry_after_ms }),
                        &detail,
                    )
                }
                Err(FetchError::Unavailable { detail }) => {
                    with_detail(status(resource_id, ReadOutcome::Unavailable), &detail)
                }
            });
        }
        serde_json::to_value(StatusReadReply { statuses }).map_err(internal)
    }
}

fn hello(params: &serde_json::Value) -> Result<serde_json::Value, ErrorBody> {
    let params: HelloParams = serde_json::from_value(params.clone()).map_err(invalid)?;
    if !params.protocol.iter().any(|v| v == PROTOCOL) {
        return Err(ErrorBody::new(
            WireErrorCode::UnsupportedProtocol,
            "this adapter speaks protocol 1 only",
        )
        .with_detail(serde_json::json!({"offered": params.protocol})));
    }
    serde_json::to_value(HelloReply {
        protocol: PROTOCOL.to_owned(),
        adapter_id: ADAPTER_ID.to_owned(),
        adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
        capabilities: vec![
            OP_RESOURCES_LIST.to_owned(),
            OP_BALANCES_READ.to_owned(),
            OP_HISTORY_READ.to_owned(),
            OP_STATUS_READ.to_owned(),
        ],
        local_id_derivation: map::LOCAL_ID_DERIVATION.to_owned(),
        // Serial by design: one blocking HTTP sync at a time.
        max_in_flight: 1,
    })
    .map_err(internal)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn hello_declares_the_four_read_capabilities_and_the_derivation() {
        let ok = hello(&serde_json::json!({"protocol": ["1"]})).unwrap();
        assert_eq!(ok["protocol"], "1");
        assert_eq!(ok["adapter_id"], ADAPTER_ID);
        assert_eq!(ok["local_id_derivation"], "btc-txid@1");
        assert_eq!(ok["max_in_flight"], 1);
        assert_eq!(ok["capabilities"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn hello_refuses_a_version_it_was_not_offered() {
        let Err(err) = hello(&serde_json::json!({"protocol": ["2", "999"]})) else {
            panic!("this adapter speaks protocol 1 only");
        };
        assert_eq!(err.code, WireErrorCode::UnsupportedProtocol);
    }

    // -----------------------------------------------------------------
    // End to end over a corpus: a whole sync, through the same code path a
    // real one takes -- including a failed fetch reporting NOTHING rather
    // than a partial history.
    // -----------------------------------------------------------------

    pub(crate) const ADDR: &str = "bc1qexample0";

    fn ok_of(reply: Reply<serde_json::Value>) -> serde_json::Value {
        match reply {
            Reply::Ok { ok, .. } => ok,
            Reply::Err { err, .. } => panic!("expected ok, got err: {err:?}"),
        }
    }

    fn request(op: &str, params: serde_json::Value) -> String {
        serde_json::json!({"id": 1, "op": op, "params": params}).to_string()
    }

    impl Adapter {
        /// The WHOLE path a reply takes in `main`: build it and write it.
        ///
        /// Returns what the HOST receives -- which for a reply too large
        /// for a frame is the `err` [`write_reply`] substituted, not the
        /// reply it refused.
        fn deliver(&self, frame: &str) -> Option<Reply<serde_json::Value>> {
            let reply = self.handle(frame)?;
            let mut out: Vec<u8> = Vec::new();
            write_reply(&mut out, &reply).unwrap();
            let written = out.strip_suffix(b"\n").expect("one line, LF-terminated");
            Some(serde_json::from_slice(written).expect("a reply frame is a reply"))
        }
    }

    /// A two-run corpus: run 0 answers everything, run 1 fails the chain
    /// listing with a 500 and has no address stats at all.
    fn fetch_fail_corpus(txid: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sumer-btc-e2e-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, body: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write("run0/now", "1767225600");
        write(
            &format!("run0/address_{ADDR}.json"),
            &serde_json::json!({
                "address": ADDR,
                "chain_stats": {"funded_txo_sum": 900, "spent_txo_sum": 0, "tx_count": 1},
                "mempool_stats": {"funded_txo_sum": 0, "spent_txo_sum": 0, "tx_count": 0},
            })
            .to_string(),
        );
        write(
            &format!("run0/address_{ADDR}_txs_chain.json"),
            &serde_json::json!([{
                "txid": txid,
                "fee": 100,
                "status": {
                    "confirmed": true, "block_height": 800_000,
                    "block_hash": "0".repeat(64), "block_time": 1_600_000_000i64
                },
                "vin": [{"prevout": {"scriptpubkey_address": "bc1qthem0000", "value": 1_000}}],
                "vout": [{"scriptpubkey_address": ADDR, "value": 900}],
            }])
            .to_string(),
        );
        write(&format!("run0/address_{ADDR}_txs_mempool.json"), "[]");
        // Run 1: the provider is having a bad day.
        write("run1/now", "1767312000");
        write(
            &format!("run1/address_{ADDR}_txs_chain.status"),
            "500\nupstream is on fire",
        );
        root
    }

    fn adapter_over(root: &Path, run: u64, state: &Path) -> Adapter {
        let config = root.join("wallets.json");
        if !config.exists() {
            std::fs::write(
                &config,
                serde_json::json!({"wallets": [{"resource_id": "w", "addresses": [ADDR]}]})
                    .to_string(),
            )
            .unwrap();
        }
        Adapter {
            wallets: wallet::load(&config).unwrap(),
            source: Source::replay(root, run),
            store: seen::Store::new(Some(state.to_path_buf())),
            crawls: RefCell::new(HashMap::new()),
        }
    }

    /// A read that failed produces ZERO observations -- there is no
    /// partial diff -- and the balances the last good read established,
    /// carried by `stale { as_of }`. The history half is `unavailable`:
    /// nothing about a history is cached, so there is no prior answer for
    /// it to be stale about.
    #[test]
    fn a_failed_fetch_suppresses_the_diff_entirely() {
        let txid = "a".repeat(64);
        let root = fetch_fail_corpus(&txid);
        let state = root.join("state");

        // Phase 1: everything answers.
        let phase1 = adapter_over(&root, 0, &state);
        let balances = ok_of(
            phase1
                .deliver(&request(
                    OP_BALANCES_READ,
                    serde_json::json!({"resource_ids": ["w"]}),
                ))
                .unwrap(),
        );
        assert_eq!(balances["observations"][0]["amount"]["amount"], "900");
        assert_eq!(balances["observations"][1]["amount"]["amount"], "0");
        assert_eq!(
            balances["statuses"][0]["outcome"]["fetched"]["page_empty"],
            false
        );

        let history = ok_of(
            phase1
                .deliver(&request(
                    OP_HISTORY_READ,
                    serde_json::json!({"resources": [{"resource_id": "w"}]}),
                ))
                .unwrap(),
        );
        assert_eq!(history["observations"].as_array().unwrap().len(), 1);
        assert_eq!(history["observations"][0]["local_id"], format!("w:{txid}"));
        assert_eq!(history["observations"][0]["state"], "active");
        assert_eq!(history["observations"][0]["amount"]["amount"], "900");
        assert!(history["statuses"][0]["page"]["next"].is_null(), "drained");
        assert_eq!(history["statuses"][0]["page"]["cursor_resumable"], "exact");

        // Phase 2: the chain listing 500s. The transaction is absent from
        // every listing this run -- and absence is NOT evidence.
        let phase2 = adapter_over(&root, 1, &state);
        let history = ok_of(
            phase2
                .deliver(&request(
                    OP_HISTORY_READ,
                    serde_json::json!({"resources": [{"resource_id": "w"}]}),
                ))
                .unwrap(),
        );
        assert_eq!(
            history["observations"].as_array().unwrap().len(),
            0,
            "no partial diff: a half-read chain is not a history"
        );
        let status = &history["statuses"][0];
        assert_eq!(
            status["outcome"], "unavailable",
            "a history read caches nothing, so a failed one has no prior \
             answer to report as stale"
        );
        assert!(
            status["page"].is_null(),
            "a read that did not happen claims no resume point"
        );
        assert_eq!(status["provider_detail"]["code"], "http_500");
        assert_eq!(
            status["provider_detail"]["raw"]["body"],
            "upstream is on fire"
        );

        let balances = ok_of(
            phase2
                .deliver(&request(
                    OP_BALANCES_READ,
                    serde_json::json!({"resource_ids": ["w"]}),
                ))
                .unwrap(),
        );
        assert_eq!(
            balances["statuses"][0]["outcome"]["stale"]["as_of"],
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(
            balances["observations"][0]["amount"]["amount"], "900",
            "the last good balance is preserved and reported as stale"
        );

        // Phase 3: run 0 again, with the phase-1 cache still on disk. The
        // transaction is back in the listing and reported active. This
        // adapter emits no tombstone under any circumstances (ADR 0004 7),
        // and the assertion below is the standing check on that.
        let phase3 = adapter_over(&root, 0, &state);
        let history = ok_of(
            phase3
                .deliver(&request(
                    OP_HISTORY_READ,
                    serde_json::json!({"resources": [{"resource_id": "w"}]}),
                ))
                .unwrap(),
        );
        assert_eq!(history["observations"][0]["state"], "active");
        assert!(history["observations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["tombstone_reason"].is_null()));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// One address's `count` confirmed transactions, chained across the
    /// 25-per-page chain listing exactly as Esplora pages it.
    pub(crate) fn paged_corpus(corpus: &Path, addr: &str, count: usize) {
        std::fs::create_dir_all(corpus).unwrap();
        std::fs::write(
            corpus.join(format!("address_{addr}_txs_mempool.json")),
            "[]",
        )
        .unwrap();

        let txs: Vec<serde_json::Value> = (0..count)
            .map(|n| {
                serde_json::json!({
                    "txid": format!("{n:064x}"),
                    "fee": 200,
                    "status": {
                        "confirmed": true,
                        "block_height": 800_000 + n / 10,
                        "block_hash": format!("{:064x}", 800_000 + n / 10),
                        "block_time": 1_700_000_000i64,
                    },
                    "vin": [{"prevout": {"scriptpubkey_address": "bc1qthem0000", "value": 1_100}}],
                    "vout": [{"scriptpubkey_address": ADDR, "value": 1_000}],
                })
            })
            .collect();

        let mut name = format!("address_{addr}_txs_chain");
        for chunk in txs.chunks(25) {
            std::fs::write(
                corpus.join(format!("{name}.json")),
                serde_json::Value::Array(chunk.to_vec()).to_string(),
            )
            .unwrap();
            let last = chunk.last().unwrap()["txid"].as_str().unwrap().to_owned();
            name = format!("address_{addr}_txs_chain_{last}");
        }
        // A final short page ends the crawl. When the last chunk was
        // exactly full, that page has to be an empty one.
        if count.is_multiple_of(25) {
            std::fs::write(corpus.join(format!("{name}.json")), "[]").unwrap();
        }
    }

    /// One crawl is ONE SNAPSHOT, and later pages of it never touch the
    /// provider again -- proved by deleting the entire corpus after the
    /// first page and draining the rest anyway.
    #[test]
    fn later_pages_of_a_crawl_are_served_from_the_snapshot() {
        const COUNT: usize = 1_200;
        let root = std::env::temp_dir().join(format!(
            "sumer-btc-snapshot-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let corpus = root.join("corpus/run0");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("now"), "1767225600").unwrap();
        paged_corpus(&corpus, ADDR, COUNT);
        let config = wallets_json(
            &root,
            serde_json::json!([{"resource_id": "w", "addresses": [ADDR]}]),
        );
        let adapter = adapter_with(&config, &root.join("corpus"), 0, &root.join("state"));

        let page_of = |page: serde_json::Value| -> serde_json::Value {
            let resource = match page {
                serde_json::Value::Null => serde_json::json!({"resource_id": "w"}),
                cursor => serde_json::json!({"resource_id": "w", "page": cursor}),
            };
            ok_of(
                adapter
                    .deliver(&request(
                        OP_HISTORY_READ,
                        serde_json::json!({"resources": [resource]}),
                    ))
                    .unwrap(),
            )
        };

        let first = page_of(serde_json::Value::Null);
        let next = first["statuses"][0]["page"]["next"].clone();
        assert!(
            !next.is_null(),
            "fixture too small: {COUNT} transactions fit in one 512 KiB page, so this \
             test would prove nothing about a second one"
        );
        assert_eq!(
            first["statuses"][0]["page"]["page_size_reduced_to"],
            first["observations"].as_array().unwrap().len(),
            "a page that cut early reports how many it emitted"
        );

        // The provider is now gone. Everything from here has to come out
        // of the snapshot taken at the first page.
        std::fs::remove_dir_all(root.join("corpus")).unwrap();

        let mut ids: Vec<String> = first["observations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["local_id"].as_str().unwrap().to_owned())
            .collect();
        let mut cursor = next;
        let mut pages = 1;
        while !cursor.is_null() {
            let reply = page_of(cursor.clone());
            assert_eq!(
                reply["statuses"][0]["outcome"]["fetched"]["page_empty"], false,
                "page {pages}: the snapshot should still be serving observations"
            );
            ids.extend(
                reply["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|o| o["local_id"].as_str().unwrap().to_owned()),
            );
            cursor = reply["statuses"][0]["page"]["next"].clone();
            pages += 1;
            assert!(pages < 20, "not draining");
        }
        assert!(pages > 1, "the crawl must have taken more than one page");

        let expected: Vec<String> = (0..COUNT).map(|n| format!("w:{n:064x}")).collect();
        assert_eq!(
            ids, expected,
            "every transaction, exactly once, in order, across the page boundary"
        );

        // A new `page: None` starts a NEW crawl -- which, with the corpus
        // deleted, cannot be taken. That is the proof the drained crawl's
        // snapshot was spent rather than kept as a cache.
        let fresh = page_of(serde_json::Value::Null);
        assert_eq!(fresh["observations"].as_array().unwrap().len(), 0);
        assert_eq!(
            fresh["statuses"][0]["outcome"], "unavailable",
            "a fresh crawl against a dead provider says so, rather than \
             silently re-serving the spent snapshot"
        );
        assert!(
            fresh["statuses"][0]["page"].is_null(),
            "a read that did not happen claims no resume point"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unconfigured_resource_is_not_fetched_never_gone() {
        let txid = "b".repeat(64);
        let root = fetch_fail_corpus(&txid);
        let adapter = adapter_over(&root, 0, &root.join("state2"));
        let reply = ok_of(
            adapter
                .deliver(&request(
                    OP_BALANCES_READ,
                    serde_json::json!({"resource_ids": ["nope"]}),
                ))
                .unwrap(),
        );
        assert_eq!(reply["statuses"][0]["outcome"], "not_fetched");
        assert_eq!(reply["statuses"][0]["resource_id"], "nope");
        assert_eq!(reply["observations"].as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_undeclared_op_is_an_err_that_does_not_end_the_connection() {
        let root = fetch_fail_corpus(&"c".repeat(64));
        let adapter = adapter_over(&root, 0, &root.join("state3"));
        let reply = adapter
            .deliver(&request("execute", serde_json::json!({})))
            .unwrap();
        match reply {
            Reply::Err { err, .. } => assert_eq!(err.code, WireErrorCode::Unsupported),
            Reply::Ok { .. } => panic!("an unknown op must not succeed"),
        }
        // ...and the very next request is answered normally.
        assert!(adapter
            .deliver(&request(OP_RESOURCES_LIST, serde_json::json!({})))
            .is_some());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_window_page_request_is_refused_without_naming_a_resource() {
        let root = fetch_fail_corpus(&"d".repeat(64));
        let adapter = adapter_over(&root, 0, &root.join("state4"));
        let reply = adapter
            .deliver(&request(
                OP_HISTORY_READ,
                serde_json::json!({"resources": [{"resource_id": "w", "page": {
                    "kind": "window",
                    "resource_id": "w",
                    "start": "2026-01-01T00:00:00Z",
                    "end": "2026-02-01T00:00:00Z"
                }}]}),
            ))
            .unwrap();
        match reply {
            Reply::Err { err, .. } => {
                assert_eq!(err.code, WireErrorCode::InvalidRequest);
                let detail = err.detail.unwrap();
                assert!(
                    detail.get("resource_id").is_none(),
                    "spec/wire.md 8: an err payload naming a resource wanted a status instead"
                );
            }
            Reply::Ok { .. } => panic!("this adapter serves cursor pages only"),
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    // -----------------------------------------------------------------
    // The frame limit: MAX_FRAME_BYTES is the whole REPLY's ceiling, and
    // an oversized frame is a fatal kill with no resync (spec/wire.md 2).
    // -----------------------------------------------------------------

    pub(crate) fn tmp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sumer-btc-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    pub(crate) fn write_at(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn frame_bytes(reply: &Reply<serde_json::Value>) -> usize {
        serde_json::to_vec(reply).unwrap().len()
    }

    pub(crate) fn wallets_json(root: &Path, wallets: serde_json::Value) -> PathBuf {
        let config = root.join("wallets.json");
        std::fs::write(&config, serde_json::json!({"wallets": wallets}).to_string()).unwrap();
        config
    }

    pub(crate) fn adapter_with(config: &Path, corpus: &Path, run: u64, state: &Path) -> Adapter {
        Adapter {
            wallets: wallet::load(config).unwrap(),
            source: Source::replay(corpus, run),
            store: seen::Store::new(Some(state.to_path_buf())),
            crawls: RefCell::new(HashMap::new()),
        }
    }

    /// TWO RESOURCES, ordinary synthetic transactions, one reply. A budget
    /// spent per resource is not a frame limit: the reply carries both.
    #[test]
    fn two_resources_share_one_frames_budget() {
        const COUNT: usize = 1_200;
        let root = tmp_root("frame-two-resources");
        let corpus = root.join("corpus/run0");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("now"), "1767225600").unwrap();
        paged_corpus(&corpus, "bc1qexample0", COUNT);
        paged_corpus(&corpus, "bc1qexample1", COUNT);
        let config = wallets_json(
            &root,
            serde_json::json!([
                {"resource_id": "w0", "addresses": ["bc1qexample0"]},
                {"resource_id": "w1", "addresses": ["bc1qexample1"]},
            ]),
        );
        let adapter = adapter_with(&config, &root.join("corpus"), 0, &root.join("state"));

        let reply = adapter
            .deliver(&request(
                OP_HISTORY_READ,
                serde_json::json!({"resources": [
                    {"resource_id": "w0"}, {"resource_id": "w1"}
                ]}),
            ))
            .unwrap();
        let bytes = frame_bytes(&reply);
        assert!(
            bytes <= sumer_wire::MAX_FRAME_BYTES,
            "{bytes} bytes on the wire, over MAX_FRAME_BYTES ({}): an oversized \
             frame is a fatal kill with no resync, which is the denial of service \
             the byte-cut exists to prevent",
            sumer_wire::MAX_FRAME_BYTES
        );

        // ...and a budget shared between two resources must SHARE it, not
        // starve one of them. Drain both and count.
        let mut ids: Vec<String> = Vec::new();
        let mut reply = reply;
        for round in 0..40 {
            let ok = ok_of(reply);
            assert_eq!(
                ok["statuses"].as_array().unwrap().len(),
                2,
                "every requested resource_id appears in statuses exactly once"
            );
            ids.extend(
                ok["observations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|o| o["local_id"].as_str().unwrap().to_owned()),
            );
            let resources: Vec<serde_json::Value> = ok["statuses"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|s| !s["page"]["next"].is_null())
                .map(|s| {
                    serde_json::json!({
                        "resource_id": s["resource_id"], "page": s["page"]["next"]
                    })
                })
                .collect();
            if resources.is_empty() {
                break;
            }
            assert!(round < 39, "not draining");
            reply = adapter
                .deliver(&request(
                    OP_HISTORY_READ,
                    serde_json::json!({"resources": resources}),
                ))
                .unwrap();
        }
        let mut expected: Vec<String> = (0..COUNT)
            .flat_map(|n| [format!("w0:{n:064x}"), format!("w1:{n:064x}")])
            .collect();
        expected.sort();
        let mut got = ids.clone();
        got.sort();
        assert_eq!(
            got, expected,
            "every transaction of BOTH wallets, exactly once: a shared budget \
             must page the surplus, never drop it"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A status entry is mandatory for EVERY requested resource and
    /// appears exactly once (`spec/observation.md` 6), so its cost is
    /// knowable before paging begins
    /// and must be reserved before a single observation is admitted.
    /// Charging each entry only when its turn came let the resources at
    /// the front of a batch spend bytes the resources behind them were
    /// always going to need: one real wallet followed by 6,100 unknown
    /// resources put 1,061,875 bytes on the wire, and an oversized frame
    /// is a fatal kill with no resync (`spec/wire.md` 2).
    #[test]
    fn every_status_entry_is_reserved_before_any_observation() {
        const UNKNOWN: usize = 6_100;
        let root = tmp_root("frame-status-reservation");
        let corpus = root.join("corpus/run0");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("now"), "1767225600").unwrap();
        paged_corpus(&corpus, ADDR, 1_200);
        let config = wallets_json(
            &root,
            serde_json::json!([{"resource_id": "w", "addresses": [ADDR]}]),
        );
        let adapter = adapter_with(&config, &root.join("corpus"), 0, &root.join("state"));

        // The real wallet FIRST: it is the one whose observations get to
        // spend the budget, and the 6,100 statuses behind it are the ones
        // that were charged too late to stop it.
        let mut resources = vec![serde_json::json!({"resource_id": "w"})];
        resources
            .extend((0..UNKNOWN).map(|n| serde_json::json!({"resource_id": format!("u{n:04}")})));
        let reply = adapter
            .deliver(&request(
                OP_HISTORY_READ,
                serde_json::json!({"resources": resources}),
            ))
            .unwrap();
        let bytes = frame_bytes(&reply);
        assert!(
            bytes <= sumer_wire::MAX_FRAME_BYTES,
            "{bytes} bytes on the wire, over MAX_FRAME_BYTES ({}): the mandatory \
             statuses were charged after the observations that had already spent \
             their bytes",
            sumer_wire::MAX_FRAME_BYTES
        );

        let ok = ok_of(reply);
        assert_eq!(
            ok["statuses"].as_array().unwrap().len(),
            UNKNOWN + 1,
            "every requested resource_id appears in statuses exactly once"
        );
        assert_eq!(
            ok["observations"].as_array().unwrap().len(),
            0,
            "the statuses alone exhaust the frame, so nothing is left to admit \
             an observation with"
        );
        // ...and the wallet is told where to resume rather than that it
        // drained: a page with no room is not an empty history.
        let wallet = &ok["statuses"][0];
        assert_eq!(wallet["resource_id"], "w");
        assert_eq!(wallet["outcome"]["fetched"]["page_empty"], true);
        assert!(
            !wallet["page"]["next"].is_null(),
            "a page that could not afford one observation must still name a \
             resume point: {wallet}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The ceiling itself, at the only place bytes reach stdout. A frame
    /// over `MAX_FRAME_BYTES` is a fatal kill with no resync, so it is
    /// never written -- the host gets an `err` on the same id instead.
    #[test]
    fn a_reply_too_large_for_a_frame_is_answered_with_an_err() {
        let huge: Reply<serde_json::Value> = Reply::ok(
            RequestId(7),
            serde_json::json!({"pad": "y".repeat(MAX_FRAME_BYTES)}),
        );
        let mut out: Vec<u8> = Vec::new();
        write_reply(&mut out, &huge).unwrap();
        assert!(
            out.len() <= MAX_FRAME_BYTES + 1,
            "{} bytes written, and the trailing LF is the only byte allowed \
             past the cap",
            out.len()
        );
        assert_eq!(out.last(), Some(&b'\n'));
        let back: serde_json::Value = serde_json::from_slice(&out[..out.len() - 1]).unwrap();
        assert_eq!(back["id"], 7, "the same id: this is an answer, not a drop");
        assert_eq!(back["err"]["code"], "internal");
        assert!(back.get("ok").is_none());
    }

    /// A provider error body is evidence and rides verbatim -- but verbatim
    /// is not unbounded. A 503 with a megabyte of HTML must not make the
    /// reply unrepresentable.
    #[test]
    fn a_provider_error_body_cannot_overflow_a_frame() {
        let root = tmp_root("frame-error-body");
        write_at(&root, "corpus/run0/now", "1767225600");
        write_at(
            &root,
            &format!("corpus/run0/address_{ADDR}.status"),
            &format!("503\n{}", "x".repeat(1_100_000)),
        );
        let config = wallets_json(
            &root,
            serde_json::json!([{"resource_id": "w", "addresses": [ADDR]}]),
        );
        let adapter = adapter_with(&config, &root.join("corpus"), 0, &root.join("state"));

        let reply = adapter
            .deliver(&request(
                OP_BALANCES_READ,
                serde_json::json!({"resource_ids": ["w"]}),
            ))
            .unwrap();
        let bytes = frame_bytes(&reply);
        assert!(
            bytes <= sumer_wire::MAX_FRAME_BYTES,
            "{bytes} bytes on the wire, over MAX_FRAME_BYTES ({})",
            sumer_wire::MAX_FRAME_BYTES
        );
        let ok = ok_of(reply);
        assert_eq!(ok["statuses"][0]["outcome"], "unavailable");
        let body = ok["statuses"][0]["provider_detail"]["raw"]["body"]
            .as_str()
            .unwrap();
        assert!(
            body.starts_with('x') && body.ends_with("[truncated: 1100000 bytes]"),
            "a capped body says so, rather than silently pretending it is verbatim: \
             {body:?}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `block_hash` is a provider scalar with no bounded length, and it
    /// rides in `provider_extra`. spec/observation.md 6 step 1 is exactly
    /// the mechanism for it.
    #[test]
    fn a_giant_block_hash_is_truncated_rather_than_fatal() {
        let root = tmp_root("frame-block-hash");
        let txid = "a".repeat(64);
        write_at(&root, "corpus/run0/now", "1767225600");
        write_at(
            &root,
            &format!("corpus/run0/address_{ADDR}_txs_chain.json"),
            &serde_json::json!([{
                "txid": txid,
                "fee": 100,
                "status": {
                    "confirmed": true, "block_height": 800_000,
                    "block_hash": "0".repeat(1_100_000), "block_time": 1_600_000_000i64
                },
                "vin": [{"prevout": {"scriptpubkey_address": "bc1qthem0000", "value": 1_000}}],
                "vout": [{"scriptpubkey_address": ADDR, "value": 900}],
            }])
            .to_string(),
        );
        write_at(
            &root,
            &format!("corpus/run0/address_{ADDR}_txs_mempool.json"),
            "[]",
        );
        let config = wallets_json(
            &root,
            serde_json::json!([{"resource_id": "w", "addresses": [ADDR]}]),
        );
        let adapter = adapter_with(&config, &root.join("corpus"), 0, &root.join("state"));

        let reply = adapter
            .deliver(&request(
                OP_HISTORY_READ,
                serde_json::json!({"resources": [{"resource_id": "w"}]}),
            ))
            .unwrap();
        let bytes = frame_bytes(&reply);
        assert!(
            bytes <= sumer_wire::MAX_FRAME_BYTES,
            "{bytes} bytes on the wire, over MAX_FRAME_BYTES ({})",
            sumer_wire::MAX_FRAME_BYTES
        );
        let ok = ok_of(reply);
        let obs = &ok["observations"][0];
        assert_eq!(obs["amount"]["amount"], "900", "the record survives");
        assert_eq!(obs["provider_extra"]["_truncated"], true);
        assert_eq!(obs["provenance"]["completeness"], "partial");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Balances read on day 1, history on day 2, balance fetch fails on
    /// day 3. The cached AMOUNTS are day 1's, so day 1 is the only honest
    /// `as_of`: a history read observed no balance and may not restamp one.
    #[test]
    fn history_never_restamps_the_balances_it_did_not_fetch() {
        let root = tmp_root("freshness-split");
        let txid = "e".repeat(64);
        let listing = serde_json::json!([{
            "txid": txid,
            "fee": 100,
            "status": {
                "confirmed": true, "block_height": 800_000,
                "block_hash": "0".repeat(64), "block_time": 1_600_000_000i64
            },
            "vin": [{"prevout": {"scriptpubkey_address": "bc1qthem0000", "value": 1_000}}],
            "vout": [{"scriptpubkey_address": ADDR, "value": 900}],
        }])
        .to_string();
        let stats = serde_json::json!({
            "address": ADDR,
            "chain_stats": {"funded_txo_sum": 900, "spent_txo_sum": 0, "tx_count": 1},
            "mempool_stats": {"funded_txo_sum": 0, "spent_txo_sum": 0, "tx_count": 0},
        })
        .to_string();

        // Day 1: balances answer.
        write_at(&root, "corpus/run0/now", "1767225600");
        write_at(&root, &format!("corpus/run0/address_{ADDR}.json"), &stats);
        // Day 2: history answers. Nothing here observes a balance.
        write_at(&root, "corpus/run1/now", "1767312000");
        write_at(
            &root,
            &format!("corpus/run1/address_{ADDR}_txs_chain.json"),
            &listing,
        );
        write_at(
            &root,
            &format!("corpus/run1/address_{ADDR}_txs_mempool.json"),
            "[]",
        );
        // Day 3: the balance fetch fails.
        write_at(&root, "corpus/run2/now", "1767398400");
        write_at(
            &root,
            &format!("corpus/run2/address_{ADDR}.status"),
            "503\nupstream is on fire",
        );

        let config = wallets_json(
            &root,
            serde_json::json!([{"resource_id": "w", "addresses": [ADDR]}]),
        );
        let corpus = root.join("corpus");
        let state = root.join("state");

        let day1 = ok_of(
            adapter_with(&config, &corpus, 0, &state)
                .deliver(&request(
                    OP_BALANCES_READ,
                    serde_json::json!({"resource_ids": ["w"]}),
                ))
                .unwrap(),
        );
        assert_eq!(day1["observations"][0]["amount"]["amount"], "900");

        let day2 = ok_of(
            adapter_with(&config, &corpus, 1, &state)
                .deliver(&request(
                    OP_HISTORY_READ,
                    serde_json::json!({"resources": [{"resource_id": "w"}]}),
                ))
                .unwrap(),
        );
        assert_eq!(day2["observations"].as_array().unwrap().len(), 1);

        let day3 = ok_of(
            adapter_with(&config, &corpus, 2, &state)
                .deliver(&request(
                    OP_BALANCES_READ,
                    serde_json::json!({"resource_ids": ["w"]}),
                ))
                .unwrap(),
        );
        assert_eq!(
            day3["observations"][0]["amount"]["amount"], "900",
            "day 1's amount, preserved"
        );
        assert_eq!(
            day3["statuses"][0]["outcome"]["stale"]["as_of"], "2026-01-01T00:00:00Z",
            "day 1's amount must carry day 1's timestamp: a history read observed \
             no balance, and restamping one is a false freshness claim about money"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// One `resource_id` twice has no conforming answer: one status entry
    /// drops a request the host made, two break "exactly once". So it is
    /// an envelope error -- and one that does NOT name the id, because
    /// `spec/wire.md` 8 makes that the signal a status outcome was right.
    #[test]
    fn a_repeated_resource_id_is_refused_without_naming_it() {
        let root = fetch_fail_corpus(&"f".repeat(64));
        let adapter = adapter_over(&root, 0, &root.join("state-repeat"));
        let batches = [
            (
                OP_BALANCES_READ,
                serde_json::json!({"resource_ids": ["w", "w"]}),
            ),
            (
                OP_HISTORY_READ,
                serde_json::json!({"resources": [{"resource_id": "w"}, {"resource_id": "w"}]}),
            ),
            (
                OP_STATUS_READ,
                serde_json::json!({"resource_ids": ["w", "w"]}),
            ),
        ];
        for (op, params) in batches {
            match adapter.deliver(&request(op, params)).unwrap() {
                Reply::Err { err, .. } => {
                    assert_eq!(err.code, WireErrorCode::InvalidRequest, "{op}");
                    assert!(
                        err.detail.is_none(),
                        "{op}: an err payload naming a resource wanted a status instead"
                    );
                }
                Reply::Ok { ok, .. } => {
                    panic!("{op}: two statuses for one requested id break \"exactly once\": {ok}")
                }
            }
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A provider identity rides in every observation this adapter emits.
    /// Over the bound it is REFUSED, never truncated: a truncated
    /// provenance is a false one, and exit 2 is the honest answer.
    #[test]
    fn an_oversized_source_is_a_usage_error_not_a_truncation() {
        let long = format!("https://{}.invalid/api", "p".repeat(MAX_SOURCE_BYTES));
        let args = ["--wallets", "w.json", "--source", &long];
        let Err(e) = Options::parse(args.iter().map(|a| (*a).to_owned())) else {
            panic!("a {}-byte --source must not be accepted", long.len());
        };
        assert!(e.contains("--source is"), "{e}");
        assert!(
            Options::parse(
                [
                    "--wallets",
                    "w.json",
                    "--source",
                    &"x".repeat(MAX_SOURCE_BYTES)
                ]
                .iter()
                .map(|a| (*a).to_owned())
            )
            .is_ok(),
            "the bound itself is legal"
        );
    }

    #[test]
    fn a_status_entry_never_carries_a_credential_clock() {
        // Watch-only: there is no credential and no SCA session, so these
        // two fields are absent by construction, not by remembering.
        let s = status("w", ReadOutcome::Unavailable);
        let json = serde_json::to_value(&s).unwrap();
        assert!(json.get("credential_expires_at").is_none());
        assert!(json.get("strong_auth_expires_at").is_none());
    }
}
