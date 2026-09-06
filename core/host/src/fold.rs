//! Revision assignment and the live-set fold (spec/observation.md §3-4).
//!
//! Adapters emit **observations**, never revisions: this module is the one
//! place a monotonically increasing `revision: u64` is assigned, and the
//! one place the fold total order (`received_at, surface, arrival_index`,
//! `surface` bytewise -- [`sumer_wire::fold_order_key`]) turns a stream of
//! observations into a per-`local_id` chain and a live set.
//!
//! Public and reusable on purpose (frozen contract, section (g)): the
//! conformance suite calls this module rather than reimplementing
//! resumption/revision assignment, so a real host and the suite's
//! expectations are checked against **one** implementation instead of two
//! that could silently diverge.
//!
//! **Revision order vs. chain order are two different things, on purpose.**
//! `revision` is assigned strictly in the order [`Fold::ingest`] is called
//! (the order the host actually received observations on the wire) -- a
//! restarted or stateless adapter has no other order to give it. The
//! *chain* -- what `chain()` returns -- is sorted by the fold's total
//! order instead, which can reorder two same-`local_id` observations from
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

/// Accumulates observations from one or more adapters into per-`local_id`
/// chains, assigning each a `revision` and folding them into a live set.
///
/// `local_id` alone is the fold key, deliberately -- **dedup by
/// `provider_id` alone is forbidden** (spec/observation.md §3):
/// `provider_id` is optional evidence carried on an observation, not an
/// identity a pending-only surface is guaranteed to have yet.
#[derive(Default)]
pub struct Fold {
    chains: HashMap<String, Vec<Entry>>,
    revisions: HashMap<(String, String), u64>,
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
    /// adapter is revision 1, the next is 2, and so on.
    pub fn ingest(&mut self, observation: Observation) -> u64 {
        let key = (
            observation.provenance.adapter_id.clone(),
            observation.local_id.clone(),
        );
        let revision = self.revisions.entry(key).or_insert(0);
        *revision += 1;
        let revision = *revision;

        let arrival_index = self.arrival_counter;
        self.arrival_counter += 1;

        let chain = self.chains.entry(observation.local_id.clone()).or_default();
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

    /// The full, ordered observation chain for one `local_id` (fold total
    /// order, ascending), or an empty slice if nothing has been ingested
    /// for it. Retained even once tombstoned -- **tombstone is not
    /// terminal**, so nothing is ever dropped from a chain.
    #[must_use]
    pub fn chain(&self, local_id: &str) -> Vec<&RevisionedObservation> {
        self.chains
            .get(local_id)
            .map(|entries| entries.iter().map(|e| &e.revisioned).collect())
            .unwrap_or_default()
    }

    /// Every `local_id` this fold has ever seen an observation for.
    pub fn local_ids(&self) -> impl Iterator<Item = &str> {
        self.chains.keys().map(String::as_str)
    }

    /// The live set: for every `local_id`, its most-recent-by-total-order
    /// observation, filtered to those currently `active`. A `local_id`
    /// whose latest observation is `tombstoned` is absent here (it is not
    /// currently live) even though its full history remains in `chain()` --
    /// and a later `active` observation for the same `local_id` (a
    /// re-mined transaction) brings it back, since this always looks at
    /// the *latest* entry, never a cached "is it dead" flag.
    #[must_use]
    pub fn live_set(&self) -> HashMap<&str, &RevisionedObservation> {
        self.chains
            .iter()
            .filter_map(|(local_id, entries)| {
                let latest = entries.last()?;
                (latest.revisioned.observation.state == ObservationState::Active)
                    .then_some((local_id.as_str(), &latest.revisioned))
            })
            .collect()
    }

    /// The most-recent-by-total-order observation for one `local_id`,
    /// regardless of its state (active or tombstoned).
    #[must_use]
    pub fn current(&self, local_id: &str) -> Option<&RevisionedObservation> {
        self.chains
            .get(local_id)
            .and_then(|entries| entries.last())
            .map(|e| &e.revisioned)
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
        let chain = fold.chain("L1");
        assert_eq!(
            chain.len(),
            2,
            "the full ordered chain, not just the final state"
        );
        assert_eq!(chain[0].revision, 1);
        assert_eq!(chain[1].revision, 2);
        let live = fold.live_set();
        assert_eq!(
            live.get("L1").unwrap().observation.amount.to_string(),
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
            !fold.live_set().contains_key("L2"),
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
            fold.live_set().contains_key("L2"),
            "tombstone is not terminal -- a re-mine revives it"
        );
        assert_eq!(
            fold.chain("L2").len(),
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
        assert_eq!(fold.chain("L1").len(), 2);
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
        let chain = fold.chain("L1");
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
