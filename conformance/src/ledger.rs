//! The evidence ledger: what one execution actually emitted, per resource,
//! as an **ordered sequence** -- and how that is compared against the
//! sequence a fixture declared.
//!
//! # Why a sequence and not a set
//!
//! The design this replaces compared the fold's live set against a
//! *membership* list: "every `local_id` the fixture names is present, and
//! the counts match." Seven assertions a deliberately broken adapter
//! satisfied were traced to that shape. Membership cannot see:
//!
//! * **duplicate collapse** -- an adapter that emits one observation twice
//!   and one not at all lands on the same set, and (once the fold
//!   deduplicates by `local_id`) on the same count;
//! * **emission order** -- pending-before-posted, tombstone-before-remine,
//!   the resume boundary: every one of them is an ordering claim, and a set
//!   has no order to contradict;
//! * **orphans in the other direction** -- a per-entry loop over the
//!   fixture's list only ever asks "did the declared thing arrive", never
//!   "did anything else".
//!
//! Sequence equality subsumes membership in both directions at once, and
//! it is the only comparison that can be *made* to fail by reordering.
//!
//! There is no membership language left in this crate. If it comes back,
//! so do the seven.

use crate::assert::{brief, diff_observation, expect_array, expect_object, expect_str, Failure};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Which of a resource's two independent sequences an entry belongs to.
///
/// Balances and history are **separate sequences**, never one pooled list:
/// they arrive from different ops, a balance line has no `local_id`, and
/// interleaving them would make the order of either unassertable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ObsKind {
    Balances,
    History,
}

impl ObsKind {
    /// The key a fixture spells this sequence under, inside
    /// `expect.ledger.<resource_id>`.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            ObsKind::Balances => "balances",
            ObsKind::History => "history",
        }
    }

    /// Which named assertion a mismatch in this sequence is evidence
    /// against: A1 is exact-value fidelity on the balances list, A2 is the
    /// history the fold is built from.
    #[must_use]
    pub fn assertion(self) -> &'static str {
        match self {
            ObsKind::Balances => "A1",
            ObsKind::History => "A2",
        }
    }
}

/// Which sequence is being compared, and under what label -- bundled so the
/// comparison below stays under clippy's argument-count lint.
pub struct SequenceRef<'a> {
    pub label: &'a str,
    pub resource_id: &'a str,
    pub kind: ObsKind,
}

/// One observation as it was emitted, in the form two executions can be
/// held to: the host's typed decode with the unpredictable `received_at`
/// removed (see [`crate::assert::stable_view`]).
pub struct Entry {
    /// `Some` for a history observation, `None` for a balance line (which
    /// has no `local_id` -- a balance is a provider-named category, not a
    /// record).
    pub local_id: Option<String>,
    pub json: Value,
}

/// Everything one execution emitted, keyed by `(resource_id, kind)` and
/// ordered within each key.
///
/// The order is **arrival order within a serially dispatched crawl**, which
/// is the same thing as the transcript's dispatch order -- see the
/// serial-dispatch note on [`crate::exec::run_crawl`]. Nothing here is
/// sorted: sorting would destroy the one property this type exists to
/// assert.
#[derive(Default)]
pub struct Ledger {
    seqs: BTreeMap<(String, ObsKind), Vec<Entry>>,
    /// How many reply frames each `(resource, kind)` sequence was drained
    /// from -- page count, which A5 bounds after a resume.
    frames: BTreeMap<(String, ObsKind), u64>,
}

impl Ledger {
    pub fn push(
        &mut self,
        kind: ObsKind,
        resource_id: &str,
        local_id: Option<String>,
        json: Value,
    ) {
        self.seqs
            .entry((resource_id.to_owned(), kind))
            .or_default()
            .push(Entry { local_id, json });
    }

    /// Records that one more reply frame was received for this sequence,
    /// whether or not it carried any observation. An empty page is still a
    /// page.
    pub fn count_frame(&mut self, kind: ObsKind, resource_id: &str) {
        *self
            .frames
            .entry((resource_id.to_owned(), kind))
            .or_insert(0) += 1;
    }

    #[must_use]
    pub fn frames(&self, kind: ObsKind, resource_id: &str) -> u64 {
        self.frames
            .get(&(resource_id.to_owned(), kind))
            .copied()
            .unwrap_or(0)
    }

    #[must_use]
    pub fn sequence(&self, kind: ObsKind, resource_id: &str) -> &[Entry] {
        self.seqs
            .get(&(resource_id.to_owned(), kind))
            .map_or(&[], Vec::as_slice)
    }

    /// Every `(resource_id, kind)` this execution emitted anything under.
    #[must_use]
    pub fn keys(&self) -> BTreeSet<(String, ObsKind)> {
        self.seqs.keys().cloned().collect()
    }

    /// Every `local_id` that appeared anywhere in this execution, in any
    /// sequence -- what "must never appear" is checked against.
    #[must_use]
    pub fn local_ids(&self) -> BTreeSet<&str> {
        self.seqs
            .values()
            .flatten()
            .filter_map(|e| e.local_id.as_deref())
            .collect()
    }

    /// The whole `local_id` -> emitted-records association, in arrival
    /// order, across every resource: what A9 compares between two
    /// executions. Records later superseded or tombstoned are kept --
    /// purity is a claim about every record the derivation touches, not
    /// only the survivors.
    #[must_use]
    pub fn history_by_local_id(&self) -> BTreeMap<String, Value> {
        let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for ((_, kind), entries) in &self.seqs {
            if *kind != ObsKind::History {
                continue;
            }
            for entry in entries {
                if let Some(local_id) = &entry.local_id {
                    out.entry(local_id.clone())
                        .or_default()
                        .push(entry.json.clone());
                }
            }
        }
        out.into_iter().map(|(k, v)| (k, Value::Array(v))).collect()
    }
}

/// How much of a declared sequence one execution was obliged to emit.
///
/// **This is a driver constant and must never become a fixture key.** A
/// fixture is an adapter-adjacent file; letting one declare its own
/// leniency is letting the thing under test decide how hard the test is.
/// Every [`Mode::Truncated`](crate::exec::Mode) in this crate is written
/// down at a call site in `case_interrupted_pagination`, where the runner
/// -- not the fixture -- knows the execution was cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// The emitted sequence must EQUAL the declared sequence.
    Complete,
    /// The emitted sequence must equal a CONTIGUOUS SLICE of the declared
    /// sequence: a killed run yields a prefix, an `exact` resume a suffix,
    /// a `batch_restart` resume the whole thing.
    ///
    /// This relaxes exactly one half of the assertion -- "everything
    /// declared was emitted". Content mismatch, orphans (more emitted than
    /// declared), and omitted-never-appears stay fully armed.
    Truncated,
}

/// One resource's declared sequences, with the omission flags resolved.
#[derive(Default)]
pub struct Declared {
    /// Declared sequences by `(resource_id, kind)`, with every entry
    /// flagged `"omitted": true` removed.
    pub seqs: BTreeMap<(String, ObsKind), Vec<Value>>,
    /// Every `local_id` flagged `"omitted": true`: removed from the
    /// sequence above, and separately asserted never to appear anywhere.
    pub omitted: BTreeSet<String>,
}

/// Parses `expect.ledger`:
///
/// ```json
/// { "<resource_id>": { "balances": [ {...}, ... ], "history": [ {...}, ... ] } }
/// ```
///
/// Each entry is a subset of the observation as the adapter emitted it.
/// An entry carrying `"omitted": true` names a record the adapter was
/// required to drop entirely (spec/observation.md §6 step 2): it is taken
/// out of the sequence and moved to [`Declared::omitted`].
///
/// **An absent key is an empty sequence, never an unchecked one.** Nothing
/// is inserted here for a `balances`/`history` a fixture does not spell,
/// and `assert_execution` compares the *union* of the declared keys and
/// the emitted ones -- so an undeclared sequence is compared against a
/// declared length of zero. A fixture cannot opt out of a check by leaving
/// a key off.
///
/// # Known limit: an `omitted` entry cannot assert its POSITION
///
/// An `"omitted": true` entry is lifted out of the sequence here, so the
/// sequence closes over the gap. What survives is the absence itself (that
/// `local_id` must appear nowhere in the execution) and the relative order
/// of everything around it. What is *not* assertable is where the hole
/// was: "the sibling arrived AFTER the record that had to be dropped" is
/// indistinguishable from "it arrived before". Positional-absence
/// machinery for one hypothetical case is not worth its weight; this is
/// written down instead of built.
#[must_use]
pub fn parse_declared(ledger: &Value, label: &str, failures: &mut Vec<Failure>) -> Declared {
    let mut declared = Declared::default();
    let Some(by_resource) = expect_object(ledger, &format!("{label}: expect.ledger"), failures)
    else {
        return declared;
    };
    for (resource_id, kinds) in by_resource {
        let what = format!("{label}: expect.ledger.{resource_id}");
        let Some(kinds) = expect_object(kinds, &what, failures) else {
            continue;
        };
        for (kind_key, entries) in kinds {
            let kind = match kind_key.as_str() {
                "balances" => ObsKind::Balances,
                "history" => ObsKind::History,
                other => {
                    failures.push(Failure::new(
                        "setup",
                        format!(
                            "{what} names sequence {other:?}; the only two sequences are \
                             \"balances\" and \"history\""
                        ),
                    ));
                    continue;
                }
            };
            let Some(entries) = expect_array(entries, &format!("{what}.{kind_key}"), failures)
            else {
                continue;
            };
            let mut kept = Vec::new();
            for entry in entries {
                let omitted = entry
                    .get("omitted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut entry = entry.clone();
                if let Value::Object(map) = &mut entry {
                    map.remove("omitted");
                }
                if omitted {
                    match entry.get("local_id").and_then(|v| {
                        expect_str(v, &format!("{what}.{kind_key}[].local_id"), failures)
                    }) {
                        Some(local_id) => {
                            declared.omitted.insert(local_id.to_owned());
                        }
                        None => failures.push(Failure::new(
                            "setup",
                            format!(
                                "{what}.{kind_key}: an entry is flagged \"omitted\" but names no \
                                 local_id, so nothing can be checked for its absence"
                            ),
                        )),
                    }
                } else {
                    kept.push(entry);
                }
            }
            declared.seqs.insert((resource_id.clone(), kind), kept);
        }
    }
    declared
}

/// Sequence equality (or, under [`Completeness::Truncated`], contiguous-slice
/// equality) for one `(resource_id, kind)`.
///
/// Every element is compared with [`diff_observation`]: a subset of the
/// fields the fixture names, with `provider_extra` compared **exactly** and
/// `amount` compared via `Amount::cmp_same_asset`.
pub fn compare_sequence(
    seq: &SequenceRef<'_>,
    declared: &[Value],
    emitted: &[Entry],
    completeness: Completeness,
    failures: &mut Vec<Failure>,
) {
    let assertion = seq.kind.assertion();
    let where_ = format!("{}: {}/{}", seq.label, seq.resource_id, seq.kind.key());

    if emitted.len() > declared.len() {
        failures.push(Failure::new(
            assertion,
            format!(
                "{where_}: {} observation(s) emitted, only {} declared -- the extra one(s) are \
                 orphans the fixture never named: {:?}",
                emitted.len(),
                declared.len(),
                emitted
                    .iter()
                    .skip(declared.len())
                    .map(|e| brief(&e.json))
                    .collect::<Vec<_>>()
            ),
        ));
        return;
    }
    if completeness == Completeness::Complete && emitted.len() != declared.len() {
        failures.push(Failure::new(
            assertion,
            format!(
                "{where_}: {} observation(s) emitted, {} declared -- the sequence must be EQUAL, \
                 not a subset. Emitted: {:?}",
                emitted.len(),
                declared.len(),
                emitted.iter().map(|e| brief(&e.json)).collect::<Vec<_>>()
            ),
        ));
        return;
    }

    // Which alignment to judge: offset 0 for an execution that had to emit
    // everything; for a truncated one, whichever contiguous window fits
    // best -- so a genuine content mismatch is still reported (against its
    // closest alignment) rather than excused by sliding somewhere else.
    let offsets: Vec<usize> = match completeness {
        Completeness::Complete => vec![0],
        Completeness::Truncated => (0..=(declared.len() - emitted.len())).collect(),
    };
    let Some((offset, diffs)) = offsets
        .into_iter()
        .map(|offset| (offset, align_diffs(&where_, declared, emitted, offset)))
        .min_by_key(|(_, diffs)| diffs.len())
    else {
        return;
    };
    if diffs.is_empty() {
        return;
    }
    let sliced = completeness == Completeness::Truncated;
    for diff in diffs {
        failures.push(Failure::new(
            assertion,
            if sliced {
                format!("{diff} (best-matching contiguous slice starts at declared index {offset})")
            } else {
                diff
            },
        ));
    }
}

fn align_diffs(where_: &str, declared: &[Value], emitted: &[Entry], offset: usize) -> Vec<String> {
    let mut diffs = Vec::new();
    for (i, entry) in emitted.iter().enumerate() {
        let Some(expected) = declared.get(offset + i) else {
            continue;
        };
        diff_observation(
            &entry.json,
            expected,
            &format!("{where_}[{}]", offset + i),
            &mut diffs,
        );
    }
    diffs
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(local_id: &str, amount: &str) -> Entry {
        Entry {
            local_id: Some(local_id.to_owned()),
            json: serde_json::json!({
                "local_id": local_id,
                "amount": {"asset": "USD", "amount": amount},
            }),
        }
    }

    fn declared_entry(local_id: &str, amount: &str) -> Value {
        serde_json::json!({
            "local_id": local_id,
            "amount": {"asset": "USD", "amount": amount},
        })
    }

    fn run(declared: &[Value], emitted: &[Entry], completeness: Completeness) -> Vec<Failure> {
        let mut failures = Vec::new();
        compare_sequence(
            &SequenceRef {
                label: "case",
                resource_id: "r1",
                kind: ObsKind::History,
            },
            declared,
            emitted,
            completeness,
            &mut failures,
        );
        failures
    }

    #[test]
    fn equal_sequences_pass() {
        let declared = vec![declared_entry("a", "1.00"), declared_entry("b", "2.00")];
        let emitted = vec![entry("a", "1.00"), entry("b", "2.000")];
        assert!(run(&declared, &emitted, Completeness::Complete).is_empty());
    }

    /// The hole membership could not see: the same set, the same count,
    /// the wrong order.
    #[test]
    fn reordering_fails() {
        let declared = vec![declared_entry("a", "1.00"), declared_entry("b", "2.00")];
        let emitted = vec![entry("b", "2.00"), entry("a", "1.00")];
        assert!(!run(&declared, &emitted, Completeness::Complete).is_empty());
    }

    /// The other hole: one record emitted twice and one not at all folds
    /// to an identical live set.
    #[test]
    fn duplicate_collapse_fails() {
        let declared = vec![declared_entry("a", "1.00"), declared_entry("b", "2.00")];
        let emitted = vec![entry("a", "1.00"), entry("a", "1.00")];
        assert!(!run(&declared, &emitted, Completeness::Complete).is_empty());
    }

    #[test]
    fn truncated_accepts_a_prefix_and_a_suffix_but_not_a_gap() {
        let declared = vec![
            declared_entry("a", "1.00"),
            declared_entry("b", "2.00"),
            declared_entry("c", "3.00"),
        ];
        assert!(run(
            &declared,
            &[entry("a", "1.00"), entry("b", "2.00")],
            Completeness::Truncated
        )
        .is_empty());
        assert!(run(
            &declared,
            &[entry("b", "2.00"), entry("c", "3.00")],
            Completeness::Truncated
        )
        .is_empty());
        // a then c: contiguous nowhere.
        assert!(!run(
            &declared,
            &[entry("a", "1.00"), entry("c", "3.00")],
            Completeness::Truncated
        )
        .is_empty());
    }

    /// Truncated relaxes only "everything declared was emitted". Wrong
    /// money inside the slice, and orphans past the end, still fail.
    #[test]
    fn truncated_still_catches_content_and_orphans() {
        let declared = vec![declared_entry("a", "1.00"), declared_entry("b", "2.00")];
        assert!(!run(&declared, &[entry("a", "9999.00")], Completeness::Truncated).is_empty());
        assert!(!run(
            &declared,
            &[entry("a", "1.00"), entry("b", "2.00"), entry("c", "3.00")],
            Completeness::Truncated
        )
        .is_empty());
    }

    #[test]
    fn an_omitted_entry_leaves_the_sequence_and_is_remembered() {
        let mut failures = Vec::new();
        let declared = parse_declared(
            &serde_json::json!({
                "r1": {"history": [
                    {"local_id": "a"},
                    {"local_id": "too-big", "omitted": true},
                    {"local_id": "b"}
                ]}
            }),
            "case",
            &mut failures,
        );
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(declared.seqs[&("r1".to_owned(), ObsKind::History)].len(), 2);
        assert!(declared.omitted.contains("too-big"));
    }
}
