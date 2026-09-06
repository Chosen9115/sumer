//! The incremental JSON-Lines framer.
//!
//! Bytes arrive in arbitrarily-sized chunks (a subprocess pipe gives no
//! guarantee about read boundaries). Frames are LF (`\n`)-terminated UTF-8
//! JSON values; [`crate::MAX_FRAME_BYTES`] bounds a frame's length
//! *excluding* the terminating LF.
//!
//! Required invariants (see the crate's fuzz target, `fuzz/fuzz_targets/codec.rs`):
//! - never panics on any byte sequence;
//! - never allocates beyond `MAX_FRAME_BYTES + 1` bytes for the
//!   in-progress frame (`FrameDecoder::new` reserves exactly that capacity
//!   once, up front, so no later growth can overshoot it);
//! - **split-invariance**: feeding the same bytes through any chunking
//!   produces the identical frame sequence (and the identical violation, if
//!   any) as feeding them in one call -- the decoder is a byte-at-a-time
//!   state machine with no lookahead across chunk boundaries;
//! - oversize is detected the instant the buffered byte count would exceed
//!   the cap, not after accumulating arbitrarily more first.
//!
//! On any violation the decoder is **done**: a truncated JSON-Lines stream
//! has no resync point, so there is no attempt to find the next newline and
//! continue. Every subsequent call returns the same violation without
//! touching the buffer.

use crate::error::ProtocolViolationKind;
use crate::MAX_FRAME_BYTES;

/// Incrementally decodes a byte stream into JSON-Lines frames.
pub struct FrameDecoder {
    buf: Vec<u8>,
    violation: Option<ProtocolViolationKind>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    /// Reserves exactly `MAX_FRAME_BYTES + 1` bytes up front: the largest
    /// the in-progress buffer can ever validly reach before a push detects
    /// the oversize condition and bails, so no later reallocation is
    /// possible.
    #[must_use]
    pub fn new() -> FrameDecoder {
        FrameDecoder {
            buf: Vec::with_capacity(MAX_FRAME_BYTES.saturating_add(1)),
            violation: None,
        }
    }

    /// Feeds the next chunk of bytes (any length, including zero).
    /// Completed frames are appended, in order, to `out`. Frames already
    /// appended to `out` before a violation is hit are *not* rolled back --
    /// that is what makes the same input split into different chunkings
    /// produce the same observable frame sequence.
    ///
    /// # Errors
    /// Returns the [`ProtocolViolationKind`] on the first oversize,
    /// non-UTF-8, or unparseable frame. Once returned, every subsequent
    /// call returns the same violation immediately and does not touch the
    /// buffer or `out`.
    pub fn push(
        &mut self,
        chunk: &[u8],
        out: &mut Vec<serde_json::Value>,
    ) -> Result<(), ProtocolViolationKind> {
        if let Some(kind) = self.violation {
            return Err(kind);
        }
        for &b in chunk {
            if b == b'\n' {
                match Self::decode_frame(&self.buf) {
                    Ok(value) => {
                        out.push(value);
                        self.buf.clear();
                    }
                    Err(kind) => {
                        self.violation = Some(kind);
                        return Err(kind);
                    }
                }
            } else {
                self.buf.push(b);
                if self.buf.len() > MAX_FRAME_BYTES {
                    self.violation = Some(ProtocolViolationKind::OversizeFrame);
                    return Err(ProtocolViolationKind::OversizeFrame);
                }
            }
        }
        Ok(())
    }

    fn decode_frame(bytes: &[u8]) -> Result<serde_json::Value, ProtocolViolationKind> {
        let text = std::str::from_utf8(bytes).map_err(|_| ProtocolViolationKind::NonUtf8)?;
        serde_json::from_str(text).map_err(|_| ProtocolViolationKind::NotJson)
    }

    /// `true` once a violation has been recorded; no more frames will ever
    /// be produced.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.violation.is_some()
    }

    /// Bytes buffered for the frame currently in progress (no terminating
    /// LF has arrived yet). Non-empty at end-of-stream means a truncated
    /// final frame; classifying that (e.g. as `AdapterCrashed`) is a
    /// host-loop decision, not this decoder's.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn push_all(
        decoder: &mut FrameDecoder,
        chunk: &[u8],
    ) -> Result<Vec<serde_json::Value>, ProtocolViolationKind> {
        let mut out = Vec::new();
        decoder.push(chunk, &mut out)?;
        Ok(out)
    }

    #[test]
    fn decodes_single_frame() {
        let mut d = FrameDecoder::new();
        let frames = push_all(&mut d, b"{\"id\":1}\n").unwrap();
        assert_eq!(frames, vec![serde_json::json!({"id": 1})]);
        assert_eq!(d.pending_bytes(), 0);
    }

    #[test]
    fn decodes_multiple_frames_in_one_chunk() {
        let mut d = FrameDecoder::new();
        let frames = push_all(&mut d, b"{\"a\":1}\n{\"b\":2}\n").unwrap();
        assert_eq!(
            frames,
            vec![serde_json::json!({"a": 1}), serde_json::json!({"b": 2})]
        );
    }

    #[test]
    fn empty_push_yields_no_frames() {
        let mut d = FrameDecoder::new();
        assert_eq!(
            push_all(&mut d, b"").unwrap(),
            Vec::<serde_json::Value>::new()
        );
    }

    #[test]
    fn partial_frame_across_two_pushes() {
        let mut d = FrameDecoder::new();
        assert_eq!(
            push_all(&mut d, b"{\"id\":").unwrap(),
            Vec::<serde_json::Value>::new()
        );
        assert_eq!(d.pending_bytes(), 6);
        let frames = push_all(&mut d, b"1}\n").unwrap();
        assert_eq!(frames, vec![serde_json::json!({"id": 1})]);
    }

    #[test]
    fn non_utf8_frame_is_a_violation() {
        let mut d = FrameDecoder::new();
        let mut out = Vec::new();
        let err = d.push(&[0xFF, 0xFE, b'\n'], &mut out).unwrap_err();
        assert_eq!(err, ProtocolViolationKind::NonUtf8);
        assert!(d.is_finished());
    }

    #[test]
    fn unparseable_json_is_a_violation() {
        let mut d = FrameDecoder::new();
        let mut out = Vec::new();
        let err = d.push(b"not json\n", &mut out).unwrap_err();
        assert_eq!(err, ProtocolViolationKind::NotJson);
    }

    #[test]
    fn empty_line_is_not_json() {
        let mut d = FrameDecoder::new();
        let mut out = Vec::new();
        let err = d.push(b"\n", &mut out).unwrap_err();
        assert_eq!(err, ProtocolViolationKind::NotJson);
    }

    #[test]
    fn oversize_detected_exactly_at_cap_plus_one_no_resync() {
        let mut d = FrameDecoder::new();
        let mut out = Vec::new();
        // Exactly at the cap: no LF yet, should not error.
        let at_cap = vec![b'9'; MAX_FRAME_BYTES];
        d.push(&at_cap, &mut out).unwrap();
        assert_eq!(d.pending_bytes(), MAX_FRAME_BYTES);
        // One more byte pushes it over: this must be the exact point of failure.
        let err = d.push(b"9", &mut out).unwrap_err();
        assert_eq!(err, ProtocolViolationKind::OversizeFrame);
        assert_eq!(d.pending_bytes(), MAX_FRAME_BYTES + 1);
        // No resync: further input (even a valid terminated frame) is refused.
        let err2 = d.push(b"{}\n", &mut out).unwrap_err();
        assert_eq!(err2, ProtocolViolationKind::OversizeFrame);
        assert!(out.is_empty());
    }

    #[test]
    fn frames_decoded_before_a_violation_in_the_same_call_are_kept() {
        let mut d = FrameDecoder::new();
        let mut out = Vec::new();
        let err = d.push(b"{\"a\":1}\nnot json\n", &mut out).unwrap_err();
        assert_eq!(err, ProtocolViolationKind::NotJson);
        assert_eq!(out, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn violation_is_sticky_across_calls() {
        let mut d = FrameDecoder::new();
        let mut out = Vec::new();
        d.push(b"not json\n", &mut out).unwrap_err();
        let err = d.push(b"{\"a\":1}\n", &mut out).unwrap_err();
        assert_eq!(err, ProtocolViolationKind::NotJson);
        assert!(out.is_empty());
    }
}
