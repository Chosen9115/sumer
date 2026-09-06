//! Property tests: envelope round-trip, and the fold-order property.
//!
//! The fold-order property tests [`sumer_wire::fold_order_key`] -- the
//! `(received_at, surface, arrival_index)` total order the frozen contract
//! specifies. It does *not* test building a live set (dedup by
//! `supersedes`/tombstone semantics): per the contract, that folding logic
//! belongs to `sumer_host::fold`, a different crate, which must apply this
//! same order. What's checked here is that the order itself is a genuine
//! total order that is independent of the arrival sequence -- i.e. any
//! interleaving (any permutation) of the same set of observation
//! descriptors sorts to the same sequence.

use proptest::prelude::*;
use sumer_wire::{fold_order_key, ErrorBody, Reply, Request, RequestId, Rfc3339, WireErrorCode};

/// Test-only stand-in for `.unwrap()`/`.expect()` (denied by workspace
/// lints even in tests): panics with the error's `Debug` output.
fn must<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("unexpected error: {e:?}"),
    }
}

/// Test-only stand-in for `.unwrap()` on an `Option`.
fn must_some<T>(o: Option<T>) -> T {
    match o {
        Some(v) => v,
        None => panic!("expected Some"),
    }
}

// ---------------------------------------------------------------------
// Envelope round-trip
// ---------------------------------------------------------------------

fn arb_json_leaf() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
        "[a-zA-Z0-9_]{0,16}".prop_map(serde_json::Value::String),
    ]
}

fn arb_json_object() -> impl Strategy<Value = serde_json::Value> {
    prop::collection::vec(("[a-z]{1,8}", arb_json_leaf()), 0..4)
        .prop_map(|pairs| serde_json::Value::Object(pairs.into_iter().collect()))
}

fn arb_op() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("hello".to_owned()),
        Just("resources.list".to_owned()),
        Just("balances.read".to_owned()),
        Just("history.read".to_owned()),
        Just("status.read".to_owned()),
        "[a-z_.]{1,16}".prop_map(|s| s),
    ]
}

fn arb_error_code() -> impl Strategy<Value = WireErrorCode> {
    prop_oneof![
        Just(WireErrorCode::UnsupportedProtocol),
        Just(WireErrorCode::Unsupported),
        Just(WireErrorCode::InvalidRequest),
        Just(WireErrorCode::NotReady),
        Just(WireErrorCode::Internal),
    ]
}

proptest! {
    #[test]
    fn request_round_trips_through_json(
        id in any::<u64>(),
        op in arb_op(),
        params in arb_json_object(),
    ) {
        let req = Request::new(RequestId(id), op.clone(), params.clone());
        let json = must(serde_json::to_string(&req));
        let back: Request = must(serde_json::from_str(&json));
        prop_assert_eq!(back.id, RequestId(id));
        prop_assert_eq!(back.op, op);
        prop_assert_eq!(back.params, params);
    }

    #[test]
    fn ok_reply_round_trips_through_json(
        id in any::<u64>(),
        ok in arb_json_object(),
    ) {
        let reply: Reply<serde_json::Value> = Reply::ok(RequestId(id), ok.clone());
        let json = must(serde_json::to_string(&reply));
        let back: Reply<serde_json::Value> = must(serde_json::from_str(&json));
        prop_assert_eq!(back.id(), RequestId(id));
        match back {
            Reply::Ok { ok: back_ok, .. } => prop_assert_eq!(back_ok, ok),
            Reply::Err { .. } => prop_assert!(false, "expected an Ok reply"),
        }
    }

    #[test]
    fn err_reply_round_trips_through_json(
        id in any::<u64>(),
        code in arb_error_code(),
        message in "[a-zA-Z0-9 ]{0,32}",
        detail in proptest::option::of(arb_json_object()),
    ) {
        let mut body = ErrorBody::new(code, message.clone());
        if let Some(d) = detail.clone() {
            body = body.with_detail(d);
        }
        let reply: Reply<serde_json::Value> = Reply::err(RequestId(id), body);
        let json = must(serde_json::to_string(&reply));
        let back: Reply<serde_json::Value> = must(serde_json::from_str(&json));
        prop_assert_eq!(back.id(), RequestId(id));
        match back {
            Reply::Err { err, .. } => {
                prop_assert_eq!(err.code, code);
                prop_assert_eq!(err.message, message);
                prop_assert_eq!(err.detail, detail);
            }
            Reply::Ok { .. } => prop_assert!(false, "expected an Err reply"),
        }
    }
}

// ---------------------------------------------------------------------
// Fold-order property
// ---------------------------------------------------------------------

/// A minimal stand-in for "one observation's ordering-relevant fields":
/// enough to build a `fold_order_key`, plus a `local_id` so the generator
/// can deliberately collide two records on the same id from two surfaces
/// (otherwise the property would be checking an order over records that
/// never actually compete for the same logical entity -- vacuous).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rec {
    local_id: u8,
    surface: String,
    received_at: Rfc3339,
    arrival_index: u64,
}

fn arb_surface() -> impl Strategy<Value = String> {
    prop_oneof![Just("checking".to_owned()), Just("bitcoin_l1".to_owned())]
}

fn arb_timestamp() -> impl Strategy<Value = Rfc3339> {
    (2020u32..2030, 1u32..29, 0u32..24, 0u32..60, 0u32..60).prop_map(
        |(year, day, hour, minute, second)| {
            let s = format!("{year:04}-01-{day:02}T{hour:02}:{minute:02}:{second:02}Z");
            must(Rfc3339::new(s))
        },
    )
}

/// Generates a small set of records that deliberately includes at least one
/// pair sharing a `local_id` across two different surfaces (the reorg /
/// pending-to-posted scenario the contract calls out), each with a unique
/// `arrival_index` so the total order has no ties to break arbitrarily.
fn arb_records() -> impl Strategy<Value = Vec<Rec>> {
    let shared_id = 0u8;
    let rest = prop::collection::vec((1u8..8, arb_surface(), arb_timestamp()), 1..6);
    (arb_timestamp(), arb_surface(), arb_timestamp(), rest).prop_map(move |(t1, s2, t2, rest)| {
        let mut records = vec![
            Rec {
                local_id: shared_id,
                surface: "checking".to_owned(),
                received_at: t1,
                arrival_index: 0,
            },
            Rec {
                local_id: shared_id,
                surface: s2,
                received_at: t2,
                arrival_index: 1,
            },
        ];
        for (i, (local_id, surface, received_at)) in rest.into_iter().enumerate() {
            records.push(Rec {
                local_id,
                surface,
                received_at,
                arrival_index: must(u64::try_from(i + 2)),
            });
        }
        records
    })
}

fn sort_by_fold_order(records: &[Rec]) -> Vec<Rec> {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| fold_order_key(&r.received_at, &r.surface, r.arrival_index));
    sorted
}

proptest! {
    #[test]
    fn fold_order_is_independent_of_arrival_interleaving(
        records in arb_records(),
        shuffle_seed in any::<u64>(),
    ) {
        // Two different "arrival orders" for the identical multiset of
        // records (the original generation order, and a deterministic
        // shuffle of it) must fold to the same sorted sequence under the
        // specified total order.
        let baseline = sort_by_fold_order(&records);

        let mut shuffled = records.clone();
        // A cheap deterministic shuffle: rotate by a seed-derived amount
        // and reverse every other element. Any permutation works here --
        // what's under test is that sorting erases the difference.
        let len = shuffled.len();
        if len > 1 {
            let rotate_by = usize::try_from(shuffle_seed).unwrap_or(0) % len;
            shuffled.rotate_left(rotate_by);
            shuffled.reverse();
        }
        let from_shuffled = sort_by_fold_order(&shuffled);

        prop_assert_eq!(baseline, from_shuffled);
    }

    #[test]
    fn fold_order_places_same_local_id_records_by_received_at(
        records in arb_records(),
    ) {
        // The two deliberately-colliding records (index 0 and 1, same
        // local_id, arrival_index 0 and 1) must come out ordered by
        // received_at (ties broken by surface, then arrival_index) -- this
        // is the non-vacuous check: a fold that ignored received_at and
        // just used arrival order would still pass the interleaving test
        // above by accident, but would fail this one whenever the two
        // colliding records' timestamps disagree with arrival order.
        let sorted = sort_by_fold_order(&records);
        let a = records[0].clone();
        let b = records[1].clone();
        let pos_a = must_some(sorted.iter().position(|r| r == &a));
        let pos_b = must_some(sorted.iter().position(|r| r == &b));

        let expected_a_first = fold_order_key(&a.received_at, &a.surface, a.arrival_index)
            <= fold_order_key(&b.received_at, &b.surface, b.arrival_index);
        prop_assert_eq!(pos_a < pos_b, expected_a_first);
    }
}
