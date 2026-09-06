//! Negative decoding: the frames a conforming host must REJECT.
//!
//! Two traps, both proved live against this crate before the fix that
//! closed them:
//!   - an envelope satisfying both the request and the reply shape (or
//!     neither) is malformed (`spec/wire.md` §1), but `#[serde(untagged)]`
//!     picked the first variant that fit and silently ignored the rest;
//!   - serde's derived struct impl accepts a positional array, so every
//!     wire object could also arrive as a JSON array -- the exact trap
//!     `sumer-money`'s `AmountWire` was hand-written to close, never
//!     applied to the rest.

use sumer_wire::{
    BalanceWire, BalancesReadParams, BalancesReadReply, Degraded, ErrorBody, FrameDecoder,
    HelloParams, HelloReply, HistoryReadParams, HistoryReadReply, ObservationWire, PageReply,
    PageRequest, ProvenanceWire, ProviderDetail, ReadOutcome, Reply, Request, ResourceDescriptor,
    ResourceQuery, ResourceStatus, ResourcesListParams, ResourcesListReply, StatusReadParams,
    StatusReadReply,
};

/// Feeds `text` through the real framer -- the only path bytes from an
/// adapter ever take into a wire type -- and returns the frame it yields.
/// Deserializing anything in this file from anything else would be testing
/// a path production does not use: `serde_json::Value` in particular
/// collapses duplicate keys, so a check run against one cannot see them.
fn framed(text: &str) -> String {
    let mut decoder = FrameDecoder::new();
    let mut frames = Vec::new();
    let mut line = text.to_owned();
    line.push('\n');
    match decoder.push(line.as_bytes(), &mut frames) {
        Ok(()) => {}
        Err(kind) => panic!("the framer rejected {text} outright: {kind:?}"),
    }
    match frames.len() {
        1 => frames.remove(0),
        n => panic!("expected exactly one frame from {text}, got {n}"),
    }
}

/// Asserts `json` does not deserialize as `T`, through the framer.
macro_rules! rejects {
    ($ty:ty, $json:expr, $why:expr) => {{
        let value: serde_json::Value = $json;
        let text = serde_json::to_string(&value).expect("serialize");
        rejects_frame!($ty, &text, $why);
    }};
}

/// As [`rejects`], for a frame that cannot be written as a
/// `serde_json::Value` in the first place -- a duplicate key, say.
macro_rules! rejects_frame {
    ($ty:ty, $text:expr, $why:expr) => {{
        let text: &str = $text;
        let frame = framed(text);
        let result: Result<$ty, _> = serde_json::from_str(&frame);
        assert!(
            result.is_err(),
            "{}: {} deserialized as {} off the framer but must be rejected",
            $why,
            frame,
            stringify!($ty)
        );
    }};
}

macro_rules! accepts {
    ($ty:ty, $json:expr) => {{
        let value: serde_json::Value = $json;
        let text = serde_json::to_string(&value).expect("serialize");
        let frame = framed(&text);
        let result: Result<$ty, _> = serde_json::from_str(&frame);
        match result {
            Ok(v) => v,
            Err(e) => panic!("{frame} should deserialize as {}: {e}", stringify!($ty)),
        }
    }};
}

// ---------------------------------------------------------------------
// Envelope: presence and exclusivity, checked before variant selection
// ---------------------------------------------------------------------

#[test]
fn reply_with_both_ok_and_err_is_malformed() {
    rejects!(
        Reply<serde_json::Value>,
        serde_json::json!({"id": 1, "ok": {"a": 1}, "err": {"code": "internal", "message": "boom"}}),
        "exactly one of ok/err (spec/wire.md 1)"
    );
}

#[test]
fn reply_with_neither_ok_nor_err_is_malformed() {
    rejects!(
        Reply<serde_json::Value>,
        serde_json::json!({"id": 1}),
        "exactly one of ok/err"
    );
}

#[test]
fn reply_mixing_request_fields_is_malformed() {
    rejects!(
        Reply<serde_json::Value>,
        serde_json::json!({"id": 1, "op": "balances.read", "ok": {}}),
        "a frame satisfying both shapes is malformed"
    );
    rejects!(
        Reply<serde_json::Value>,
        serde_json::json!({"id": 1, "ok": {}, "params": {}}),
        "params belongs to a request"
    );
}

#[test]
fn reply_without_id_is_malformed() {
    rejects!(
        Reply<serde_json::Value>,
        serde_json::json!({"ok": {}}),
        "every frame carries an id"
    );
}

#[test]
fn duplicate_keys_are_malformed_off_the_framer() {
    // The trap this closes: `serde_json::Value` keeps the LAST of two
    // duplicate keys and drops the first, so a host that parsed a frame
    // into a `Value` before decoding it typed would accept every one of
    // these. They are checked here on the bytes the framer hands on.
    rejects_frame!(
        Reply<serde_json::Value>,
        r#"{"id":1,"ok":{"a":1},"ok":{"b":2}}"#,
        "a repeated `ok` is malformed"
    );
    rejects_frame!(
        Reply<serde_json::Value>,
        r#"{"id":1,"id":2,"ok":{}}"#,
        "a repeated `id` is malformed"
    );
    rejects_frame!(
        Request,
        r#"{"id":1,"op":"hello","op":"balances.read","params":{}}"#,
        "a repeated `op` is malformed"
    );
    rejects_frame!(
        HelloReply,
        r#"{"protocol":"1","protocol":"999","adapter_id":"a","adapter_version":"0.1","capabilities":[],"local_id_derivation":"d@1"}"#,
        "a repeated `protocol` is malformed"
    );
    rejects_frame!(
        ResourceStatus,
        r#"{"resource_id":"c1","outcome":"unavailable","outcome":{"fetched":{"page_empty":false}}}"#,
        "a repeated `outcome` is malformed"
    );
}

#[test]
fn request_mixing_reply_fields_is_malformed() {
    rejects!(
        Request,
        serde_json::json!({"id": 1, "op": "hello", "ok": {}}),
        "a frame satisfying both shapes is malformed"
    );
    rejects!(
        Request,
        serde_json::json!({"id": 1, "op": "hello", "err": {"code": "internal", "message": "x"}}),
        "a frame satisfying both shapes is malformed"
    );
}

#[test]
fn request_without_op_is_malformed() {
    rejects!(
        Request,
        serde_json::json!({"id": 1, "params": {}}),
        "a request names its op"
    );
}

#[test]
fn well_formed_envelopes_still_decode() {
    let ok = accepts!(
        Reply<serde_json::Value>,
        serde_json::json!({"id": 7, "ok": {"observations": []}})
    );
    assert!(matches!(ok, Reply::Ok { .. }));
    let err = accepts!(
        Reply<serde_json::Value>,
        serde_json::json!({"id": 7, "err": {"code": "unsupported", "message": "no"}})
    );
    assert!(matches!(err, Reply::Err { .. }));
    let req = accepts!(Request, serde_json::json!({"id": 7, "op": "hello"}));
    assert_eq!(req.op, "hello");
}

// ---------------------------------------------------------------------
// Every wire type is object-only: a positional array is not a wire object
// ---------------------------------------------------------------------

#[test]
fn positional_arrays_are_not_wire_objects() {
    rejects!(
        ProvenanceWire,
        serde_json::json!(["a", "p", "s", "2026-09-06T00:00:00Z", null, "complete"]),
        "provenance is an object"
    );
    rejects!(
        BalanceWire,
        serde_json::json!([
            "checking-1",
            "available",
            null,
            {"asset": "USD", "amount": "1.00"},
            ["a", "p", "s", "2026-09-06T00:00:00Z", null, "complete"]
        ]),
        "a balance line is an object"
    );
    rejects!(
        ObservationWire,
        serde_json::json!([
            "checking-1", "abc", null, null, "active", null, "s", "posted",
            {"asset": "USD", "amount": "1.00"}, null, "provider_positive", "rent", {},
            ["a", "p", "s", "2026-09-06T00:00:00Z", null, "complete"]
        ]),
        "an observation is an object"
    );
    rejects!(
        ProviderDetail,
        serde_json::json!(["rate_limited", "slow down", {}]),
        "provider_detail is an object"
    );
    rejects!(
        ResourceStatus,
        serde_json::json!(["acct1", "unavailable", null, null, null, null, null]),
        "a status entry is an object"
    );
    rejects!(
        PageReply,
        serde_json::json!(["exact", null, null, null]),
        "a page reply is an object"
    );
    rejects!(
        PageRequest,
        serde_json::json!(["cursor", "abc"]),
        "a page request is an object"
    );
    rejects!(
        ReadOutcome,
        serde_json::json!({"fetched": [false]}),
        "an outcome body is an object"
    );
    rejects!(
        Degraded,
        serde_json::json!(["abc", 70000]),
        "a degrade entry is an object"
    );
    rejects!(
        ReadOutcome,
        serde_json::json!({"oversized_observation": {"local_id": "abc", "bytes": 70000}}),
        "oversized degradation is `degraded`, not an outcome"
    );
    rejects!(
        ReadOutcome,
        serde_json::json!({"fetched": {"page_empty": false}, "unavailable": null}),
        "an outcome names exactly one variant"
    );
    rejects!(
        ErrorBody,
        serde_json::json!(["internal", "boom", null]),
        "an err body is an object"
    );
    rejects!(
        HelloParams,
        serde_json::json!([["1"]]),
        "hello params are an object"
    );
    rejects!(
        HelloReply,
        serde_json::json!(["1", "a", "0.1", [], "d@1", 1]),
        "a hello reply is an object"
    );
    rejects!(
        Reply<serde_json::Value>,
        serde_json::json!([1, {"a": 1}]),
        "an envelope is an object"
    );
    rejects!(
        Request,
        serde_json::json!([1, "hello", {}]),
        "an envelope is an object"
    );
    rejects!(
        ResourceQuery,
        serde_json::json!(["acct1", null]),
        "a resource query is an object"
    );
    rejects!(
        ResourceDescriptor,
        serde_json::json!(["acct1", "p1", "bank_checking", "Checking", null]),
        "a resource descriptor is an object"
    );
    rejects!(
        ResourcesListParams,
        serde_json::json!([]),
        "params are an object"
    );
    rejects!(
        ResourcesListReply,
        serde_json::json!([[]]),
        "a reply body is an object"
    );
    rejects!(
        BalancesReadParams,
        serde_json::json!([["acct1"]]),
        "params are an object"
    );
    rejects!(
        BalancesReadReply,
        serde_json::json!([[], []]),
        "a reply body is an object"
    );
    rejects!(
        HistoryReadParams,
        serde_json::json!([[]]),
        "params are an object"
    );
    rejects!(
        HistoryReadReply,
        serde_json::json!([[], []]),
        "a reply body is an object"
    );
    rejects!(
        StatusReadParams,
        serde_json::json!([["acct1"]]),
        "params are an object"
    );
    rejects!(
        StatusReadReply,
        serde_json::json!([[]]),
        "a reply body is an object"
    );
}

// ---------------------------------------------------------------------
// Missing key vs. explicit null, for every Option<T> on a wire type
// ---------------------------------------------------------------------

fn provenance_json() -> serde_json::Value {
    serde_json::json!({
        "adapter_id": "a", "provider_id": "p", "surface": "checking",
        "observed_at": "2026-09-06T12:00:00Z", "completeness": "complete"
    })
}

fn observation_json() -> serde_json::Value {
    serde_json::json!({
        "resource_id": "checking-1",
        "local_id": "abc",
        "state": "active",
        "surface": "checking",
        "posting": "posted",
        "amount": {"asset": "USD", "amount": "10.00"},
        "raw_sign": "provider_positive",
        "description": "rent",
        "provenance": provenance_json()
    })
}

#[test]
fn optional_evidence_fields_read_the_same_missing_or_null() {
    // These fields are optional *evidence*: "the provider had nothing to
    // say" and "the key is absent" are the same statement, and both must
    // land on None rather than one of them being an error.
    let missing = accepts!(ObservationWire, observation_json());
    let mut explicit = observation_json();
    for key in [
        "provider_id",
        "supersedes_provider_id",
        "tombstone_reason",
        "fees",
    ] {
        explicit[key] = serde_json::Value::Null;
    }
    let explicit = accepts!(ObservationWire, explicit);
    assert!(missing.provider_id.is_none() && explicit.provider_id.is_none());
    assert!(missing.supersedes_provider_id.is_none() && explicit.supersedes_provider_id.is_none());
    assert!(missing.tombstone_reason.is_none() && explicit.tombstone_reason.is_none());
    assert!(missing.fees.is_none() && explicit.fees.is_none());

    let missing = accepts!(ProvenanceWire, provenance_json());
    let mut explicit = provenance_json();
    explicit["effective_at"] = serde_json::Value::Null;
    let explicit = accepts!(ProvenanceWire, explicit);
    assert!(missing.effective_at.is_none() && explicit.effective_at.is_none());

    let missing = accepts!(
        BalanceWire,
        serde_json::json!({
            "resource_id": "c1", "category": "available", "amount": null,
            "provenance": provenance_json()
        })
    );
    let explicit = accepts!(
        BalanceWire,
        serde_json::json!({
            "resource_id": "c1", "category": "available", "amount": null,
            "canonical_hint": null, "provenance": provenance_json()
        })
    );
    assert!(missing.canonical_hint.is_none() && explicit.canonical_hint.is_none());

    let missing = accepts!(
        ResourceStatus,
        serde_json::json!({"resource_id": "c1", "outcome": "unavailable"})
    );
    let explicit = accepts!(
        ResourceStatus,
        serde_json::json!({
            "resource_id": "c1", "outcome": "unavailable", "provider_detail": null,
            "page": null, "credential_expires_at": null, "strong_auth_expires_at": null,
            "history_start": null
        })
    );
    assert!(missing.page.is_none() && explicit.page.is_none());
    assert!(missing.provider_detail.is_none() && explicit.provider_detail.is_none());
    assert!(missing.history_start.is_none() && explicit.history_start.is_none());

    let missing = accepts!(PageReply, serde_json::json!({"cursor_resumable": "exact"}));
    let explicit = accepts!(
        PageReply,
        serde_json::json!({
            "cursor_resumable": "exact", "next": null, "window_capped_to": null,
            "page_size_reduced_to": null
        })
    );
    assert!(missing.next.is_none() && explicit.next.is_none());
    assert!(missing.window_capped_to.is_none() && explicit.window_capped_to.is_none());
    assert!(missing.page_size_reduced_to.is_none() && explicit.page_size_reduced_to.is_none());
}

#[test]
fn a_missing_amount_stays_a_hard_error_while_null_means_unknown() {
    // The one Option that is NOT interchangeable with an absent key:
    // `amount: null` is "the adapter looked and does not know", and a
    // missing key must never be silently read as that (or as zero).
    rejects!(
        BalanceWire,
        serde_json::json!({
            "resource_id": "c1", "category": "available", "provenance": provenance_json()
        }),
        "a missing amount is not `unknown`"
    );
    let known = accepts!(
        BalanceWire,
        serde_json::json!({
            "resource_id": "c1", "category": "available", "amount": null,
            "provenance": provenance_json()
        })
    );
    assert!(known.amount.is_none());
}

#[test]
fn unknown_fields_are_still_rejected_after_the_object_only_rule() {
    rejects!(
        ProvenanceWire,
        serde_json::json!({
            "adapter_id": "a", "provider_id": "p", "surface": "s",
            "observed_at": "2026-09-06T12:00:00Z", "completeness": "complete",
            "received_at": "2026-09-06T12:00:01Z"
        }),
        "received_at is host-stamped"
    );
    let mut extra = observation_json();
    extra["revision"] = serde_json::json!(3);
    rejects!(
        ObservationWire,
        extra,
        "the host assigns revision, not the adapter"
    );
}
