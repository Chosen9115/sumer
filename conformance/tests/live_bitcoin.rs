//! The LIVE invariant check: the real `sumer-bitcoin-adapter` against a real
//! Esplora deployment, over one well-known public address.
//!
//! `#[ignore]`d and gated on `SUMER_LIVE=1`, so it never runs in required
//! CI. `.github/workflows/nightly.yml` is the only thing that runs it.
//!
//! **A floor comes first, and everything else is downstream of it.** Every
//! invariant below — the ordering, the status coverage, the outcomes, the
//! resume bracket, even the cross-endpoint balance reconciliation — passes
//! on ZERO observations, and "both sides say 0" is this project's dominant
//! failure species. So the first thing this check asserts is that the
//! wallet is NOT empty: the address's oldest transaction must be there by
//! txid AND carrying the figure it has carried since 2010, and there must
//! be at least as many transactions as there were on the day the corpus
//! was recorded. Bitcoin history only grows, so a floor
//! never goes stale — it can only become more slack, and the txid half
//! never does.
//!
//! What this cannot be: a test of the mapping. It has one external figure
//! and no more -- the pizza payment's 10,000 BTC, an ANCHOR (see
//! `OLDEST_TX_SATS`) -- and otherwise checks the shape of the answers and
//! the relationships between them. That one anchor is load-bearing: every
//! other money assertion here compares the adapter against itself, and an
//! adapter answering `0` to everything satisfied all of them at once. The
//! amounts in general are pinned by `map.rs`'s unit tests and by the
//! replayed corpora, against JSON whose checksums are recorded.
//!
//! # Proving this check has teeth
//!
//! Every assertion here runs against the network, so the mutation battery
//! (`tests/mutations.rs`, which drives recorded fixtures) cannot reach it.
//! `SUMER_LIVE_WRAPPER` is what replaces it: it puts one of the battery's
//! man-in-the-middle wrappers (`conformance/mutations/adapters/`) in front
//! of the real adapter, so a break can be performed on the live check the
//! same way it is performed on a fixture.
//!
//! The nightly leaves it unset. It exists because a check nobody has ever
//! seen go red for the right reason is a check nobody knows the shape of --
//! and the resume bracket below was exactly that: it passed against an
//! adapter whose every resumed page came back empty.
//!
//! **A wrapper's mere invocation proves nothing.** A wrapper passes errors
//! through untouched, so any red run reads as a kill: a mutant that broke
//! the handshake, or one that got blunt and now trips section 5 as well,
//! looks exactly like a mutant killed by the section it claims. The
//! offline battery answers this with EXACTNESS -- the ids a mutant
//! provokes must EQUAL the ids it declares (`tests/mutations.rs`) -- and
//! `SUMER_LIVE_EXPECT` is that rule here. It names the section the break
//! must be caught by:
//!
//! ```text
//! SUMER_LIVE=1 \
//! SUMER_LIVE_WRAPPER=mutations/adapters/btc_resumed_pages_emptied.py \
//! SUMER_LIVE_EXPECT='§9b' \
//!   cargo test -p sumer-conformance --test live_bitcoin -- --ignored --nocapture
//! ```
//!
//! The wrapper path is resolved by the adapter process, whose working
//! directory cargo sets to this PACKAGE's root -- hence
//! `mutations/adapters/...` and not `conformance/mutations/adapters/...`,
//! which silently spawns nothing and reads as a kill at the handshake.
//!
//! With it set the test is INVERTED: it passes only when the check failed
//! at that section, and fails when the mutant survived, when it was caught
//! somewhere else first, or when the provider rate-limited the run into
//! proving nothing.
//!
//! What that establishes, and what it does not: every assertion BEFORE the
//! named one passed, because the run reached it. The assertions AFTER it
//! were never evaluated -- an ordered check stops at its first failure, so
//! "and nowhere else" is a claim about everything upstream of the kill and
//! nothing downstream of it. The offline battery collects a whole run's
//! failures and does not have this limit; this is the price of assertions
//! that panic, and it is stated rather than implied to be covered.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use sumer_host::{AdapterHandle, Terminal};
use sumer_money::{Amount, AssetId};
use sumer_wire::{
    Observation, ObservationState, PageRequest, Posting, RawSign, ReadOutcome, ResourceQuery,
};

/// The address this check watches: the one that received the 10,000 BTC of
/// the 2010 "Bitcoin pizza" payment. A documented public artefact, not
/// anyone's live wallet, and the same address `corpus/basic` was recorded
/// from.
const ADDRESS: &str = "17SkEw2md5avVNyYgj6RiXuQKNwkXaxFyQ";

/// Its oldest transaction — the pizza payment itself, in block 57043.
/// Confirmed history is immutable at this depth: if this txid is not in the
/// answer, the answer is wrong, however well-formed it looks.
const OLDEST_TXID: &str = "a1075db55d416d3ca199f55b6084e2115b9345e16c5cf302fc80e9d5fbf5d48d";

/// Confirmed transactions on 2026-09-07, at tip height 965942
/// (`adapters/bitcoin/corpus/basic/RECORDED.md`). A FLOOR, never an
/// equality: the address is published, anyone can pay it, and the count
/// only ever goes up.
const TX_COUNT_FLOOR: usize = 17;

/// What that transaction moved TO this address, in satoshis: the 10,000
/// BTC of the pizza payment, with nothing of the address's own spent as an
/// input, so the net delta is the whole of it.
///
/// THE ONLY EXTERNAL ORACLE THIS CHECK HAS. Every other money assertion
/// here compares the adapter against itself -- section 3 and section 8
/// reconcile two of its own endpoints, section 9b compares a resume
/// against the read before it -- and an adapter that answers `0` to
/// everything is perfectly self-consistent and passed all of them.
/// Internal consistency is the easiest thing for a broken implementation
/// to provide; one figure it did not learn from the adapter is what makes
/// the rest of them mean anything.
///
/// An ANCHOR, not a test of the mapping: one observation, one number, and
/// a number settled in block 57043 with ~900,000 blocks on top of it. It
/// can no more go stale than `OLDEST_TXID` can.
const OLDEST_TX_SATS: &str = "1000000000000";

const SOURCE: &str = "https://blockstream.info/api";
const RESOURCE: &str = "live";

fn adapter_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("a test binary has a path");
    dir.pop(); // deps/
    dir.pop(); // <profile>/
    dir.join(format!(
        "sumer-bitcoin-adapter{}",
        std::env::consts::EXE_SUFFIX
    ))
}

/// `(height, txid)` of a confirmed observation — the key the adapter orders
/// confirmed history by, read back off the wire.
fn confirmed_key(o: &Observation) -> Option<(u64, String)> {
    let height = o.provider_extra.get("block_height")?.as_u64()?;
    Some((height, o.provider_id.clone()?))
}

/// The money an observation carries. What a resume owes is not just a set
/// of `local_id`s: it is these figures, attached to those ids. Compared by
/// VALUE, because `fees` is a nested money object and a comparison that
/// settles for "present, and denominated in sat" tolerates the figure
/// itself being rewritten -- the silent degradation the offline suite
/// already shipped once.
#[derive(Debug, PartialEq, Eq)]
struct Money {
    amount: Amount,
    fees: Option<Amount>,
    raw_sign: RawSign,
}

impl Money {
    fn of(o: &Observation) -> Money {
        Money {
            amount: o.amount.clone(),
            fees: o.fees.clone(),
            raw_sign: o.raw_sign,
        }
    }
}

impl std::fmt::Display for Money {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.amount, self.amount.asset())?;
        match &self.fees {
            Some(fees) => write!(f, ", fees {fees} {}", fees.asset())?,
            None => f.write_str(", no fees")?,
        }
        write!(f, ", {:?}", self.raw_sign)
    }
}

/// The five outcomes this adapter may ever answer with
/// (`adapters/bitcoin/README.md`). `reauth_required`, `revoked`,
/// `sca_required` and `gone` are not among them: a watch-only wallet has no
/// credential, no session, and nothing that can be revoked.
fn outcome_is_allowed(outcome: &ReadOutcome) -> bool {
    matches!(
        outcome,
        ReadOutcome::Fetched { .. }
            | ReadOutcome::RateLimited { .. }
            | ReadOutcome::Unavailable
            | ReadOutcome::Stale { .. }
            | ReadOutcome::NotFetched
    )
}

/// One history read, followed to its terminal page.
#[derive(Default)]
struct Drained {
    observations: Vec<Observation>,
    /// The cursors the adapter handed out along the way, in order.
    cursors: Vec<String>,
}

/// Reads history from `from` to EXHAUSTION -- following every `next` until
/// the adapter answers with a terminal page -- checking each page's status
/// on the way. Both the uninterrupted read and the resumed one go through
/// here, so "resuming" means the same thing as "reading": drain it, or the
/// answer is not an answer.
///
/// `None` means the provider rate-limited us and the caller must SKIP.
async fn drain(handle: &AdapterHandle, from: Option<PageRequest>) -> Option<Drained> {
    let mut out = Drained::default();
    let mut page = from;
    loop {
        let reply = handle
            .history_read(vec![ResourceQuery {
                resource_id: RESOURCE.to_owned(),
                page: page.clone(),
            }])
            .await
            .expect("history.read");
        assert_eq!(reply.statuses.len(), 1);
        let status = &reply.statuses[0];
        assert_eq!(status.resource_id, RESOURCE);
        assert!(outcome_is_allowed(&status.outcome), "{:?}", status.outcome);
        if let ReadOutcome::RateLimited { retry_after_ms } = status.outcome {
            eprintln!("SKIP: {SOURCE} rate-limited us (retry after {retry_after_ms}ms)");
            return None;
        }
        assert!(
            matches!(status.outcome, ReadOutcome::Fetched { .. }),
            "history.read: {:?} -- provider_detail: {:?}",
            status.outcome,
            status.provider_detail
        );
        let next = status.page.as_ref().and_then(|p| p.next.clone());
        out.observations.extend(reply.observations);
        match next {
            Some(PageRequest::Cursor { cursor }) => {
                out.cursors.push(cursor.clone());
                page = Some(PageRequest::Cursor { cursor });
            }
            Some(other) => panic!("this adapter serves cursor pages only, got {other:?}"),
            None => return Some(out),
        }
        assert!(out.cursors.len() < 64, "history.read never drained");
    }
}

#[tokio::test]
#[ignore = "live: talks to a public Esplora deployment; nightly only"]
async fn the_live_invariants_hold() {
    if std::env::var("SUMER_LIVE").ok().as_deref() != Some("1") {
        eprintln!("SUMER_LIVE is not 1 -- refusing to touch the network");
        return;
    }
    // The invariants run in their own task so that `SUMER_LIVE_EXPECT` can
    // read the failure instead of only inheriting it. See the module docs.
    let outcome = tokio::spawn(check_the_invariants()).await;
    match (std::env::var("SUMER_LIVE_EXPECT").ok(), outcome) {
        (None, Ok(_)) => {}
        (None, Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        (None, Err(e)) => panic!("the live check did not run to a verdict: {e}"),
        (Some(expected), Ok(true)) => panic!(
            "SUMER_LIVE_EXPECT={expected}, and every invariant passed: whatever is \
             in front of this adapter SURVIVED, so {expected} does not have the \
             teeth it is claimed to have"
        ),
        // A skipped mutant is NOT a kill. Printing SKIP and exiting 0 here
        // is a green run that proved nothing -- the exact false pass this
        // whole mechanism exists to make impossible, and a contradiction
        // of the module docs above, which promise an inconclusive run
        // fails. The un-expected path keeps its SKIP: a bare nightly that
        // hits a 429 is the provider asking us to slow down, not a defect.
        (Some(expected), Ok(false)) => panic!(
            "SUMER_LIVE_EXPECT={expected}: the provider rate-limited this run, so \
             nothing was proved about {expected} either way. A skipped mutant is not \
             a kill -- run it again when the provider will talk to us"
        ),
        (Some(expected), Err(e)) => {
            let message = panic_message(e);
            assert!(
                message.contains(&expected),
                "SUMER_LIVE_EXPECT={expected}, but the check was stopped somewhere \
                 else first, so the break it names is unproved -- and every \
                 assertion after the one below went unevaluated: {message}"
            );
            eprintln!("killed at {expected}, as declared:\n{message}");
        }
    }
}

/// The message a failed invariant carried, for `SUMER_LIVE_EXPECT` to
/// match the declared section against.
fn panic_message(error: tokio::task::JoinError) -> String {
    if !error.is_panic() {
        return format!("the live check did not run to a verdict: {error}");
    }
    let panic = error.into_panic();
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "a panic carrying no message".to_owned())
}

/// Every invariant, in order. `true` once they have all been checked;
/// `false` when the provider rate-limited the run, which proves nothing
/// either way and is not a failure.
async fn check_the_invariants() -> bool {
    let binary = adapter_binary();
    assert!(binary.is_file(), "{} is not built", binary.display());

    let dir = std::env::temp_dir().join(format!("sumer-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let wallets = dir.join("wallets.json");
    std::fs::write(
        &wallets,
        serde_json::json!({"wallets": [{"resource_id": RESOURCE, "addresses": [ADDRESS]}]})
            .to_string(),
    )
    .expect("the wallet file");

    // No --state-dir on purpose: this check must not leave state behind for
    // the next night's run to be quietly influenced by, and every tombstone
    // it could produce would be one it had no way to verify.
    //
    // `SUMER_LIVE_WRAPPER`, when set, puts one of the mutation battery's
    // man-in-the-middle wrappers in front of the adapter (see the module
    // docs). Unset everywhere except when someone is deliberately breaking
    // the adapter to watch this check catch it.
    let mut argv: Vec<String> = match std::env::var("SUMER_LIVE_WRAPPER") {
        Ok(wrapper) => vec!["python3".to_owned(), wrapper],
        Err(_) => Vec::new(),
    };
    argv.extend([
        binary.to_string_lossy().into_owned(),
        "--wallets".to_owned(),
        wallets.to_string_lossy().into_owned(),
        "--source".to_owned(),
        SOURCE.to_owned(),
    ]);
    let handle = AdapterHandle::spawn(argv, [])
        .await
        .expect("the adapter handshakes");

    // 1. The handshake.
    let hello = handle.hello();
    assert_eq!(hello.protocol, "1");
    assert_eq!(hello.adapter_id, "sumer-bitcoin");
    assert_eq!(
        hello.local_id_derivation, "btc-txid@1",
        "the local_id derivation is what every observation chain is keyed by; a silent change to \
         it re-identifies every record this adapter ever emitted"
    );

    // 2. Discovery.
    let resources = handle.resources_list().await.expect("resources.list");
    let ids: Vec<String> = resources
        .resources
        .iter()
        .map(|r| r.resource_id.clone())
        .collect();
    assert_eq!(ids, vec![RESOURCE.to_owned()]);

    // 3. Balances. A 429 is a SKIP, not a failure: a free public service
    // telling us to slow down is not a defect in this adapter, and turning
    // it into a nightly failure would train everyone to ignore the alarm.
    let balances = handle
        .balances_read(vec![RESOURCE.to_owned()])
        .await
        .expect("balances.read");
    assert_eq!(
        balances.statuses.len(),
        1,
        "every requested resource_id appears in statuses exactly once"
    );
    let status = &balances.statuses[0];
    assert_eq!(status.resource_id, RESOURCE);
    assert!(outcome_is_allowed(&status.outcome), "{:?}", status.outcome);
    assert!(
        status.credential_expires_at.is_none() && status.strong_auth_expires_at.is_none(),
        "a watch-only wallet has no credential and no authentication session"
    );
    if let ReadOutcome::RateLimited { retry_after_ms } = status.outcome {
        eprintln!("SKIP: {SOURCE} rate-limited us (retry after {retry_after_ms}ms)");
        return false;
    }
    assert!(
        matches!(status.outcome, ReadOutcome::Fetched { .. }),
        "balances.read: {:?} -- provider_detail: {:?}",
        status.outcome,
        status.provider_detail
    );

    // A LIST, sorted -- never a set. A set comparison silently tolerates a
    // duplicate: append a second `unconfirmed` line carrying zero and set
    // equality still holds, every line is still a well-formed integer
    // number of satoshis, and the reconciliation in section 8 still
    // balances because zero changes no sum. "Two balance lines" is a claim
    // about COUNT, and a comparison that cannot see multiplicity is the
    // same silent degradation the `fees` comparison shipped with.
    let sat = AssetId::new("sat").expect("sat is a legal asset id");
    let mut categories: Vec<String> = balances
        .observations
        .iter()
        .map(|b| b.category.clone())
        .collect();
    categories.sort();
    assert_eq!(
        categories,
        vec!["confirmed".to_owned(), "unconfirmed".to_owned()],
        "§3: EXACTLY two balance lines, one per category -- never summed, no third \
         category, and neither category twice. A host that adds up whatever lines \
         arrive double-counts the wallet the moment one is emitted twice"
    );
    let mut reported = Amount::parse(sat.clone(), "0").expect("zero parses");
    for line in &balances.observations {
        let amount = line
            .amount
            .clone()
            .unwrap_or_else(|| panic!("{}: a successful read reports a figure", line.category));
        assert_eq!(amount.asset(), &sat);
        assert_eq!(amount.scale(), 0, "satoshis are integers");
        reported = reported.add(&amount).expect("same asset");
    }

    // 4. History, paginated to exhaustion, on the same connection.
    let Some(first) = drain(&handle, None).await else {
        return false;
    };
    let (observations, cursors) = (first.observations, first.cursors);

    // 5. THE FLOOR. Everything after this passes on an empty answer.
    let confirmed: Vec<&Observation> = observations
        .iter()
        .filter(|o| o.state == ObservationState::Active && o.posting == Posting::Posted)
        .collect();
    assert!(
        confirmed.len() >= TX_COUNT_FLOOR,
        "{} confirmed transactions for {ADDRESS}, fewer than the {TX_COUNT_FLOOR} recorded on \
         2026-09-07. Confirmed Bitcoin history does not shrink, so this is a broken read, not a \
         changed chain",
        confirmed.len()
    );
    // Against `confirmed`, not against every observation that came back:
    // the floor's second half is a claim that this transaction is HERE,
    // live and posted, and an id that arrives `tombstoned` or `pending` is
    // an id that arrives saying the opposite. Nothing conforming can trip
    // this -- block 57043 is 900k blocks deep, and with no `--state-dir`
    // this adapter emits no tombstone at all -- so the only thing it can
    // catch is a shortcut, which is the point.
    let oldest = confirmed
        .iter()
        .find(|o| o.local_id == format!("{RESOURCE}:{OLDEST_TXID}"))
        .unwrap_or_else(|| {
            panic!(
                "§5: the address's oldest transaction ({OLDEST_TXID}, block 57043) is missing \
                 from the live, posted history -- an answer without it is not this address's \
                 history, however well-formed it is"
            )
        });
    assert_eq!(
        oldest.amount,
        Amount::parse(sat.clone(), OLDEST_TX_SATS).expect("the anchor parses"),
        "§5: the pizza payment moved {OLDEST_TX_SATS} sat to {ADDRESS} in block 57043 and this \
         adapter says it moved {}. That figure is settled history, so this is the mapping being \
         wrong -- and it is the one number here that did not come from this adapter, which is \
         what makes every other money assertion in this file worth making",
        oldest.amount
    );

    // 6. Shape: every amount is an integer number of satoshis, and every
    // confirmed observation is strictly above the last by (height, txid).
    for o in &observations {
        assert_eq!(o.amount.asset(), &sat, "{}", o.local_id);
        assert_eq!(o.amount.scale(), 0, "{}", o.local_id);
        assert!(o.local_id.starts_with(&format!("{RESOURCE}:")));
    }
    let mut previous: Option<(u64, String)> = None;
    for o in &confirmed {
        let key = confirmed_key(o).unwrap_or_else(|| panic!("{}: no height/txid", o.local_id));
        if let Some(before) = &previous {
            assert!(
                &key > before,
                "confirmed history must ascend by (height, txid): {key:?} came after {before:?}"
            );
        }
        previous = Some(key);
    }

    // 7. Cursors strictly increase. Vacuous while one page holds the whole
    // wallet (512 KiB of observations is roughly 900 of them), which is
    // true of this address and stated rather than pretended otherwise.
    for pair in cursors.windows(2) {
        assert!(
            parse_cursor(&pair[1]) > parse_cursor(&pair[0]),
            "cursor did not advance: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }

    // 8. Cross-endpoint reconciliation, and only now that the floor has
    // passed: the balance came from `GET /address/:addr`'s counters, the
    // history from the transaction listings. Two endpoints, one wallet, and
    // the sum of every net delta must be what the counters say -- confirmed
    // plus unconfirmed, because a mempool transaction moves the second one.
    let mut summed = Amount::parse(sat.clone(), "0").expect("zero parses");
    for o in &observations {
        if o.state == ObservationState::Active {
            summed = summed.add(&o.amount).expect("same asset");
        }
    }
    assert_eq!(
        summed.cmp_same_asset(&reported).expect("same asset"),
        std::cmp::Ordering::Equal,
        "the transaction listings sum to {summed:?} but the address counters say {reported:?}"
    );

    // 9. The resume bracket: read again from a cursor in the middle of the
    // confirmed section. TWO things are owed, and only together do they
    // mean anything:
    //
    //   (a) nothing at or below the cursor comes back, and
    //   (b) everything above it DOES.
    //
    // (a) alone is satisfied by an adapter that answers a resume with an
    // empty, successfully-fetched terminal page -- total data loss, dressed
    // as a clean read, and it passed this check for as long as (a) was all
    // there was. An assertion about the observations a reply happened to
    // contain cannot see the reply that contains none; the history a resume
    // OWES has to be named independently, and it is: the uninterrupted read
    // above already established it.
    //
    // The cursor is built here rather than taken from a reply's `next`,
    // because this wallet drains in one page and so hands out no `next` at
    // all. Its grammar is documented (`"<height>:<txid>"`), and the whole
    // point of `exact` is that a caller may hold one.
    //
    // THE EXEMPTION, verbatim from `adapters/bitcoin/README.md`, which says
    // the checker "must use exactly this definition, or it fails a
    // conforming adapter -- which is worse than not checking". An
    // observation is EXEMPT if any of these hold:
    //
    //   1. its `posting` is `pending` (currently unconfirmed), or
    //   2. its `state` is `tombstoned`, or
    //   3. a previous reply in the same connection reported the same
    //      `local_id` with `posting: "pending"` (it was in the tracked
    //      mempool set, and has since confirmed -- this is the case the
    //      exemption exists for), or with a different `block_height` (a
    //      reorg re-mined it, possibly *downwards*, which the confirmed
    //      section would otherwise suppress forever).
    //
    // Everything else is section 1 and is not exempt. Re-emitting section 2
    // is REQUIRED behaviour -- it is what turns "this pending transaction
    // was mined into a block below your cursor" into a revision instead of
    // a record that stays pending forever -- so a check without every
    // clause of this would fail a conforming adapter. The second half of
    // clause 3 was missing here until it was noticed: a reorg that re-mined
    // a transaction DOWNWARDS, in the seconds between the two reads below,
    // made an adapter doing exactly what the README requires fail 9a.
    let seen_pending: BTreeSet<String> = observations
        .iter()
        .filter(|o| o.posting == Posting::Pending)
        .map(|o| o.local_id.clone())
        .collect();
    // Clause 3's second half: the height each `local_id` was last reported
    // at by the read that already happened.
    let seen_height: BTreeMap<&String, u64> = observations
        .iter()
        .filter_map(|o| Some((&o.local_id, confirmed_key(o)?.0)))
        .collect();
    let middle = confirmed[confirmed.len() / 2];
    let (height, txid) = confirmed_key(middle).expect("a confirmed observation has both");
    let mark = (height, txid.clone());

    // What the resume owes, from the read that already happened: every
    // confirmed observation strictly above the cursor. Drift between the
    // two reads can only ADD to this set -- a drained crawl drops its
    // snapshot, so the resume takes a fresh one, and confirmed Bitcoin
    // history does not shrink. So this is a floor on the resume, never an
    // equality, and a transaction arriving between the two reads cannot
    // make it fail. The one thing that could: a reorg at the tip, in the
    // seconds between the two reads, of a block carrying a brand-new
    // payment to this address. Section 9a below has carried exactly that
    // exposure since it was written (a re-mined transaction would land
    // below the cursor) and this is the same bet, stated rather than
    // engineered around.
    let owed: BTreeMap<String, Money> = confirmed
        .iter()
        .filter(|o| confirmed_key(o).is_some_and(|key| key > mark))
        .map(|o| (o.local_id.clone(), Money::of(o)))
        .collect();
    assert!(
        !owed.is_empty(),
        "the cursor was placed above every confirmed transaction, so nothing is owed and the \
         resume below would prove nothing"
    );

    let Some(resumed) = drain(
        &handle,
        Some(PageRequest::Cursor {
            cursor: format!("{height}:{txid}"),
        }),
    )
    .await
    else {
        return false;
    };

    // 9a. Nothing at or below the cursor, exemption aside.
    for o in &resumed.observations {
        if o.posting == Posting::Pending
            || o.state == ObservationState::Tombstoned
            || seen_pending.contains(&o.local_id)
        {
            continue;
        }
        let key = confirmed_key(o).unwrap_or_else(|| panic!("{}: no height/txid", o.local_id));
        // Clause 3's second half. Checked after the key is read, because
        // the clauses above it cover the observations that have no height.
        if seen_height
            .get(&o.local_id)
            .is_some_and(|before| *before != key.0)
        {
            continue;
        }
        assert!(
            key > mark,
            "{} came back at {key:?}, at or below the cursor ({height}, {txid}), and it is not \
             exempt: it is neither pending nor tombstoned, and the uninterrupted read reported it \
             neither pending nor at a different block_height",
            o.local_id
        );
    }

    // 9b. And everything above it comes back, live, at the same terminal
    // state the first read reached, CARRYING THE SAME MONEY. `active` +
    // `posted` is not decoration here: it is what makes "delivered" mean
    // delivered. Every clause of the exemption above is an escape hatch a
    // shortcut could hide behind -- a resume that answered `tombstoned`
    // for the whole wallet would satisfy 9a completely -- and this run
    // passes no `--state-dir`, so this adapter cannot emit a tombstone at
    // all (`adapters/bitcoin/README.md`: without one, every sync is a
    // first run and no tombstone is ever emitted). A conforming adapter
    // therefore returns every one of these exactly as it returned them a
    // moment ago.
    //
    // "Exactly" has to include the FIGURES, or this section checks
    // identity and state and nothing else: rewrite every resumed amount to
    // zero, keep the ids, the heights, `active` and `posted`, and both 9a
    // and 9b hold -- the cross-endpoint reconciliation in section 8 ran
    // against the FIRST read and never sees a resumed value at all. The
    // whole resumed history is then corrupt and this check is green. So
    // the money is compared per `local_id` against what the first drain
    // said it was, by VALUE and on every occurrence: `fees` is a nested
    // money object, and comparing it as anything less than an `Amount`
    // (presence, or asset, or a bare string that never parses) is the
    // silent degradation this project has already shipped once.
    //
    // Confirmed money does not drift between two reads seconds apart: a
    // transaction's net delta to this address, its fee and its sign are
    // pure functions of a transaction that is already in a block. Only
    // `block_height` can move under a reorg, and that is not compared here.
    let mut delivered: BTreeSet<&String> = BTreeSet::new();
    for o in &resumed.observations {
        if o.state != ObservationState::Active || o.posting != Posting::Posted {
            continue;
        }
        delivered.insert(&o.local_id);
        let Some(before) = owed.get(&o.local_id) else {
            continue;
        };
        let now = Money::of(o);
        assert!(
            now == *before,
            "§9b: resuming at ({height}, {txid}) returned {} carrying {now}, but the \
             uninterrupted read a moment ago said {before}. Same id, same state, different \
             money: a host that trusts the resume writes the wrong figure into the history \
             it already has",
            o.local_id
        );
    }
    let missing: Vec<&String> = owed.keys().filter(|id| !delivered.contains(id)).collect();
    assert!(
        missing.is_empty(),
        "§9b: resuming at ({height}, {txid}) dropped {} of the {} confirmed transactions the \
         uninterrupted read placed above it: {missing:?}. The resumed read drained to a terminal \
         page and reported success, so this is not a truncated answer -- it is history the host \
         asked for, was told it had received, and will never ask for again",
        missing.len(),
        owed.len(),
    );

    // 10. status.read: the only reply that may carry the two clocks and
    // `history_start` at all (`spec/observation.md` §7), which is why the
    // "never emitted" claim is worth making HERE rather than only on the
    // reads above. A watch-only wallet has no credential and no
    // authentication session, so there is nothing for either clock to
    // describe, and this adapter never answers `stale` to a reachability
    // question.
    let reachability = handle
        .status_read(vec![RESOURCE.to_owned()])
        .await
        .expect("status.read");
    assert_eq!(reachability.statuses.len(), 1);
    let status = &reachability.statuses[0];
    assert_eq!(status.resource_id, RESOURCE);
    assert!(outcome_is_allowed(&status.outcome), "{:?}", status.outcome);
    assert!(!matches!(status.outcome, ReadOutcome::Stale { .. }));
    if let ReadOutcome::RateLimited { retry_after_ms } = status.outcome {
        eprintln!("SKIP: {SOURCE} rate-limited us (retry after {retry_after_ms}ms)");
        return false;
    }
    // The same bar the two reads above are held to. Without it every
    // assertion in this section is about a clock that is absent because
    // NOTHING WAS READ: `not_fetched` carries no clocks either, and it is
    // what an adapter that answers this op by declining would say.
    assert!(
        matches!(status.outcome, ReadOutcome::Fetched { .. }),
        "status.read: {:?} -- provider_detail: {:?}",
        status.outcome,
        status.provider_detail
    );
    assert!(
        status.credential_expires_at.is_none()
            && status.strong_auth_expires_at.is_none()
            && status.history_start.is_none(),
        "status.read invented a clock this adapter cannot know: {status:?}"
    );

    // 11. The connection ends without a protocol violation.
    assert!(
        !matches!(handle.close().await, Some(Terminal::Violation(_))),
        "the connection ended in a protocol violation"
    );
    let _ = std::fs::remove_dir_all(&dir);
    true
}

/// A cursor as an orderable key: `"<height>:<txid>"`, optionally
/// `":m:<mempool_txid>"`. Ordering compares the confirmed mark first, which
/// is exactly the order the adapter advances it in.
fn parse_cursor(cursor: &str) -> (u64, String, String) {
    let (height, rest) = cursor.split_once(':').unwrap_or((cursor, ""));
    let (txid, mempool) = rest.split_once(":m:").unwrap_or((rest, ""));
    (
        height.parse().unwrap_or(0),
        txid.to_owned(),
        mempool.to_owned(),
    )
}
