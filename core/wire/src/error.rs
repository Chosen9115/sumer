//! Error vocabulary: the wire-visible [`WireErrorCode`] carried in an
//! envelope's `err.code`, and the host-only [`ProtocolViolationKind`] that
//! never appears on the wire because the stream is dead once it happens.

use serde::{Deserialize, Serialize};

/// Envelope `err.code` values. Exactly the five named by the frozen
/// contract -- no catch-all variant, so a new error class is a deliberate,
/// visible addition to this enum rather than a string nobody validates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorCode {
    UnsupportedProtocol,
    Unsupported,
    InvalidRequest,
    NotReady,
    Internal,
}

impl WireErrorCode {
    /// Whether a caller may reasonably retry the *same* request unchanged.
    ///
    /// Only `not_ready` describes a transient condition ("the adapter isn't
    /// ready yet, ask again"). `unsupported_protocol`, `unsupported`, and
    /// `invalid_request` describe the request itself, which retrying
    /// without changing it cannot fix. `internal` carries no resource
    /// attribution to reason about (per the wire contract's definition of
    /// `err`), so this is conservative and treats it as non-retryable
    /// rather than guessing it is transient.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, WireErrorCode::NotReady)
    }
}

/// Reasons the host kills the adapter process outright. These are
/// host-side classifications and are never written to the wire: a stream
/// that has hit one of these has no reply channel left to trust, so there
/// is no "sorry, protocol violation" envelope -- the process is finished.
///
/// [`crate::codec::FrameDecoder`] can only ever produce `OversizeFrame`,
/// `NonUtf8`, or `NotJson` (frame-level corruption). `UnknownId`,
/// `DuplicateId`, and `PreHelloOutput` are host-loop classifications built
/// on top of the decoded frames (id bookkeeping, and the pre-hello-output
/// rule) and are enforced by the host, not by this crate's framer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolViolationKind {
    /// The frame (excluding its terminating LF) exceeded
    /// [`crate::MAX_FRAME_BYTES`].
    OversizeFrame,
    /// The frame's bytes were not valid UTF-8.
    NonUtf8,
    /// The frame was valid UTF-8 but not a syntactically valid JSON value.
    NotJson,
    /// A reply named an id above the counter high-water mark, or an
    /// impossible gap: no request ever issued that id.
    UnknownId,
    /// A reply named an id that was already answered.
    DuplicateId,
    /// The adapter wrote to stdout before its hello reply.
    PreHelloOutput,
}

impl ProtocolViolationKind {
    /// Always `false`: every violation is fatal by construction. A
    /// truncated JSON-Lines stream has no resync point, so retrying (or
    /// resuming) is never offered -- the contract calls this "KILL, NO
    /// RESYNC".
    #[must_use]
    pub fn is_retryable(self) -> bool {
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn only_not_ready_is_retryable() {
        assert!(WireErrorCode::NotReady.is_retryable());
        assert!(!WireErrorCode::UnsupportedProtocol.is_retryable());
        assert!(!WireErrorCode::Unsupported.is_retryable());
        assert!(!WireErrorCode::InvalidRequest.is_retryable());
        assert!(!WireErrorCode::Internal.is_retryable());
    }

    #[test]
    fn no_protocol_violation_is_retryable() {
        for kind in [
            ProtocolViolationKind::OversizeFrame,
            ProtocolViolationKind::NonUtf8,
            ProtocolViolationKind::NotJson,
            ProtocolViolationKind::UnknownId,
            ProtocolViolationKind::DuplicateId,
            ProtocolViolationKind::PreHelloOutput,
        ] {
            assert!(!kind.is_retryable());
        }
    }

    #[test]
    fn wire_error_code_snake_case_on_the_wire() {
        let json = serde_json::to_string(&WireErrorCode::UnsupportedProtocol).unwrap();
        assert_eq!(json, "\"unsupported_protocol\"");
        let json = serde_json::to_string(&WireErrorCode::NotReady).unwrap();
        assert_eq!(json, "\"not_ready\"");
    }

    #[test]
    fn wire_error_code_round_trips() {
        for code in [
            WireErrorCode::UnsupportedProtocol,
            WireErrorCode::Unsupported,
            WireErrorCode::InvalidRequest,
            WireErrorCode::NotReady,
            WireErrorCode::Internal,
        ] {
            let json = serde_json::to_string(&code).unwrap();
            let back: WireErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(code, back);
        }
    }
}
