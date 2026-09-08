//! The sweep: one `history.read` for one resource, from `page: None` to
//! `next: null`, and the retraction it does or does not earn.
//!
//! # The nine-condition gate (`spec/observation.md` §8.1)
//!
//! A **sweep** is one `history.read` for one resource that
//!
//! 1. began `page: None`,
//! 2. ran on ONE adapter process and ONE crawl,
//! 3. drained to `next: null`,
//! 4. reported `fetched` on EVERY page,
//! 5. carried no **anonymous** `degraded` -- a `degraded` naming a
//!    `local_id` EXEMPTS that id and does not disqualify,
//! 6. had `cursor_resumable: exact` on every page,
//! 7. ran on ONE connection whose HELLO `local_id_derivation` is recorded
//!    on the crawl row,
//! 8. carried no observation whose `provenance.adapter_id` was some other
//!    adapter's -- the one disqualifier that also DROPS the offending
//!    record, because there is nowhere honest to store it,
//! 9. ran on a connection that broke no wire contract anywhere in this
//!    refresh -- including on the reads that are not this sweep's.
//!
//! Fail any one and the crawl is PARTIAL: its observations still persist
//! (they are evidence, and evidence is never thrown away), and **nothing
//! is retracted**.
//!
//! ## Condition (9) is refresh-scoped, and deliberately
//!
//! Conditions (2) and (7) are already about the CONNECTION rather than
//! about the pages, so a connection-scoped condition is not a foreign body
//! in this list. (9) widens the window to the whole refresh: a
//! `balances.read` that answered for a resource nobody asked about broke
//! the contract before the first page of any sweep was requested, and an
//! adapter that answers questions nobody asked has demonstrated it is not
//! answering the protocol. Absence is evidence only when the host is
//! confident it looked properly, and this one is not. The conservative
//! direction costs a stale row; the permissive one destroys a record.
//!
//! The taint is FORWARD-ONLY: it disqualifies every sweep that starts after
//! the violation, not the ones already committed. The adapter-wide reads
//! (`hello`, `resources.list`, `status.read`, `balances.read`) all precede
//! every sweep, so a violation there taints all of them; a violation inside
//! one resource's own page loop taints the siblings swept after it. Nothing
//! reaches backwards, because nothing has to: a sweep that already
//! committed decided on the evidence it had, and a wrongly retracted record
//! revives on the next complete sweep that carries it (§8.4).
//!
//! ## Condition (5) is an exemption, not a veto
//!
//! A `degraded` naming a `local_id` says "this one record was too big to
//! send". Treating that as a disqualifier would black out retraction for
//! that resource for as long as the record stays oversized -- and record
//! size is provider-influenced, so anyone who can push bytes into a record
//! could disable retraction for a wallet permanently. An **anonymous**
//! degrade is different: the host does not know which record went missing,
//! so it cannot tell "absent because dropped for size" from "absent
//! because gone", and it must not guess.
//!
//! ## Condition (7) gates on the PER-SWEEP HELLO VALUE ONLY
//!
//! Not on the stored observations' derivations, and not on the `adapter`
//! row. Both were tried on paper and both are catastrophic:
//!
//! * **Against stored history**: after any derivation bump the chain holds
//!   both old- and new-derivation records, so the comparison fails for
//!   ever and no sweep of that resource ever qualifies again. Retraction
//!   would deadlock permanently, silently.
//! * **Against the `adapter` row**: the first sweep after an upgrade sees
//!   "the adapter's derivation moved", disqualifies, and (in the reading
//!   where it instead *retracts*) buries the user's entire pre-upgrade
//!   history under `absent_from_complete_sweep` -- a reason blaming the
//!   provider for a software change.
//!
//! What (7) actually asks is narrow and answerable: did one connection
//! serve this whole sweep, and did the host write down which derivation
//! that connection declared? The derivation *change* is not a gate at all
//! -- it is a REASON, applied below.

use std::collections::{HashMap, HashSet};

use sumer_host::fold::Fold;
use sumer_host::paging::ResumeState;
use sumer_host::time;
use sumer_host::AdapterHandle;
use sumer_host::HostError;
use sumer_wire::{
    CursorResumable, Observation, PageRequest, ReadOutcome, ResourceQuery, ResourceStatus, Rfc3339,
    WireErrorCode,
};

use crate::error::Result;
use crate::hash::content_hash;
use crate::store::{self, Append, Store};

/// A hard stop on the page loop. An adapter that keeps handing back a
/// cursor it never advances would otherwise sweep for ever.
///
/// ponytail: a constant, not a knob. 100k pages of 512 KiB is ~50 GiB of
/// one resource's history; make it configurable the day a real provider
/// needs more.
const MAX_PAGES: usize = 100_000;

/// Retraction reasons, in the order the contract chooses between them.
pub const REASON_DERIVATION: &str = "derivation_changed";
pub const REASON_DEFINITION: &str = "resource_definition_changed";
pub const REASON_ABSENT: &str = "absent_from_complete_sweep";

/// Discrepancy kinds. Every one of these is read by `sumer status` and
/// printed by `refresh`.
pub const KIND_DERIVATION: &str = "derivation_changed";
pub const KIND_DEFINITION: &str = "resource_definition_changed";
pub const KIND_VANTAGE: &str = "vantage_changed";
pub const KIND_EMPTY: &str = "empty_sweep";
pub const KIND_HISTORY_START: &str = "history_start_exempt";

#[derive(Debug, Clone, Copy, Default)]
pub struct SweepOptions {
    /// Continue a crawl a crash cut short, from its persisted cursor. Such
    /// a sweep fails condition (1) by construction and therefore retracts
    /// nothing -- which is the point: a resumed read has not seen the
    /// history below its cursor, so it cannot testify to an absence there.
    pub resume: bool,
    /// The ONLY way to retract 100% of a resource. 100% is the one
    /// constant that needs no justification.
    pub confirm_empty: bool,
}

#[derive(Debug, Clone)]
pub struct Discrepancy {
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct SweepReport {
    pub adapter_id: String,
    pub resource_id: String,
    pub crawl_id: Option<i64>,
    pub new: usize,
    pub revised: usize,
    pub unchanged: usize,
    pub retracted: usize,
    pub complete: bool,
    pub disqualified_reason: Option<String>,
    pub discrepancies: Vec<Discrepancy>,
    /// Set when the read itself failed. The resource is reported failed
    /// (`refresh` exits 1) and nothing is retracted.
    pub error: Option<String>,
    /// Set when this sweep's own `history.read` broke the wire contract,
    /// for the caller to feed into condition (9) of the sweeps that follow
    /// it on the same connection. This sweep is already disqualified; its
    /// siblings are not, and the violation says as much about them as a
    /// `balances.read` violation says about all of them.
    pub contract_violation: Option<String>,
}

impl SweepReport {
    fn empty(adapter_id: &str, resource_id: &str) -> SweepReport {
        SweepReport {
            adapter_id: adapter_id.to_owned(),
            resource_id: resource_id.to_owned(),
            crawl_id: None,
            new: 0,
            revised: 0,
            unchanged: 0,
            retracted: 0,
            complete: false,
            disqualified_reason: None,
            discrepancies: Vec::new(),
            error: None,
            contract_violation: None,
        }
    }
}

/// Did this failure mean the connection **broke the wire contract**, as
/// opposed to failing honestly? Condition (9)'s discriminator.
///
/// An adapter that answers `err` in the contract's own closed vocabulary,
/// that times out, or that dies, has failed -- it has not lied. None of
/// those says anything about the history the connection goes on to serve,
/// and a host that treated them as taint would switch off retraction for a
/// resource on any rate-limited read. Two things do taint:
///
/// * `invalid_request`, which is how the host spells "this reply is not the
///   shape the contract requires" -- a balance for a resource nobody asked
///   about, a `statuses` array that does not cover its request. It also
///   covers an adapter genuinely returning `err: invalid_request` to a
///   request the host formed itself: the host knows its own request was
///   well formed, so the adapter is either wrong about the protocol or
///   broken, and either way is not answering it.
/// * a fatal [`ProtocolViolation`](HostError::ProtocolViolation), which has
///   already killed the process. Every sweep after it fails condition (2)
///   anyway; naming it here costs nothing and says why.
pub(crate) fn broke_the_contract(error: &HostError) -> bool {
    match error {
        HostError::Wire(err) => err.code == WireErrorCode::InvalidRequest,
        HostError::ProtocolViolation(_) => true,
        HostError::Timeout
        | HostError::AdapterCrashed { .. }
        | HostError::Spawn(_)
        | HostError::IdsExhausted => false,
    }
}

/// What the page loop observed, judged once at the end.
#[derive(Debug)]
struct Gate {
    began_at_start_of_history: bool,
    drained: bool,
    every_page_fetched: bool,
    anonymous_degrade: bool,
    every_cursor_exact: bool,
    hello_derivation: String,
    /// Condition (8). An observation whose `provenance.adapter_id` was not
    /// this connection's: the fold keys chains by that field and the store
    /// keys rows by the connected adapter, so a mismatch means the host is
    /// holding two different notions of the same chain. A host cannot
    /// conclude an absence from a set it cannot key.
    ///
    /// Unlike the other seven, this one also DROPS the offending record --
    /// see where it is set. Every other disqualifier still persists its
    /// page, because a partial-but-honest sweep is evidence; a record that
    /// denies its own origin is the one thing there is nowhere honest to
    /// put.
    foreign_provenance: bool,
    /// Which outcome, on which page, broke condition (4). Kept so the
    /// user is told `rate_limited on page 3` rather than the name of a
    /// condition they never read.
    not_fetched: Option<String>,
    /// Condition (9), and the only input to this gate that the page loop
    /// did not observe: a wire-contract violation this connection
    /// committed EARLIER IN THIS REFRESH, on a read that is not this
    /// sweep's. Carries the reason so the crawl row says which violation,
    /// not merely that there was one.
    connection_violation: Option<String>,
}

impl Gate {
    /// `None` if all nine hold. Otherwise the first condition that failed
    /// -- written to `crawl.disqualified_reason`, which is the audit of why
    /// retraction did or did not happen.
    fn disqualifier(&self) -> Option<String> {
        // (9) first: it is the one fact that was already true before this
        // sweep sent a single request, and it says the connection is not
        // answering the protocol -- which is the strongest reason on the
        // list to distrust everything else it went on to say.
        if let Some(violation) = &self.connection_violation {
            return Some(violation.clone());
        }
        if !self.began_at_start_of_history {
            return Some("did not begin at the start of available history".to_owned());
        }
        if !self.drained {
            return Some("did not drain to next: null".to_owned());
        }
        if !self.every_page_fetched {
            return Some(
                self.not_fetched
                    .clone()
                    .unwrap_or_else(|| "a page did not report fetched".to_owned()),
            );
        }
        if self.anonymous_degrade {
            return Some("an anonymous degraded record".to_owned());
        }
        if !self.every_cursor_exact {
            return Some("a page cursor was not exact-resumable".to_owned());
        }
        if self.hello_derivation.is_empty() {
            return Some("no hello local_id_derivation on the crawl".to_owned());
        }
        if self.foreign_provenance {
            return Some("an observation's provenance named another adapter".to_owned());
        }
        None
    }
}

/// Everything about this resource that is fixed before the first page.
pub struct SweepInput<'a> {
    /// The adapter this sweep's rows are keyed by, **and** the identity
    /// condition (8) judges provenance against. Those are the same string
    /// because `refresh_adapter` refuses to run at all when the stored
    /// record and the connection's HELLO disagree -- see the check there.
    /// Judging (8) against the id the host REMEMBERED, while the
    /// connection announced another, is precisely the two-notions-of-one-
    /// chain confusion (8) exists to catch.
    pub adapter_id: &'a str,
    pub resource_id: &'a str,
    /// The hello `local_id_derivation` of the ONE connection serving this
    /// sweep. Condition (7)'s only input.
    pub hello_derivation: &'a str,
    /// SHA-256 over the resource DEFINITION as this run's
    /// `resources.list` describes it.
    pub fingerprint: &'a str,
    /// `ResourceDescriptor.provider_id`: the vantage this sweep reads
    /// from. Available even when the sweep returns zero observations,
    /// which is why it comes from the descriptor rather than from
    /// provenance.
    pub provider_id: &'a str,
    /// From `status.read`, when the adapter reported one.
    pub history_start: Option<Rfc3339>,
    /// Condition (9): `Some(reason)` when this connection has already
    /// broken the wire contract during this refresh. Every sweep on that
    /// connection is disqualified, including the ones whose own pages are
    /// flawless -- see the module docs.
    pub connection_violation: Option<&'a str>,
    pub options: SweepOptions,
}

/// Runs one resource's sweep end to end.
///
/// The adapter handle is a parameter rather than something this function
/// spawns: the caller decides whether the connection records its
/// transcript ([`AdapterHandle::spawn_recorded`]), which is how the resume
/// test asserts that a `--resume` really put the stored cursor on the wire
/// instead of merely claiming to.
///
/// ponytail: the SQLite calls block the async executor for the length of
/// one transaction. A single-user CLI with one adapter in flight does not
/// notice; move them to `spawn_blocking` if a host ever sweeps many
/// adapters concurrently.
pub async fn sweep_resource(
    store: &mut Store,
    adapter: &AdapterHandle,
    input: &SweepInput<'_>,
) -> Result<SweepReport> {
    let mut report = SweepReport::empty(input.adapter_id, input.resource_id);
    let now = crate::now_rfc3339();

    // --- where this sweep starts -------------------------------------
    //
    // `refresh` ALWAYS sweeps from `page: None`. The persisted cursor
    // exists for exactly one caller -- `--resume`, after a crash -- and a
    // resumed sweep is partial by conditions (1) and (2), so it never
    // retracts. It costs nothing today: the Bitcoin adapter re-walks the
    // whole history on any crawl.
    let resumed = if input.options.resume {
        resume_point(store, input)?
    } else {
        None
    };
    let start_page: Option<PageRequest> = match &resumed {
        Some(text) => Some(serde_json::from_str(text)?),
        None => None,
    };

    let crawl_id = store::open_crawl(
        store.conn(),
        input.adapter_id,
        input.resource_id,
        now.as_str(),
        resumed.as_deref(),
        input.hello_derivation,
        Some(input.fingerprint),
    )?;
    report.crawl_id = Some(crawl_id);

    let mut gate = Gate {
        began_at_start_of_history: start_page.is_none(),
        drained: false,
        every_page_fetched: true,
        anonymous_degrade: false,
        every_cursor_exact: true,
        hello_derivation: input.hello_derivation.to_owned(),
        not_fetched: None,
        foreign_provenance: false,
        connection_violation: input.connection_violation.map(str::to_owned),
    };

    // --- the fold ----------------------------------------------------
    //
    // ONE fold per sweep: this ADAPTER's stored observations replayed in
    // stored order, then this sweep's ingested on top. Revision assignment
    // has exactly one implementation (`sumer_host::fold`) and this is its
    // caller.
    //
    // Adapter-wide, not per-resource: `revision` is assigned per
    // `(adapter_id, local_id)` (§3) and the schema enforces
    // `UNIQUE (adapter_id, local_id, revision)`, so a per-resource fold
    // splits a chain the moment one `local_id` is reported under a second
    // resource -- see `store::observations_for_adapter`.
    //
    // ponytail: O(this adapter's history) in memory per refresh. Page it
    // the day one wallet outgrows RAM; today the whole point is that the
    // diff base and the wire path agree, and two implementations of the
    // fold would not.
    let mut fold = Fold::new();
    let mut stored_meta: HashMap<(String, u64), HeadMeta> = HashMap::new();
    for stored in store::observations_for_adapter(store.conn(), input.adapter_id)? {
        stored_meta.insert(
            (stored.observation.local_id.clone(), stored.revision),
            HeadMeta {
                derivation: stored.derivation,
                fingerprint: stored.fingerprint,
            },
        );
        fold.ingest(stored.observation);
    }
    // Which software rules produced each chain's head -- the row THE FOLD
    // calls the head, not the last one inserted. The two differ when two
    // observations for one key arrive in one page out of surface order,
    // and picking the wrong one puts a stale fingerprint on the head, which
    // §8.3 then reads as a resource-definition change that never happened.
    let mut head_meta: HashMap<String, HeadMeta> = HashMap::new();
    let keys: Vec<(String, String)> = fold
        .keys()
        .map(|(adapter_id, local_id)| (adapter_id.to_owned(), local_id.to_owned()))
        .collect();
    for (adapter_id, local_id) in keys {
        if adapter_id != input.adapter_id {
            continue;
        }
        let Some(revision) = fold
            .chain(&adapter_id, &local_id)
            .last()
            .map(|h| h.revision)
        else {
            continue;
        };
        if let Some(meta) = stored_meta.remove(&(local_id.clone(), revision)) {
            head_meta.insert(local_id, meta);
        }
    }

    // Which chains are currently buried by a retraction, as of the start
    // of this sweep. Read once: retractions are only written at the end of
    // a sweep, so this cannot move underneath the page loop.
    let buried = store::retraction_high_water(store.conn(), input.adapter_id)?;

    let mut resume_state = ResumeState::new(start_page.clone());
    let mut page = start_page;
    let mut observed_ids: HashSet<String> = HashSet::new();
    let mut exempt_ids: HashSet<String> = HashSet::new();

    for page_number in 0..MAX_PAGES {
        let query = ResourceQuery {
            resource_id: input.resource_id.to_owned(),
            page: page.clone(),
        };
        let read = match adapter.history_read(vec![query]).await {
            Ok(read) => read,
            Err(e) => {
                // Condition (2): this sweep no longer has one healthy
                // adapter process behind it. The crawl stays OPEN and
                // undrained, everything already committed stays, and
                // nothing is retracted.
                let reason = format!("adapter_failed_on_page_{page_number}: {e}");
                store::finish_crawl(store.conn(), crawl_id, false, false, Some(&reason))?;
                // Condition (9) for this sweep's SIBLINGS: a page that broke
                // the contract taints every sweep the same refresh starts
                // after it, exactly as a `balances.read` violation does.
                if broke_the_contract(&e) {
                    report.contract_violation =
                        Some(format!("history.read broke the wire contract: {e}"));
                }
                report.disqualified_reason = Some(reason.clone());
                report.error = Some(reason);
                return Ok(report);
            }
        };

        let status = read
            .statuses
            .iter()
            .find(|s| s.resource_id == input.resource_id);
        // `AdapterHandle::history_read` already refuses a reply that does
        // not cover every requested resource exactly once, so this is
        // unreachable over a real connection. It stays because the
        // alternative is an `expect`, and nothing in this crate panics on
        // an adapter reply.
        let Some(status) = status else {
            let reason = format!("page_{page_number}_reported_no_status_for_this_resource");
            store::finish_crawl(store.conn(), crawl_id, false, false, Some(&reason))?;
            report.disqualified_reason = Some(reason.clone());
            report.error = Some(reason);
            return Ok(report);
        };

        judge_page(&mut gate, status, page_number, &mut exempt_ids);

        // Condition (8): an observation whose `provenance.adapter_id` is
        // not this connection's is REFUSED, not stored.
        //
        // It cannot be stored under this adapter's key -- that would
        // record, as fact, a provenance the record itself denies -- and it
        // must not be stored under the adapter it names, which would let
        // one adapter append to another's history (`spec/wire.md` §10 is
        // the reason ids are per-adapter at all). There is no third place
        // to put it, so it is dropped, the sweep is disqualified, and the
        // reason says which condition failed. It is also kept out of
        // `observed_ids`: a record the host refused must never count as
        // "this adapter reported that id" and suppress a legitimate
        // retraction.
        //
        // Provenance is judged BEFORE the resource filter. Condition (8)
        // is "EVERY observation's `provenance.adapter_id` was the
        // connection's own" -- every observation on the page, not just the
        // ones addressed to the resource being swept. Filtering first let a
        // page carry its own contradiction past the gate: the honest
        // records for this resource completed the sweep while an
        // observation naming another adapter under some other resource was
        // dropped unexamined, and the retraction went through on a page
        // that had already proved the host was holding two notions of one
        // chain.
        let mut observations: Vec<Observation> = Vec::new();
        for observation in read.observations {
            if observation.provenance.adapter_id != input.adapter_id {
                gate.foreign_provenance = true;
                continue;
            }
            if observation.resource_id != input.resource_id {
                continue;
            }
            observed_ids.insert(observation.local_id.clone());
            observations.push(observation);
        }

        let next = match &status.page {
            Some(page_reply) => {
                resume_state.record(page_reply.cursor_resumable, page_reply.next.clone());
                page_reply.next.clone()
            }
            // A read that did not happen claims no resume point. That is
            // not a drained sweep, and it must not be mistaken for one.
            None => None,
        };
        let final_page = next.is_none();
        gate.drained = final_page && status.page.is_some();

        if final_page {
            // ---- THE ONE TRANSACTION --------------------------------
            //
            // Last-page inserts, `last_seen_crawl` stamps, the cursor
            // write, THE RETRACTION DERIVATION, the adapter/resource
            // metadata updates and the crawl's open -> drained transition
            // all commit together. Split any of them and a crash can leave
            // a crawl recorded DRAINED whose retractions were never
            // derived -- and the next resume sees a finished sweep and
            // never derives them. One transaction makes that
            // unrepresentable.
            let txn = store.transaction()?;
            let counts = ingest_page(
                &txn,
                input,
                crawl_id,
                &mut fold,
                &mut head_meta,
                &buried,
                &observations,
            )?;
            report.new += counts.new;
            report.revised += counts.revised;
            report.unchanged += counts.unchanged;

            let disqualifier = gate.disqualifier();
            if let Some(reason) = &disqualifier {
                report.disqualified_reason = Some(reason.clone());
            } else {
                report.complete = true;
                let outcome = derive_retractions(
                    &txn,
                    input,
                    crawl_id,
                    &fold,
                    &head_meta,
                    &observed_ids,
                    &exempt_ids,
                    now.as_str(),
                )?;
                report.retracted = outcome.retracted;
                report.discrepancies = outcome.discrepancies;
            }

            store::set_adapter_derivation(&txn, input.adapter_id, input.hello_derivation)?;
            // The VANTAGE is adopted only by a sweep that RECONCILED it.
            // `derive_retractions` is the only writer of `vantage_changed`
            // and it never runs on a disqualified sweep, so storing the
            // new `provider_id` here unconditionally destroyed both halves
            // of §8.2's exemption at once: the audit row was never written
            // and the next sweep, comparing the new vantage against
            // itself, retracted everything the old vantage could see under
            // a reason that blames the provider. One non-`exact` page was
            // enough.
            //
            // The derivation and the fingerprint are NOT gated with it:
            // neither exempts anything (condition (7) reads the per-sweep
            // hello value, §8.3 reads the per-record fingerprint that
            // `ingest_page` has already stamped on this sweep's rows), so
            // they are metadata that must stay in step with those rows.
            store::set_resource_definition(
                &txn,
                input.adapter_id,
                input.resource_id,
                input.fingerprint,
                report.complete.then_some(input.provider_id),
            )?;
            store::finish_crawl(
                &txn,
                crawl_id,
                gate.drained,
                report.complete,
                disqualifier.as_deref(),
            )?;
            fault_before_final_commit()?;
            txn.commit()?;
            return Ok(report);
        }

        // A page that is not the last commits on its own: a crash between
        // pages must leave page 1 durable and the cursor pointing at its
        // `next`, so `--resume` picks up exactly there.
        let txn = store.transaction()?;
        let counts = ingest_page(
            &txn,
            input,
            crawl_id,
            &mut fold,
            &mut head_meta,
            &buried,
            &observations,
        )?;
        report.new += counts.new;
        report.revised += counts.revised;
        report.unchanged += counts.unchanged;
        let cursor = resume_state
            .next_request()
            .map(|r| serde_json::to_string(&r))
            .transpose()?;
        store::set_crawl_cursor(&txn, crawl_id, cursor.as_deref())?;
        txn.commit()?;

        page = next;
    }

    let reason = format!("page_limit_{MAX_PAGES}_reached_without_draining");
    store::finish_crawl(store.conn(), crawl_id, false, false, Some(&reason))?;
    report.disqualified_reason = Some(reason.clone());
    report.error = Some(reason);
    Ok(report)
}

// ---------------------------------------------------------------------
// The final-page fault point (test builds only)
// ---------------------------------------------------------------------

/// A crash simulated at the one moment no external observer can reach.
///
/// The real event -- SIGKILL after the last page's reply has been received
/// and before its transaction commits -- has no wire event separating
/// "received" from "committed", so `tests/restart.rs` can only assert the
/// postcondition over a window it cannot interlock. That is enough to say
/// the disk is never left torn; it is NOT enough to prove the five effects
/// of the final page are ONE transaction, because an implementation that
/// committed the inserts first and derived retractions second would satisfy
/// it just as well most of the time.
///
/// "One transaction makes that unrepresentable" is the load-bearing claim
/// of this whole design, so it gets an assertion that can actually fail.
/// This function aborts the final transaction in its last instant, and
/// `tests::the_final_page_is_one_transaction` asserts that NOTHING of that
/// page survived -- which is false the moment the transaction is split.
///
/// In a shipped binary this is `Ok(())` with no state behind it: the flag
/// and the branch exist only under `cfg(test)`, so there is nothing to set
/// at runtime and no knob to misconfigure.
#[cfg(not(test))]
fn fault_before_final_commit() -> Result<()> {
    Ok(())
}

#[cfg(test)]
static ABORT_BEFORE_FINAL_COMMIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn fault_before_final_commit() -> Result<()> {
    if ABORT_BEFORE_FINAL_COMMIT.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(crate::error::StoreError::Host(
            "simulated crash before the final page's commit".to_owned(),
        ));
    }
    Ok(())
}

/// Where `--resume` picks up, or `None` if there is nothing safe to resume
/// from.
///
/// The cursor is **dropped** when the derivation or the resource
/// definition has moved since the crawl that wrote it: a cursor is an
/// opaque token whose meaning belongs to the address set and the id
/// derivation that produced it, and resuming one across either change
/// would resume into a history that is no longer the same history.
fn resume_point(store: &Store, input: &SweepInput<'_>) -> Result<Option<String>> {
    let Some(crawl) = store::resumable_crawl(store.conn(), input.adapter_id, input.resource_id)?
    else {
        return Ok(None);
    };
    if crawl.local_id_derivation != input.hello_derivation {
        return Ok(None);
    }
    if crawl.fingerprint.as_deref() != Some(input.fingerprint) {
        return Ok(None);
    }
    Ok(crawl.next_page)
}

/// Folds one page's status into the gate.
fn judge_page(
    gate: &mut Gate,
    status: &ResourceStatus,
    page_number: usize,
    exempt_ids: &mut HashSet<String>,
) {
    if !matches!(status.outcome, ReadOutcome::Fetched { .. }) {
        gate.every_page_fetched = false;
        gate.not_fetched.get_or_insert_with(|| {
            format!(
                "{} on page {}",
                outcome_name(&status.outcome),
                page_number + 1
            )
        });
    }
    // EVERY entry, not the first or the last. A page can drop more than
    // one record, and one anonymous degrade among them disqualifies the
    // whole sweep no matter how many named ones sit beside it: an
    // exemption is a claim about one id, a disqualifier is a claim about
    // the page, and the page-level claim wins.
    for degraded in &status.degraded {
        match &degraded.local_id {
            // Named: this ONE record is exempt from retraction. The sweep
            // still qualifies -- see the module docs on why the veto
            // reading is third-party-triggerable.
            Some(local_id) => {
                exempt_ids.insert(local_id.clone());
            }
            None => gate.anonymous_degrade = true,
        }
    }
    if let Some(page) = &status.page {
        if page.cursor_resumable != CursorResumable::Exact {
            gate.every_cursor_exact = false;
        }
    }
}

/// The wire name of an outcome, for the one line the user reads.
fn outcome_name(outcome: &ReadOutcome) -> &'static str {
    match outcome {
        ReadOutcome::Fetched { .. } => "fetched",
        ReadOutcome::NotFetched => "not_fetched",
        ReadOutcome::Stale { .. } => "stale",
        ReadOutcome::RateLimited { .. } => "rate_limited",
        ReadOutcome::Unavailable => "unavailable",
        ReadOutcome::ReauthRequired => "reauth_required",
        ReadOutcome::Revoked => "revoked",
        ReadOutcome::Gone => "gone",
        ReadOutcome::ScaRequired => "sca_required",
    }
}

#[derive(Debug, Clone)]
struct HeadMeta {
    derivation: String,
    fingerprint: Option<String>,
}

#[derive(Debug, Default)]
struct PageCounts {
    new: usize,
    revised: usize,
    unchanged: usize,
}

/// Appends (or dedups) one page's observations.
///
/// **Dedup is by `content_hash` against the chain head.** Equal means the
/// provider is telling us the same thing it told us last time: nothing is
/// appended and `last_seen_crawl` is stamped. Without it a 5000-transaction
/// wallet would grow 5000 rows on every refresh, because `received_at`
/// alone differs -- which is exactly why `received_at` is not in the hash.
///
/// **A retracted record is the one exception, and it is not a special
/// case in the liveness rule -- it is the rule.** Liveness is "the chain's
/// highest revision exceeds every retraction for that key", so a record
/// the host retracted and the provider is now reporting again has to grow a
/// revision to come back. A reorg that re-mines a transaction re-emits it
/// BYTE-IDENTICALLY, so a dedup that fired here would leave it retracted
/// for ever, no matter how many honest sweeps carried it. "Live again" is
/// a changed fact even when the content is not.
fn ingest_page(
    conn: &rusqlite::Connection,
    input: &SweepInput<'_>,
    crawl_id: i64,
    fold: &mut Fold,
    head_meta: &mut HashMap<String, HeadMeta>,
    buried: &HashMap<String, u64>,
    observations: &[Observation],
) -> Result<PageCounts> {
    let mut counts = PageCounts::default();
    for observation in observations {
        let hash = content_hash(observation);
        let head = fold.chain(input.adapter_id, &observation.local_id);
        let head = head.last();
        let head_hash = head.map(|head| content_hash(&head.observation));
        // Liveness is asked of the chain's HIGHEST revision, never the
        // head's -- the two differ whenever the fold's total order and
        // arrival order disagree, and `live_set` asks the same question
        // the same way. Ask the head here and a backwards clock step makes
        // this say "buried" while `live_set` says "live": dedup stays off
        // for ever and the chain grows a row on every refresh.
        let currently_retracted = fold
            .highest_revision(input.adapter_id, &observation.local_id)
            .is_some_and(|revision| is_buried(buried, &observation.local_id, revision));
        if !currently_retracted && head_hash.as_deref() == Some(hash.as_str()) {
            // The head's OWN revision names the row to stamp. "The last row
            // inserted" is a different row whenever the fold's total order
            // and arrival order disagree.
            let revision = head.map_or(1, |head| head.revision);
            store::stamp_seen(
                conn,
                input.adapter_id,
                &observation.local_id,
                revision,
                crawl_id,
                input.hello_derivation,
                Some(input.fingerprint),
            )?;
            head_meta.insert(
                observation.local_id.clone(),
                HeadMeta {
                    derivation: input.hello_derivation.to_owned(),
                    fingerprint: Some(input.fingerprint.to_owned()),
                },
            );
            counts.unchanged += 1;
            continue;
        }
        let revision = fold.ingest(observation.clone());
        store::append_observation(
            conn,
            &Append {
                adapter_id: input.adapter_id,
                revision,
                crawl_id,
                derivation: input.hello_derivation,
                fingerprint: Some(input.fingerprint),
                content_hash: &hash,
                observation,
            },
        )?;
        head_meta.insert(
            observation.local_id.clone(),
            HeadMeta {
                derivation: input.hello_derivation.to_owned(),
                fingerprint: Some(input.fingerprint.to_owned()),
            },
        );
        if revision == 1 {
            counts.new += 1;
        } else {
            counts.revised += 1;
        }
    }
    Ok(counts)
}

struct RetractionOutcome {
    retracted: usize,
    discrepancies: Vec<Discrepancy>,
}

/// The retraction derivation, run inside the final page's transaction.
///
/// A record is LIVE iff its chain head is `active` **and** the chain's
/// highest revision exceeds every retraction revision for its key.
/// Revival needs no special case anywhere: a later sweep appends revision
/// N+1, which is greater than N, and the record is live again.
#[allow(clippy::too_many_arguments)]
fn derive_retractions(
    conn: &rusqlite::Connection,
    input: &SweepInput<'_>,
    crawl_id: i64,
    fold: &Fold,
    head_meta: &HashMap<String, HeadMeta>,
    observed_ids: &HashSet<String>,
    exempt_ids: &HashSet<String>,
    now: &str,
) -> Result<RetractionOutcome> {
    let mut discrepancies = Vec::new();
    let high_water = store::retraction_high_water(conn, input.adapter_id)?;

    // The diff base: the same [`live_set`] the user's `history` listing is
    // rendered from, called rather than restated.
    let live = live_set(fold, &high_water, input.adapter_id, input.resource_id);

    // --- EXEMPTION: vantage ------------------------------------------
    //
    // A different vantage is a different view of the same history, not
    // evidence that anything is gone. This sweep retracts NOTHING, notes
    // the change, and adopts the new vantage -- so the NEXT sweep from it
    // retracts normally. The rule terminates; it does not disable
    // retraction for ever.
    let stored = store::resource(conn, input.adapter_id, input.resource_id)?;
    let previous_vantage = stored.and_then(|r| r.last_provider_id);
    if let Some(previous) = previous_vantage {
        if previous != input.provider_id {
            let detail = format!("{previous} -> {}", input.provider_id);
            store::append_discrepancy(
                conn,
                input.adapter_id,
                input.resource_id,
                KIND_VANTAGE,
                crawl_id,
                &detail,
                now,
            )?;
            return Ok(RetractionOutcome {
                retracted: 0,
                discrepancies: vec![Discrepancy {
                    kind: KIND_VANTAGE.to_owned(),
                    detail,
                }],
            });
        }
    }

    // --- EXEMPTION: the empty sweep ----------------------------------
    //
    // A qualifying sweep that returned ZERO observations against a
    // non-empty live set is asking the host to retract 100% of a resource.
    // That is the one constant that needs no justification, and
    // `--confirm-empty` is the only way past it. No percentage threshold
    // anywhere: 99% is not a number anyone can defend.
    if observed_ids.is_empty() && !live.is_empty() && !input.options.confirm_empty {
        let detail = format!(
            "a complete sweep returned no observations against {} live record(s); \
             nothing retracted -- re-run with --confirm-empty to retract them all",
            live.len()
        );
        store::append_discrepancy(
            conn,
            input.adapter_id,
            input.resource_id,
            KIND_EMPTY,
            crawl_id,
            &detail,
            now,
        )?;
        return Ok(RetractionOutcome {
            retracted: 0,
            discrepancies: vec![Discrepancy {
                kind: KIND_EMPTY.to_owned(),
                detail,
            }],
        });
    }

    let mut retracted = 0usize;
    let mut derivation_changed = 0usize;
    let mut definition_changed = 0usize;
    let mut history_start_exempt = 0usize;

    for (local_id, revision, head) in &live {
        if observed_ids.contains(local_id) {
            continue;
        }
        // --- EXEMPTION: a named degrade ------------------------------
        if exempt_ids.contains(local_id) {
            continue;
        }
        // --- EXEMPTION: history_start --------------------------------
        //
        // The adapter told us, in this same run's `status.read`, that its
        // history begins at T. A record older than T was never in this
        // sweep's reach, so its absence is not evidence of anything. A
        // pruned or re-pointed Esplora is otherwise the wrong retraction
        // that never self-corrects.
        if let Some(start) = &input.history_start {
            let at = head
                .provenance
                .effective_at
                .as_ref()
                .unwrap_or(&head.provenance.observed_at);
            // INSTANTS, never text. `sumer_wire`'s validator accepts
            // fractional seconds and numeric offsets, so text comparison is
            // wrong on reachable input in both directions:
            // `"...T00:00:00Z" < "...T00:00:00.500Z"` is false as strings
            // and true as instants, and `01:00:00+02:00` and `00:00:00Z`
            // are the same moment spelled two ways. Either reading puts a
            // record inside the provider's window on the wrong side of it
            // and retracts it.
            //
            // A timestamp that cannot be ordered EXEMPTS. The two errors
            // are not symmetric: a wrong exemption leaves a stale row, a
            // wrong retraction hides a financial record.
            let ordered = match (time::instant(at), time::instant(start)) {
                (Some(at), Some(start)) => at >= start,
                _ => false,
            };
            if !ordered {
                history_start_exempt += 1;
                continue;
            }
        }

        let meta = head_meta.get(local_id);
        let reason = if meta.is_some_and(|m| m.derivation != input.hello_derivation) {
            derivation_changed += 1;
            REASON_DERIVATION
        } else if meta.is_some_and(|m| m.fingerprint.as_deref() != Some(input.fingerprint)) {
            definition_changed += 1;
            REASON_DEFINITION
        } else {
            REASON_ABSENT
        };
        store::append_retraction(
            conn,
            input.adapter_id,
            local_id,
            *revision,
            reason,
            crawl_id,
            now,
        )?;
        retracted += 1;
    }

    // One discrepancy row per sweep for each of the two software-change
    // reasons -- not one per record. A derivation bump is ONE event.
    for (count, kind, what) in [
        (derivation_changed, KIND_DERIVATION, "derivation"),
        (definition_changed, KIND_DEFINITION, "resource definition"),
        (history_start_exempt, KIND_HISTORY_START, "history_start"),
    ] {
        if count == 0 {
            continue;
        }
        let detail = match kind {
            KIND_HISTORY_START => format!(
                "{count} live record(s) predate the adapter's reported history_start and were \
                 exempted from retraction"
            ),
            _ => format!(
                "the {what} changed; {count} record(s) that can no longer be re-emitted were \
                 retired under {kind}, naming the software change rather than the provider"
            ),
        };
        store::append_discrepancy(
            conn,
            input.adapter_id,
            input.resource_id,
            kind,
            crawl_id,
            &detail,
            now,
        )?;
        discrepancies.push(Discrepancy {
            kind: kind.to_owned(),
            detail,
        });
    }

    Ok(RetractionOutcome {
        retracted,
        discrepancies,
    })
}

/// Whether a retraction currently buries this chain.
///
/// **The liveness rule, and the only copy of it** (`spec/observation.md`
/// §8.4): a record is live iff the fold's live set holds it AND its
/// chain's HIGHEST revision EXCEEDS every retraction revision for its key.
/// Revival needs no special case -- a later sweep appends revision N+1,
/// which exceeds N.
///
/// "Highest", not "the head's": `revision` is arrival order and the head
/// is fold order, so a wall clock that stepped backwards between refreshes
/// (NTP, a restored snapshot -- `received_at` is `SystemTime::now()` at
/// second precision) sorts the re-emitted observation BELOW the buried
/// head, and the head's revision never grows past the retraction. The
/// record is then buried permanently while its chain grows a row per
/// refresh. The invariant this restores: the revision liveness is measured
/// against is the maximum in the chain.
fn is_buried(high_water: &HashMap<String, u64>, local_id: &str, revision: u64) -> bool {
    high_water
        .get(local_id)
        .is_some_and(|retracted| *retracted >= revision)
}

/// The live set of one resource: fold heads that are `active` and not
/// buried, sorted by `local_id`.
///
/// **Stated once and called from all three places** that need it -- the
/// retraction diff base ([`derive_retractions`]), what the user sees
/// ([`live_records`]), and the dedup decision in [`ingest_page`], which
/// asks [`is_buried`] directly about one key. The comment this replaces
/// claimed the rule was already shared while three copies of the predicate
/// sat beside each other, so the guarantee that the user's view cannot
/// diverge from the retraction base rested on three copies happening to
/// match.
fn live_set(
    fold: &Fold,
    high_water: &HashMap<String, u64>,
    adapter_id: &str,
    resource_id: &str,
) -> Vec<(String, u64, Observation)> {
    let mut live: Vec<(String, u64, Observation)> = fold
        .live_set()
        .into_iter()
        .filter(|((owner, _), head)| {
            *owner == adapter_id && head.observation.resource_id == resource_id
        })
        .filter_map(|((_, local_id), head)| {
            // The chain's highest revision, not the head's -- see
            // [`is_buried`]. It is also the revision a retraction derived
            // from this set is recorded against, so the two can never
            // disagree about what would bury this record.
            let revision = fold.highest_revision(adapter_id, local_id)?;
            (!is_buried(high_water, local_id, revision))
                .then(|| (local_id.to_owned(), revision, head.observation.clone()))
        })
        .collect();
    live.sort_by(|a, b| a.0.cmp(&b.0));
    live
}

/// The live set of one resource, as `history` reads it.
pub fn live_records(
    store: &Store,
    adapter_id: &str,
    resource_id: &str,
) -> Result<Vec<(String, u64, Observation)>> {
    let mut fold = Fold::new();
    for stored in store::observations_for_adapter(store.conn(), adapter_id)? {
        fold.ingest(stored.observation);
    }
    let high_water = store::retraction_high_water(store.conn(), adapter_id)?;
    Ok(live_set(&fold, &high_water, adapter_id, resource_id))
}

/// SHA-256 over the resource DEFINITION the adapter just described: its
/// `kind` and its `provider_extra`.
///
/// `label` is deliberately absent. Renaming a wallet is not a change to
/// what the wallet *is*, and hashing it would retire a user's whole
/// history under `resource_definition_changed` the day they fixed a typo.
/// A Bitcoin wallet's `provider_extra` carries its `address_set_sha256`,
/// so adding or removing an address does move this -- which is the case
/// the reason exists for.
#[must_use]
pub fn resource_fingerprint(descriptor: &sumer_wire::ResourceDescriptor) -> String {
    let payload = serde_json::json!({
        "kind": descriptor.kind,
        "provider_extra": descriptor.provider_extra.clone().unwrap_or_default(),
    });
    crate::hash::sha256_hex(payload.to_string().as_bytes())
}

/// `status.read`'s `history_start` for one resource, if it reported one.
#[must_use]
pub fn history_start(statuses: &[ResourceStatus], resource_id: &str) -> Option<Rfc3339> {
    statuses
        .iter()
        .find(|s| s.resource_id == resource_id)
        .and_then(|s| s.history_start.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One scripted run of `adapters/fake/fake_adapter.py`, answering
    /// `history.read` and nothing else -- these tests call
    /// [`sweep_resource`] directly, so discovery and status never happen.
    fn run(observations: serde_json::Value) -> serde_json::Value {
        json!({
            "label": "fault point",
            "hello": {
                "protocol": "1",
                "adapter_id": "fake-adapter",
                "adapter_version": "0.1.0",
                "capabilities": ["history.read"],
                "local_id_derivation": "fixture-literal@1",
                "max_in_flight": 1
            },
            "provenance": {
                "adapter_id": "fake-adapter",
                "provider_id": "p1",
                "surface": "s",
                "observed_at": "2026-01-01T00:00:00Z",
                "completeness": "complete"
            },
            "on": {"history.read": [{"when": {}, "do": [{"op": "reply_ok", "body": {
                "observations": observations,
                "statuses": [{
                    "resource_id": "acct",
                    "page": {"cursor_resumable": "exact", "next": null}
                }]
            }}]}]}
        })
    }

    fn obs(local_id: &str, amount: &str) -> serde_json::Value {
        json!({
            "resource_id": "acct",
            "local_id": local_id,
            "state": "active",
            "surface": "s",
            "posting": "posted",
            "amount": {"asset": "usd", "amount": amount},
            "raw_sign": "provider_positive",
            "description": local_id
        })
    }

    fn input<'a>(fingerprint: &'a str) -> SweepInput<'a> {
        SweepInput {
            adapter_id: "fake-adapter",
            resource_id: "acct",
            hello_derivation: "fixture-literal@1",
            fingerprint,
            provider_id: "p1",
            history_start: None,
            connection_violation: None,
            options: SweepOptions::default(),
        }
    }

    async fn connect(fixture: &std::path::Path, run: usize) -> AdapterHandle {
        let adapter = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../adapters/fake/fake_adapter.py");
        AdapterHandle::spawn(
            vec!["python3".to_owned(), adapter.to_string_lossy().into_owned()],
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

    /// **The final page is ONE transaction.**
    ///
    /// This simulates the window `tests/restart.rs` documents but cannot
    /// interlock: the last page's reply has been received, its inserts,
    /// stamps, cursor write, retraction derivation, metadata updates and
    /// the crawl's open -> drained transition have all been issued, and the
    /// process dies before the commit. The real event is a SIGKILL; what
    /// this substitutes is a failure at the same instant, which leaves the
    /// same durable state and, unlike a signal, lands there every time.
    ///
    /// What it asserts is what a SPLIT transaction cannot satisfy: **not
    /// one row** of that page survived. An implementation that commits the
    /// inserts and then derives retractions separately leaves them behind,
    /// and this test says so.
    #[tokio::test]
    async fn the_final_page_is_one_transaction() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 not found on PATH -- skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("sumer-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = dir.join("fixture.json");
        std::fs::write(
            &fixture,
            serde_json::to_vec(&json!({
                "case": "fault",
                "script": {"runs": [
                    run(json!([obs("keep", "10.00"), obs("drop", "20.00")])),
                    run(json!([obs("keep", "11.00")])),
                ]}
            }))
            .unwrap(),
        )
        .unwrap();

        let profile = crate::Profile::new(dir.join("profile"));
        let mut store = Store::init(&profile).unwrap();

        // A clean sweep first: two live records.
        let handle = connect(&fixture, 0).await;
        let planted = sweep_resource(&mut store, &handle, &input("fp1"))
            .await
            .unwrap();
        let _ = handle.close().await;
        assert!(planted.complete);
        assert_eq!(planted.new, 2);

        // Now the same sweep the gate table's control case runs -- `drop`
        // omitted, `keep` revised -- killed in the last instant before its
        // transaction commits.
        ABORT_BEFORE_FINAL_COMMIT.store(true, std::sync::atomic::Ordering::SeqCst);
        let handle = connect(&fixture, 1).await;
        let crashed = sweep_resource(&mut store, &handle, &input("fp1")).await;
        let _ = handle.close().await;
        assert!(crashed.is_err(), "the simulated crash aborts the sweep");

        let rows: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM observation", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            rows, 2,
            "NOT ONE ROW of the final page survived -- inserts, stamps, cursor, \
             derivation and the drained transition are one transaction. A split \
             one leaves `keep` revision 2 behind and this count is 3."
        );
        let retractions: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM retraction", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            retractions, 0,
            "and the retraction it would have derived is not there"
        );
        let crawl = store::crawls(store.conn(), 1).unwrap();
        let crawl = crawl.first().unwrap();
        assert!(
            !crawl.drained && !crawl.complete,
            "a crawl recorded DRAINED whose retractions were never derived is the one \
             state this design must not be able to reach"
        );

        // And the next refresh starts fresh and finishes the job.
        let handle = connect(&fixture, 1).await;
        let recovered = sweep_resource(&mut store, &handle, &input("fp1"))
            .await
            .unwrap();
        let _ = handle.close().await;
        assert!(recovered.complete);
        assert_eq!(recovered.retracted, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One observation, built directly rather than scripted: the clock
    /// behaviour this asserts on cannot be reached through the adapter.
    fn observed(local_id: &str, received_at: &str) -> Observation {
        use sumer_money::{Amount, AssetId};
        use sumer_wire::{
            Completeness, ObservationState, Posting, Provenance, ProvenanceWire, RawSign, Staleness,
        };
        Observation {
            resource_id: "acct".to_owned(),
            local_id: local_id.to_owned(),
            provider_id: None,
            supersedes_provider_id: None,
            state: ObservationState::Active,
            tombstone_reason: None,
            surface: "s".to_owned(),
            posting: Posting::Posted,
            amount: Amount::parse(AssetId::new("usd").unwrap(), "10.00").unwrap(),
            fees: None,
            raw_sign: RawSign::ProviderPositive,
            description: local_id.to_owned(),
            provider_extra: serde_json::Map::new(),
            provenance: Provenance::stamp(
                ProvenanceWire {
                    adapter_id: "fake-adapter".to_owned(),
                    provider_id: "p1".to_owned(),
                    surface: "s".to_owned(),
                    observed_at: Rfc3339::new("2026-01-01T00:00:00Z").unwrap(),
                    effective_at: None,
                    completeness: Completeness::Complete,
                },
                Rfc3339::new(received_at).unwrap(),
                Staleness::Live,
            ),
        }
    }

    /// **A record buried by a retraction revives on the next sweep, even
    /// if the wall clock stepped backwards** (`spec/observation.md` §8.4).
    ///
    /// `received_at` is `SystemTime::now()` at second precision with no
    /// monotonic guard, so an NTP correction or a restored VM snapshot
    /// makes the re-emitted observation sort BELOW the buried head in the
    /// fold's total order. Liveness then compared the retraction
    /// high-water against the fold HEAD's revision -- a number in arrival
    /// order -- so the head's revision never grew past the retraction, the
    /// record stayed buried for ever, and because `currently_retracted`
    /// correctly disables dedup the chain grew one row per refresh with no
    /// way back.
    ///
    /// The two orders are both correct and they answer different
    /// questions; the bug was comparing across them. Liveness is an
    /// arrival-order question ("has the host learned anything since the
    /// retraction?"), so it asks the chain's HIGHEST revision.
    #[test]
    fn a_backwards_clock_step_does_not_bury_a_record_for_ever() {
        let mut fold = Fold::new();
        assert_eq!(fold.ingest(observed("x", "2026-01-01T00:00:10Z")), 1);
        // The retraction that buried revision 1, then the same record
        // re-emitted while the clock reads five seconds EARLIER.
        let high_water = HashMap::from([("x".to_owned(), 1u64)]);
        assert_eq!(fold.ingest(observed("x", "2026-01-01T00:00:05Z")), 2);

        let live = live_set(&fold, &high_water, "fake-adapter", "acct");
        assert_eq!(
            live.len(),
            1,
            "revision 2 exceeds the retraction at revision 1: the record is live \
             again, whatever the clock did"
        );
        assert_eq!(
            live[0].1, 2,
            "and the revision liveness (and the next retraction) is measured \
             against is the chain's highest, not the fold head's"
        );
        assert!(
            !is_buried(&high_water, "x", live[0].1),
            "the invariant: liveness is measured against the chain's maximum \
             revision, so nothing can bury a record the host has learned since"
        );
    }
}
