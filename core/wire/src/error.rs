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

/// Reasons the host kills the adapter process outright. These are
/// host-side classifications and are never written to the wire: a stream
/// that has hit one of these has no reply channel left to trust, so there
/// is no "sorry, protocol violation" envelope -- the process is finished.
///
/// [`crate::codec::FrameDecoder`] can only ever produce `OversizeFrame`,
/// `NonUtf8`, or `NotJson` (frame-level corruption). `UnterminatedFrame`,
/// `UnknownId`, `DuplicateId`, `PreHelloOutput`, `StdinEofIgnored`, and
/// `StdoutHeldOpen` are host-loop classifications built on top of the
/// decoded frames (the decoder's leftover bytes at end of stream, id
/// bookkeeping, the pre-hello-output rule, and what the process and its
/// stdout do once the host has closed its stdin) and are enforced by the
/// host, not by this crate's framer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolViolationKind {
    /// The frame (excluding its terminating LF) exceeded
    /// [`crate::MAX_FRAME_BYTES`].
    OversizeFrame,
    /// The frame's bytes were not valid UTF-8.
    NonUtf8,
    /// The frame was valid UTF-8 but not a syntactically valid JSON value.
    NotJson,
    /// The adapter's stdout reached end of stream with bytes still buffered
    /// that no LF ever terminated (spec/wire.md §2). They are not a frame
    /// and never became one: the stream stopped in the middle of one. The
    /// host neither judged them nor may discard them -- unjudged output is
    /// exactly what the close boundary exists to make impossible.
    UnterminatedFrame,
    /// A reply named an id above the counter high-water mark, or an
    /// impossible gap: no request ever issued that id.
    UnknownId,
    /// A reply named an id that was already answered.
    DuplicateId,
    /// The adapter wrote to stdout before its hello reply.
    PreHelloOutput,
    /// The host closed the adapter's stdin -- the defined end of a
    /// connection (spec/wire.md §7) -- and the process was still running
    /// when the connection's deadline expired. Anything it writes from
    /// here answers no request and reaches no caller, so the connection
    /// cannot be judged to have ended cleanly; it did not end.
    StdinEofIgnored,
    /// The adapter process exited, but its stdout was still open when the
    /// host stopped waiting for it: something that inherited the write end
    /// outlived the process (spec/wire.md §7 -- "drop whatever you are
    /// still holding"). Awaiting the process therefore did NOT establish
    /// end of stream, so the host cannot say it read everything the
    /// connection produced. Distinct from [`ProtocolViolationKind::StdinEofIgnored`]
    /// on purpose: there the process is still running; here it exited and
    /// the stream outlived it.
    ///
    /// **This kind names a missed drain, never a culprit.** What the host
    /// times is its own reader task finishing; task completion cannot
    /// identify descriptor ownership, so an adapter that forked a writer
    /// and a host whose reader was simply not scheduled in time are
    /// indistinguishable from here. Read it as "nobody saw the end of this
    /// stream", not as "the adapter held it open".
    StdoutHeldOpen,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
