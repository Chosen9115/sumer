//! `sumer-wire`: the JSON-Lines wire protocol shared by the host and its
//! adapter subprocesses.
//!
//! This crate defines *shapes and pure logic only*: the envelope, the
//! incremental frame decoder, the four read capabilities' request/reply
//! types, and the observation model. It does not run a host loop, spawn
//! processes, or fold observations into a live set (that is
//! `sumer_host::fold`, per the frozen contract for this milestone) --
//! keeping those out of this crate is what lets the conformance suite
//! assert that a real host and this crate's property tests agree on the
//! rules rather than on two independent readings of them.
//!
//! No floats anywhere: money moves through this wire as [`sumer_money::Amount`],
//! embedded as-is and never redeclared.

#![forbid(unsafe_code)]

/// Largest allowed byte length of one frame, counted *before* its
/// terminating LF.
pub const MAX_FRAME_BYTES: usize = 1_048_576;

/// Largest allowed serialized byte length of a single observation before an
/// adapter must truncate `provider_extra` (and, if still oversized, omit
/// the observation and report `oversized_observation` instead). Enforcing
/// this bound is the adapter's/host's job; this crate only names the
/// constant both sides must agree on.
pub const MAX_OBSERVATION_BYTES: usize = 65_536;

mod codec;
mod envelope;
mod error;
mod observation;
mod ops;

pub use codec::FrameDecoder;
pub use envelope::{ErrorBody, HelloParams, HelloReply, Reply, Request, RequestId};
pub use error::{ProtocolViolationKind, WireErrorCode};
pub use observation::{
    fold_order_key, validate_plain_text, Balance, BalanceWire, CanonicalHint, Completeness,
    CursorResumable, Observation, ObservationState, ObservationWire, PageReply, PageRequest,
    PlainTextError, Posting, Provenance, ProvenanceWire, ProviderDetail, RawSign, ReadOutcome,
    ResourceStatus, Rfc3339, Rfc3339Error, Staleness,
};
pub use ops::{
    BalancesReadParams, BalancesReadReply, HistoryReadParams, HistoryReadReply, ResourceDescriptor,
    ResourceQuery, ResourcesListParams, ResourcesListReply, StatusReadParams, StatusReadReply,
    OP_BALANCES_READ, OP_HELLO, OP_HISTORY_READ, OP_RESOURCES_LIST, OP_STATUS_READ,
};

// Embedded as-is, per the contract: this crate never redeclares or
// reimplements money or asset identifiers.
pub use sumer_money::{Amount, AssetId, MoneyError};
