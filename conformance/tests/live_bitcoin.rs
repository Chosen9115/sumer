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
//! txid, and there must be at least as many transactions as there were on
//! the day the corpus was recorded. Bitcoin history only grows, so a floor
//! never goes stale — it can only become more slack, and the txid half
//! never does.
//!
//! What this cannot be: a test of the mapping. It has no independent oracle
//! for what any of these transactions are worth, so it checks the shape of
//! the answers and the relationships between them. The amounts are pinned
//! by `map.rs`'s unit tests and by the replayed corpora, against JSON whose
//! checksums are recorded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use sumer_host::{AdapterHandle, Terminal};
use sumer_money::{Amount, AssetId};
use sumer_wire::{Observation, ObservationState, PageRequest, Posting, ReadOutcome, ResourceQuery};

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

#[tokio::test]
#[ignore = "live: talks to a public Esplora deployment; nightly only"]
async fn the_live_invariants_hold() {
    if std::env::var("SUMER_LIVE").ok().as_deref() != Some("1") {
        eprintln!("SUMER_LIVE is not 1 -- refusing to touch the network");
        return;
    }
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
    let handle = AdapterHandle::spawn(
        vec![
            binary.to_string_lossy().into_owned(),
            "--wallets".to_owned(),
            wallets.to_string_lossy().into_owned(),
            "--source".to_owned(),
            SOURCE.to_owned(),
        ],
        [],
    )
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
        return;
    }
    assert!(
        matches!(status.outcome, ReadOutcome::Fetched { .. }),
        "balances.read: {:?} -- provider_detail: {:?}",
        status.outcome,
        status.provider_detail
    );

    let sat = AssetId::new("sat").expect("sat is a legal asset id");
    let categories: BTreeSet<String> = balances
        .observations
        .iter()
        .map(|b| b.category.clone())
        .collect();
    assert_eq!(
        categories,
        ["confirmed".to_owned(), "unconfirmed".to_owned()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "two balance lines, never summed, and no third category"
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
    let mut observations: Vec<Observation> = Vec::new();
    let mut cursors: Vec<String> = Vec::new();
    let mut page: Option<PageRequest> = None;
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
            return;
        }
        assert!(
            matches!(status.outcome, ReadOutcome::Fetched { .. }),
            "history.read: {:?} -- provider_detail: {:?}",
            status.outcome,
            status.provider_detail
        );
        observations.extend(reply.observations);
        let next = status.page.as_ref().and_then(|p| p.next.clone());
        match next {
            Some(PageRequest::Cursor { cursor }) => {
                cursors.push(cursor.clone());
                page = Some(PageRequest::Cursor { cursor });
            }
            Some(other) => panic!("this adapter serves cursor pages only, got {other:?}"),
            None => break,
        }
        assert!(cursors.len() < 64, "history.read never drained");
    }

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
    assert!(
        observations
            .iter()
            .any(|o| o.local_id == format!("{RESOURCE}:{OLDEST_TXID}")),
        "the address's oldest transaction ({OLDEST_TXID}, block 57043) is missing -- an answer \
         without it is not this address's history, however well-formed it is"
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
    // confirmed section, and nothing at or below it may come back.
    //
    // The cursor is built here rather than taken from a reply's `next`,
    // because this wallet drains in one page and so hands out no `next` at
    // all. Its grammar is documented (`"<height>:<txid>"`), and the whole
    // point of `exact` is that a caller may hold one.
    //
    // THE EXEMPTION, verbatim from `adapters/bitcoin/README.md`: an
    // observation is exempt if, and only if, its `posting` is `pending`, or
    // its `state` is `tombstoned`, or a previous reply in this same
    // connection reported the same `local_id` with `posting: "pending"`.
    // Everything else is section 1 and is not exempt. Re-emitting section 2
    // is REQUIRED behaviour -- it is what turns "this pending transaction
    // was mined into a block below your cursor" into a revision instead of
    // a record that stays pending forever -- so a check without this clause
    // would fail a conforming adapter, which is worse than not checking.
    let seen_pending: BTreeSet<String> = observations
        .iter()
        .filter(|o| o.posting == Posting::Pending)
        .map(|o| o.local_id.clone())
        .collect();
    let middle = confirmed[confirmed.len() / 2];
    let (height, txid) = confirmed_key(middle).expect("a confirmed observation has both");
    let resumed = handle
        .history_read(vec![ResourceQuery {
            resource_id: RESOURCE.to_owned(),
            page: Some(PageRequest::Cursor {
                cursor: format!("{height}:{txid}"),
            }),
        }])
        .await
        .expect("history.read (resumed)");
    for o in &resumed.observations {
        let exempt = o.posting == Posting::Pending
            || o.state == ObservationState::Tombstoned
            || seen_pending.contains(&o.local_id);
        if exempt {
            continue;
        }
        let key = confirmed_key(o).unwrap_or_else(|| panic!("{}: no height/txid", o.local_id));
        assert!(
            key > (height, txid.clone()),
            "{} came back at {key:?}, at or below the cursor ({height}, {txid}), and it is not \
             exempt: it is neither pending nor tombstoned, and it was never reported pending on \
             this connection",
            o.local_id
        );
    }

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
