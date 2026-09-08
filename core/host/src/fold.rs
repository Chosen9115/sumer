//! Revision assignment and the live-set fold (spec/observation.md §3-4).
//!
//! Adapters emit **observations**, never revisions: this module is the one
//! place a monotonically increasing `revision: u64` is assigned, and the
//! one place the fold total order (`received_at, surface, arrival_index`,
//! `surface` bytewise -- [`sumer_wire::fold_order_key`]) turns a stream of
//! observations into a per-`(adapter_id, local_id)` chain and a live set.
//!
//! Public and reusable on purpose (frozen contract, section (g)): the
//! conformance suite calls this module rather than reimplementing
//! resumption/revision assignment, so a real host and the suite's
//! expectations are checked against **one** implementation instead of two
//! that could silently diverge.
//!
//! `sumer refresh` is this module's caller inside the host: it replays one
//! ADAPTER's stored observations into one `Fold` in stored order (the key
//! is `(adapter_id, local_id)`, so a per-resource replay would split a
//! chain the moment a `local_id` moved resource), ingests the sweep's, and
//! takes [`Fold::live_set`] as the base it derives retraction from
//! (`spec/observation.md` §8). Revision assignment
//! therefore has exactly one implementation, exercised both end to end by
//! the CLI and directly by the conformance suite.
//!
//! **Revision order vs. chain order are two different things, on purpose.**
//! `revision` is assigned strictly in the order [`Fold::ingest`] is called
//! (the order the host actually received observations on the wire) -- a
//! restarted or stateless adapter has no other order to give it. The
//! *chain* -- what `chain()` returns -- is sorted by the fold's total
//! order instead, which can reorder two same-key observations from
//! two different surfaces that arrived in the same page (same
//! `received_at`) by `surface` bytewise. So a chain's revision numbers do
//! not have to appear in ascending order down the chain when that happens;
//! both orderings are correct simultaneously, they answer different
//! questions ("when did the host learn this" vs. "what does the fold say
//! happened, once two surfaces are reconciled").

use std::collections::HashMap;

use sumer_wire::{fold_order_key, Observation, ObservationState};

/// One observation, tagged with the host-assigned revision it was given at
/// arrival time.
#[derive(Debug, Clone)]
pub struct RevisionedObservation {
    pub revision: u64,
    pub observation: Observation,
}

struct Entry {
    arrival_index: u64,
    revisioned: RevisionedObservation,
}

/// Accumulates observations from one or more adapters into per-chain
/// history, assigning each a `revision` and folding them into a live set.
///
/// **The key is `(adapter_id, local_id)`, never `local_id` alone.**
/// `local_id` is only a pure function of *one* provider's data, derived by
/// one adapter's own `local_id_derivation` (spec/observation.md §3), so two
/// adapters are as free to both emit `"tx-1"` as two adapters are to both
/// name a resource `"main"` (spec/wire.md §10). Keying on `local_id` alone
/// lets one adapter's observation overwrite another's, and one adapter's
/// tombstone delete another's transaction.
///
/// Within that key, **dedup by `provider_id` alone is forbidden**
/// (spec/observation.md §3): `provider_id` is optional evidence carried on
/// an observation, not an identity a pending-only surface is guaranteed to
/// have yet.
#[derive(Default)]
pub struct Fold {
    chains: HashMap<(String, String), Vec<Entry>>,
    arrival_counter: u64,
}

impl Fold {
    #[must_use]
    pub fn new() -> Fold {
        Fold::default()
    }

    /// Ingests one observation, in the order the host actually received it.
    /// Returns the `revision` assigned to it: the host assigns `revision`
    /// by arrival order per `(adapter_id, local_id)` (spec/observation.md
    /// §3) -- the first observation of a given `local_id` from a given
    /// adapter is revision 1, the next is 2, and so on. That is exactly the
    /// length of its chain, which is why there is no second counter to keep
    /// in step with it.
    pub fn ingest(&mut self, observation: Observation) -> u64 {
        let arrival_index = self.arrival_counter;
        self.arrival_counter += 1;

        let key = (
            observation.provenance.adapter_id.clone(),
            observation.local_id.clone(),
        );
        let chain = self.chains.entry(key).or_default();
        let revision = u64::try_from(chain.len()).unwrap_or(u64::MAX) + 1;
        chain.push(Entry {
            arrival_index,
            revisioned: RevisionedObservation {
                revision,
                observation,
            },
        });
        chain.sort_by(|a, b| {
            let ka = fold_order_key(
                &a.revisioned.observation.provenance.received_at,
                &a.revisioned.observation.surface,
                a.arrival_index,
            );
            let kb = fold_order_key(
                &b.revisioned.observation.provenance.received_at,
                &b.revisioned.observation.surface,
                b.arrival_index,
            );
            ka.cmp(&kb)
        });

        revision
    }

    /// The full, ordered observation chain for one `(adapter_id, local_id)`
    /// (fold total order, ascending), or an empty slice if nothing has been
    /// ingested for it. Retained even once tombstoned -- **tombstone is not
    /// terminal**, so nothing is ever dropped from a chain.
    #[must_use]
    pub fn chain(&self, adapter_id: &str, local_id: &str) -> Vec<&RevisionedObservation> {
        self.chains
            .get(&(adapter_id.to_owned(), local_id.to_owned()))
            .map(|entries| entries.iter().map(|e| &e.revisioned).collect())
            .unwrap_or_default()
    }

    /// The highest `revision` in one chain -- the newest thing the host
    /// has LEARNED about that key, which is not always the chain's head.
    ///
    /// The head is the last entry in the fold's total order; `revision` is
    /// arrival order. The two disagree whenever `received_at` does not
    /// increase with arrival (two surfaces reconciled out of order in one
    /// page, or a wall clock that stepped backwards between refreshes),
    /// and a question asked in arrival order must be answered in arrival
    /// order. Liveness is such a question -- "has anything been learned
    /// since the retraction at revision N?" -- so it asks this, not
    /// `chain().last()`. Asking the head instead lets a backwards clock
    /// step bury a record whose revision can then never grow past the
    /// retraction, permanently.
    #[must_use]
    pub fn highest_revision(&self, adapter_id: &str, local_id: &str) -> Option<u64> {
        self.chains
            .get(&(adapter_id.to_owned(), local_id.to_owned()))
            .and_then(|entries| entries.iter().map(|e| e.revisioned.revision).max())
    }

    /// Every `(adapter_id, local_id)` this fold has ever seen an
    /// observation for.
    pub fn keys(&self) -> impl Iterator<Item = (&str, &str)> {
        self.chains
            .keys()
            .map(|(adapter_id, local_id)| (adapter_id.as_str(), local_id.as_str()))
    }

    /// The live set: for every `(adapter_id, local_id)`, its
    /// most-recent-by-total-order observation, filtered to those currently
    /// `active`. A chain whose latest observation is `tombstoned` is absent
    /// here (it is not currently live) even though its full history remains
    /// in `chain()` -- and a later `active` observation for the same key (a
    /// re-mined transaction) brings it back, since this always looks at the
    /// *latest* entry, never a cached "is it dead" flag.
    #[must_use]
    pub fn live_set(&self) -> HashMap<(&str, &str), &RevisionedObservation> {
        self.chains
            .iter()
            .filter_map(|((adapter_id, local_id), entries)| {
                let latest = entries.last()?;
                (latest.revisioned.observation.state == ObservationState::Active)
                    .then_some(((adapter_id.as_str(), local_id.as_str()), &latest.revisioned))
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use sumer_money::{Amount, AssetId};
    use sumer_wire::{
        Completeness, Posting, Provenance, ProvenanceWire, RawSign, Rfc3339, Staleness,
    };

    fn prov(adapter_id: &str, surface: &str, received_at: &str) -> Provenance {
        let wire = ProvenanceWire {
            adapter_id: adapter_id.to_owned(),
            provider_id: "p".to_owned(),
            surface: surface.to_owned(),
            observed_at: Rfc3339::new(received_at).unwrap(),
            effective_at: None,
            completeness: Completeness::Complete,
        };
        Provenance::stamp(wire, Rfc3339::new(received_at).unwrap(), Staleness::Live)
    }

    fn obs(
        resource_id: &str,
        local_id: &str,
        state: ObservationState,
        amount: &str,
        adapter_id: &str,
        surface: &str,
        received_at: &str,
    ) -> Observation {
        Observation {
            resource_id: resource_id.to_owned(),
            local_id: local_id.to_owned(),
            provider_id: None,
            supersedes_provider_id: None,
            state,
            tombstone_reason: None,
            surface: surface.to_owned(),
            posting: Posting::Posted,
            amount: Amount::parse(AssetId::new("USD").unwrap(), amount).unwrap(),
            fees: None,
            raw_sign: RawSign::ProviderPositive,
            description: "d".to_owned(),
            provider_extra: serde_json::Map::new(),
            provenance: prov(adapter_id, surface, received_at),
        }
    }

    #[test]
    fn revision_assigned_by_arrival_order_per_adapter_and_local_id() {
        let mut fold = Fold::new();
        let r1 = fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "42.00",
            "a1",
            "checking",
            "2026-01-01T00:00:00Z",
        ));
        let r2 = fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "42.37",
            "a1",
            "checking",
            "2026-01-02T00:00:00Z",
        ));
        assert_eq!(r1, 1);
        assert_eq!(r2, 2);
    }

    #[test]
    fn revision_counters_are_independent_per_local_id() {
        let mut fold = Fold::new();
        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "1.00",
            "a1",
            "s",
            "2026-01-01T00:00:00Z",
        ));
        let r = fold.ingest(obs(
            "acct",
            "L2",
            ObservationState::Active,
            "2.00",
            "a1",
            "s",
            "2026-01-01T00:00:00Z",
        ));
        assert_eq!(r, 1, "a different local_id starts its own count at 1");
    }

    #[test]
    fn bank_pending_to_posted_chain_is_retained_in_full() {
        let mut fold = Fold::new();
        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "42.00",
            "a1",
            "checking",
            "2026-01-01T00:00:00Z",
        ));
        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "42.37",
            "a1",
            "checking",
            "2026-01-02T00:00:00Z",
        ));
        let chain = fold.chain("a1", "L1");
        assert_eq!(
            chain.len(),
            2,
            "the full ordered chain, not just the final state"
        );
        assert_eq!(chain[0].revision, 1);
        assert_eq!(chain[1].revision, 2);
        let live = fold.live_set();
        assert_eq!(
            live.get(&("a1", "L1"))
                .unwrap()
                .observation
                .amount
                .to_string(),
            "42.37"
        );
    }

    #[test]
    fn bitcoin_reorg_chain_active_tombstoned_active() {
        let mut fold = Fold::new();
        fold.ingest(obs(
            "wallet",
            "L2",
            ObservationState::Active,
            "1.00",
            "a1",
            "chain",
            "2026-01-01T00:00:00Z",
        ));
        fold.ingest(obs(
            "wallet",
            "L2",
            ObservationState::Tombstoned,
            "1.00",
            "a1",
            "chain",
            "2026-01-02T00:00:00Z",
        ));
        assert!(
            !fold.live_set().contains_key(&("a1", "L2")),
            "tombstoned is not in the live set"
        );
        fold.ingest(obs(
            "wallet",
            "L2",
            ObservationState::Active,
            "1.00",
            "a1",
            "chain",
            "2026-01-03T00:00:00Z",
        ));
        assert!(
            fold.live_set().contains_key(&("a1", "L2")),
            "tombstone is not terminal -- a re-mine revives it"
        );
        assert_eq!(
            fold.chain("a1", "L2").len(),
            3,
            "append-only: nothing was dropped"
        );
    }

    #[test]
    fn dedup_by_provider_id_alone_is_forbidden_by_construction() {
        // Two observations sharing no provider_id (both None here) but the
        // same local_id must still fold into one chain -- local_id is the
        // only key this module ever uses.
        let mut fold = Fold::new();
        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "1.00",
            "a1",
            "s",
            "2026-01-01T00:00:00Z",
        ));
        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "2.00",
            "a1",
            "s",
            "2026-01-02T00:00:00Z",
        ));
        assert_eq!(fold.chain("a1", "L1").len(), 2);
    }

    #[test]
    fn two_adapters_sharing_a_local_id_keep_separate_chains() {
        // `local_id` is only unique within one adapter (its derivation is
        // per-adapter, spec/observation.md 3). Two adapters that both call
        // a transaction "L1" must not collide: B must not overwrite A's
        // amount, and a B-side tombstone must not delete A's transaction.
        let mut fold = Fold::new();
        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "1000.00",
            "A",
            "s",
            "2026-01-01T00:00:00Z",
        ));
        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "2000.00",
            "B",
            "s",
            "2026-01-02T00:00:00Z",
        ));
        assert_eq!(
            fold.live_set().len(),
            2,
            "one live entry per (adapter_id, local_id), not per local_id"
        );

        fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Tombstoned,
            "2000.00",
            "B",
            "s",
            "2026-01-03T00:00:00Z",
        ));
        assert_eq!(
            fold.live_set().len(),
            1,
            "B's tombstone must not delete A's transaction"
        );
    }

    #[test]
    fn same_page_two_surfaces_orders_by_surface_bytewise_not_arrival() {
        let mut fold = Fold::new();
        // Ingested "B" first (arrival order), but total order sorts by
        // surface bytewise after received_at ties, so "A" must land first
        // in the chain despite arriving second.
        let r_b = fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "1.00",
            "a1",
            "B",
            "2026-01-01T00:00:00Z",
        ));
        let r_a = fold.ingest(obs(
            "acct",
            "L1",
            ObservationState::Active,
            "2.00",
            "a1",
            "A",
            "2026-01-01T00:00:00Z",
        ));
        assert_eq!(r_b, 1, "revision reflects arrival order");
        assert_eq!(r_a, 2);
        let chain = fold.chain("a1", "L1");
        assert_eq!(
            chain[0].observation.surface, "A",
            "fold order sorts by surface bytewise"
        );
        assert_eq!(chain[1].observation.surface, "B");
        assert_eq!(
            chain[0].revision, 2,
            "chain position and revision can diverge"
        );
    }
}
