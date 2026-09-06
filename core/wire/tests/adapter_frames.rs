//! Deserialization proof: real frames captured from `adapters/fake/fake_adapter.py`
//! (SUMER_FIXTURE=conformance/cases/large_amounts.json), fed through the actual
//! Rust wire types. This is the seam Contract Amendment 1 exists to close --
//! if the Python adapter's op params/reply shapes and this crate's types have
//! drifted, these `serde_json::from_str` calls fail.
//!
//! Frames captured by hand-driving the adapter:
//!   hello                          -> HelloReply
//!   resources.list  params {}      -> ResourcesListReply
//!   balances.read   {"resource_ids":[...]} -> BalancesReadReply
//!   status.read     {"resource_ids":[...]} -> StatusReadReply

use sumer_wire::{
    BalancesReadReply, CanonicalHint, HelloReply, HistoryReadReply, ReadOutcome, Reply,
    ResourcesListReply, StatusReadReply,
};

fn must<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("unexpected error: {e:?}"),
    }
}

fn must_some<T>(o: Option<T>) -> T {
    match o {
        Some(v) => v,
        None => panic!("expected Some"),
    }
}

const HELLO_FRAME: &str = r#"{"id":0,"ok":{"protocol":"1","adapter_id":"fake-adapter","adapter_version":"0.1.0","capabilities":["resources.list","balances.read","history.read","status.read"],"local_id_derivation":"fixture-literal@1","max_in_flight":1}}"#;

const RESOURCES_LIST_FRAME: &str = r#"{"id":1,"ok":{"resources":[{"resource_id":"wallet-eth","provider_id":"0xEXAMPLE000000000000000000000000000WALLET","kind":"onchain_wallet","label":"Example ETH wallet (fixture placeholder)"},{"resource_id":"wallet-btc","provider_id":"bc1qexamplefixtureplaceholderaddress","kind":"onchain_wallet","label":"Example BTC wallet (fixture placeholder)"}]}}"#;

const BALANCES_READ_FRAME: &str = r#"{"id":2,"ok":{"observations":[{"resource_id":"wallet-eth","category":"wei_balance","canonical_hint":null,"amount":{"asset":"eth-wei","amount":"115792089237316195423570985008687907853269984665640564039457584007913129639935"},"provenance":{"adapter_id":"fake-adapter","provider_id":"0xEXAMPLE000000000000000000000000000WALLET","surface":"onchain","observed_at":"2026-09-01T00:00:00Z","completeness":"complete"}},{"resource_id":"wallet-eth","category":"eth_balance","canonical_hint":"available","amount":{"asset":"ETH","amount":"1000000.123456789012345678"},"provenance":{"adapter_id":"fake-adapter","provider_id":"0xEXAMPLE000000000000000000000000000WALLET","surface":"onchain","observed_at":"2026-09-01T00:00:00Z","completeness":"complete"}},{"resource_id":"wallet-btc","category":"satoshi_balance","canonical_hint":"available","amount":{"asset":"sat","amount":"2100000000000000"},"provenance":{"adapter_id":"fake-adapter","provider_id":"bc1qexamplefixtureplaceholderaddress","surface":"onchain","observed_at":"2026-09-01T00:00:00Z","completeness":"complete"}}],"statuses":[{"resource_id":"wallet-eth","outcome":{"fetched":{"page_empty":false}}},{"resource_id":"wallet-btc","outcome":{"fetched":{"page_empty":false}}}]}}"#;

const STATUS_READ_FRAME: &str = r#"{"id":3,"ok":{"statuses":[{"resource_id":"wallet-eth","outcome":{"fetched":{"page_empty":false}}},{"resource_id":"wallet-btc","outcome":{"fetched":{"page_empty":false}}}]}}"#;

// Captured from SUMER_FIXTURE=conformance/cases/oversized_observation.json,
// a history.read reply -- exercises the A10 degrade path (truncated sibling
// present, oversized sibling omitted with `oversized_observation` in
// `statuses`) and the per-resource `page` nested inside `statuses[i]`
// (Ruling A3/A4: history.read is batched, so paging state lives per
// resource, never as a reply-wide sibling of `observations`/`statuses`).
const HISTORY_READ_OVERSIZED_FRAME: &str = r#"{"id":1,"ok":{"observations":[{"resource_id":"wallet-oversized","local_id":"wallet-oversized:sibling-small","provider_id":"sibling-small","state":"active","surface":"onchain","posting":"posted","amount":{"asset":"sat","amount":"500"},"raw_sign":"provider_positive","description":"SMALL SIBLING EVENT, UNTRUNCATED (fixture placeholder)","provider_extra":{"memo":"small and ordinary"},"provenance":{"adapter_id":"fake-adapter","provider_id":"sibling-small","surface":"onchain","observed_at":"2026-09-01T10:00:00Z","completeness":"complete"}},{"resource_id":"wallet-oversized","local_id":"wallet-oversized:big-truncatable","provider_id":"big-truncatable","state":"active","surface":"onchain","posting":"posted","amount":{"asset":"sat","amount":"700000"},"raw_sign":"provider_positive","description":"EVENT WHOSE provider_extra WAS TOO LARGE AND HAS BEEN TRUNCATED (fixture placeholder)","provider_extra":{"_truncated":true,"_original_bytes":120000},"provenance":{"adapter_id":"fake-adapter","provider_id":"big-truncatable","surface":"onchain","observed_at":"2026-09-01T10:05:00Z","completeness":"partial"}}],"statuses":[{"resource_id":"wallet-oversized","outcome":{"oversized_observation":{"local_id":"wallet-oversized:big-untruncatable","bytes":260000}},"page":{"cursor_resumable":"exact","next":null}}]}}"#;

#[test]
fn hello_frame_from_real_adapter_deserializes() {
    let reply: Reply<HelloReply> = must(serde_json::from_str(HELLO_FRAME));
    match reply {
        Reply::Ok { ok, .. } => {
            assert_eq!(ok.adapter_id, "fake-adapter");
            assert_eq!(ok.max_in_flight, 1);
            assert_eq!(
                ok.capabilities,
                vec![
                    "resources.list".to_owned(),
                    "balances.read".to_owned(),
                    "history.read".to_owned(),
                    "status.read".to_owned(),
                ]
            );
        }
        Reply::Err { .. } => panic!("expected ok"),
    }
}

#[test]
fn resources_list_frame_from_real_adapter_deserializes() {
    let reply: Reply<ResourcesListReply> = must(serde_json::from_str(RESOURCES_LIST_FRAME));
    match reply {
        Reply::Ok { ok, .. } => {
            assert_eq!(ok.resources.len(), 2);
            assert_eq!(ok.resources[0].resource_id, "wallet-eth");
            assert_eq!(ok.resources[0].kind, "onchain_wallet");
            assert_eq!(ok.resources[1].resource_id, "wallet-btc");
        }
        Reply::Err { .. } => panic!("expected ok"),
    }
}

#[test]
fn balances_read_frame_from_real_adapter_deserializes_and_amounts_parse() {
    let reply: Reply<BalancesReadReply> = must(serde_json::from_str(BALANCES_READ_FRAME));
    match reply {
        Reply::Ok { ok, .. } => {
            assert_eq!(ok.observations.len(), 3);
            // Every observation carries its resource_id (Ruling A1).
            assert_eq!(ok.observations[0].resource_id, "wallet-eth");
            assert_eq!(ok.observations[2].resource_id, "wallet-btc");
            // The 78-digit uint256 wei value survived through sumer_money::Amount,
            // never touching an f64 -- this is the whole point of large_amounts.json.
            let wei = must_some(ok.observations[0].amount.as_ref());
            assert_eq!(
                wei.to_string(),
                "115792089237316195423570985008687907853269984665640564039457584007913129639935"
            );
            assert_eq!(
                ok.observations[1].canonical_hint,
                Some(CanonicalHint::Available)
            );
            // Every requested resource_id appears in statuses exactly once (A7).
            assert_eq!(ok.statuses.len(), 2);
            assert!(matches!(
                ok.statuses[0].outcome,
                ReadOutcome::Fetched { page_empty: false }
            ));
        }
        Reply::Err { .. } => panic!("expected ok"),
    }
}

#[test]
fn status_read_frame_from_real_adapter_has_no_observations_field() {
    // status.read has NO observations (Ruling A4) -- prove the wire frame
    // itself carries no such key, not just that our struct omits one.
    let value: serde_json::Value = must(serde_json::from_str(STATUS_READ_FRAME));
    assert!(value["ok"].get("observations").is_none());

    let reply: Reply<StatusReadReply> = must(serde_json::from_str(STATUS_READ_FRAME));
    match reply {
        Reply::Ok { ok, .. } => assert_eq!(ok.statuses.len(), 2),
        Reply::Err { .. } => panic!("expected ok"),
    }
}

#[test]
fn history_read_oversized_frame_from_real_adapter_deserializes() {
    let reply: Reply<HistoryReadReply> = must(serde_json::from_str(HISTORY_READ_OVERSIZED_FRAME));
    match reply {
        Reply::Ok { ok, .. } => {
            // A10: truncated sibling present with completeness partial and
            // the truncation marker, small sibling untouched, oversized
            // third observation omitted entirely (only two observations,
            // not three) but still reported in statuses.
            assert_eq!(ok.observations.len(), 2);
            assert_eq!(ok.observations[0].resource_id, "wallet-oversized");
            assert_eq!(ok.statuses.len(), 1);
            let status = &ok.statuses[0];
            assert_eq!(status.resource_id, "wallet-oversized");
            match &status.outcome {
                ReadOutcome::OversizedObservation { local_id, bytes } => {
                    assert_eq!(
                        local_id.as_deref(),
                        Some("wallet-oversized:big-untruncatable")
                    );
                    assert_eq!(*bytes, 260_000);
                }
                other => panic!("expected OversizedObservation, got {other:?}"),
            }
            // The per-resource page lives nested inside this status entry,
            // never as a reply-wide sibling (Ruling A3/A4).
            let page = must_some(status.page.as_ref());
            assert!(page.next.is_none());
        }
        Reply::Err { .. } => panic!("expected ok"),
    }
}
