//! The JSON-Lines envelope: one request or reply per LF-terminated line.
//!
//! `Request`/`Reply` carry `params`/`ok` as raw [`serde_json::Value`]
//! deliberately. An op string the host doesn't recognize, or params that
//! don't match a recognized op's schema, must still deserialize as a valid
//! envelope so the host can answer with an ordinary `unsupported` /
//! `invalid_request` reply -- never a protocol kill. Only frame-level
//! corruption (see [`crate::codec`]) is fatal.

use crate::error::WireErrorCode;
use serde::{Deserialize, Serialize};

/// A wire request id: a `u64` from a monotonic counter, never reused for
/// the process lifetime. `#[serde(transparent)]` makes it serialize as a
/// bare JSON number (`"id":7`), matching the envelope exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub u64);

impl RequestId {
    /// The id reserved for the handshake request (`{"id":0,"op":"hello",...}`).
    pub const HELLO: RequestId = RequestId(0);
}

/// A decoded request line: `{"id":7,"op":"balances.read","params":{...}}`.
///
/// `op` is a plain `String`, not an enum: an unrecognized op string must
/// still parse successfully here, so recognizing (or rejecting) it is a
/// business decision made one layer up (see `ops.rs`), not a parse failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub op: String,
    #[serde(default = "default_params")]
    pub params: serde_json::Value,
}

fn default_params() -> serde_json::Value {
    serde_json::Value::Null
}

impl Request {
    /// Builds a request line with the given id, op, and params.
    #[must_use]
    pub fn new(id: RequestId, op: impl Into<String>, params: serde_json::Value) -> Request {
        Request {
            id,
            op: op.into(),
            params,
        }
    }
}

/// The body of an `err` reply: `{"code":...,"message":...,"detail":...}`.
///
/// `detail` is free-form evidence (e.g. `{"op": "..."}` for an unsupported
/// op) -- never a stable identifier callers should match on; `code` is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: WireErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

impl ErrorBody {
    #[must_use]
    pub fn new(code: WireErrorCode, message: impl Into<String>) -> ErrorBody {
        ErrorBody {
            code,
            message: message.into(),
            detail: None,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, detail: serde_json::Value) -> ErrorBody {
        self.detail = Some(detail);
        self
    }
}

/// A decoded reply line: `{"id":7,"ok":{...}}` or `{"id":7,"err":{...}}`.
///
/// `#[serde(untagged)]` distinguishes the two by which of `ok`/`err` is
/// present, matching the wire shape exactly -- there is no separate tag
/// field. `T` is the op-specific `ok` payload type; the host generally
/// decodes first as `Reply<serde_json::Value>` (it must know the original
/// request's op, hence its expected reply shape, before it can decode
/// further) and only then re-parses `ok` into the concrete type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reply<T> {
    Ok { id: RequestId, ok: T },
    Err { id: RequestId, err: ErrorBody },
}

impl<T> Reply<T> {
    #[must_use]
    pub fn id(&self) -> RequestId {
        match self {
            Reply::Ok { id, .. } | Reply::Err { id, .. } => *id,
        }
    }

    #[must_use]
    pub fn ok(id: RequestId, ok: T) -> Reply<T> {
        Reply::Ok { id, ok }
    }

    #[must_use]
    pub fn err(id: RequestId, err: ErrorBody) -> Reply<T> {
        Reply::Err { id, err }
    }
}

/// `hello` request params: `{"protocol":["1"]}`, the protocol versions the
/// host offers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloParams {
    pub protocol: Vec<String>,
}

fn default_max_in_flight() -> u32 {
    1
}

/// `hello` reply `ok` payload. `max_in_flight` defaults to `1` when the
/// adapter omits it -- a legal serial adapter is explicitly permitted, and
/// the host must never assume concurrency it wasn't told about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloReply {
    pub protocol: String,
    pub adapter_id: String,
    pub adapter_version: String,
    pub capabilities: Vec<String>,
    pub local_id_derivation: String,
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: u32,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn request_id_serializes_as_bare_number() {
        let json = serde_json::to_string(&RequestId(7)).unwrap();
        assert_eq!(json, "7");
    }

    #[test]
    fn request_wire_shape() {
        let req = Request::new(RequestId(7), "balances.read", serde_json::json!({"a": 1}));
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id": 7, "op": "balances.read", "params": {"a": 1}})
        );
    }

    #[test]
    fn request_missing_params_defaults_to_null() {
        let req: Request = serde_json::from_str(r#"{"id":0,"op":"hello"}"#).unwrap();
        assert_eq!(req.params, serde_json::Value::Null);
    }

    #[test]
    fn unrecognized_op_still_parses() {
        // The whole point: an op the host has never heard of must not be a
        // parse failure, or the host would have to kill the connection
        // instead of replying `unsupported`.
        let req: Request =
            serde_json::from_str(r#"{"id":1,"op":"underwater_basket_weaving","params":{}}"#)
                .unwrap();
        assert_eq!(req.op, "underwater_basket_weaving");
    }

    #[test]
    fn reply_ok_wire_shape() {
        let reply = Reply::ok(RequestId(7), serde_json::json!({"balance": "1.00"}));
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id": 7, "ok": {"balance": "1.00"}})
        );
    }

    #[test]
    fn reply_err_wire_shape_omits_absent_detail() {
        let reply: Reply<serde_json::Value> = Reply::err(
            RequestId(2),
            ErrorBody::new(WireErrorCode::Unsupported, "no such op"),
        );
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id": 2, "err": {"code": "unsupported", "message": "no such op"}})
        );
    }

    #[test]
    fn reply_err_with_detail() {
        let reply: Reply<serde_json::Value> = Reply::err(
            RequestId(2),
            ErrorBody::new(WireErrorCode::Unsupported, "no such op")
                .with_detail(serde_json::json!({"op": "frobnicate"})),
        );
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(
            json["err"]["detail"],
            serde_json::json!({"op": "frobnicate"})
        );
    }

    #[test]
    fn reply_round_trips_ok_and_err() {
        let ok: Reply<serde_json::Value> = Reply::ok(RequestId(1), serde_json::json!(42));
        let json = serde_json::to_string(&ok).unwrap();
        let back: Reply<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id(), RequestId(1));
        assert!(matches!(back, Reply::Ok { .. }));

        let err: Reply<serde_json::Value> = Reply::err(
            RequestId(2),
            ErrorBody::new(WireErrorCode::Internal, "boom"),
        );
        let json = serde_json::to_string(&err).unwrap();
        let back: Reply<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id(), RequestId(2));
        assert!(matches!(back, Reply::Err { .. }));
    }

    #[test]
    fn hello_reply_defaults_max_in_flight_when_absent() {
        let reply: HelloReply = serde_json::from_str(
            r#"{"protocol":"1","adapter_id":"a","adapter_version":"0.1",
                "capabilities":[],"local_id_derivation":"a@1"}"#,
        )
        .unwrap();
        assert_eq!(reply.max_in_flight, 1);
    }

    #[test]
    fn hello_reply_respects_explicit_max_in_flight() {
        let reply: HelloReply = serde_json::from_str(
            r#"{"protocol":"1","adapter_id":"a","adapter_version":"0.1",
                "capabilities":[],"local_id_derivation":"a@1","max_in_flight":4}"#,
        )
        .unwrap();
        assert_eq!(reply.max_in_flight, 4);
    }
}
