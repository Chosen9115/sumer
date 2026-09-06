//! Integration-level framer cases: multi-frame decoding and, most
//! importantly, split-boundary regressions -- the same bytes fed through
//! different chunkings must parse identically (same frames, same
//! violation).

use sumer_wire::{FrameDecoder, ProtocolViolationKind, MAX_FRAME_BYTES};

/// Test-only stand-in for `.unwrap()` (denied by workspace lints): panics
/// with the error's `Debug` output instead of silently relying on a method
/// clippy is configured to reject even in tests.
fn must<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("unexpected error: {e:?}"),
    }
}

/// Test-only stand-in for `.unwrap_err()`.
fn must_err<T: std::fmt::Debug, E>(r: Result<T, E>) -> E {
    match r {
        Err(e) => e,
        Ok(v) => panic!("expected Err, got Ok({v:?})"),
    }
}

/// Feeds `input` through `decoder` split into consecutive chunks of size
/// `chunk_size` (the last chunk may be shorter). Returns the frames decoded
/// and, if the stream ended in a violation, that violation.
fn decode_chunked(input: &[u8], chunk_size: usize) -> (Vec<String>, Option<ProtocolViolationKind>) {
    let mut decoder = FrameDecoder::new();
    let mut out = Vec::new();
    let mut violation = None;
    let step = chunk_size.max(1);
    for chunk in input.chunks(step) {
        if let Err(kind) = decoder.push(chunk, &mut out) {
            violation = Some(kind);
            break;
        }
    }
    (out, violation)
}

/// Asserts that decoding `input` in one shot, byte-by-byte, and in a
/// handful of other chunk sizes all produce the identical outcome.
fn assert_split_invariant(input: &[u8]) -> (Vec<String>, Option<ProtocolViolationKind>) {
    let whole = decode_chunked(input, input.len().max(1));
    for chunk_size in [1, 2, 3, 7, 64, 4096] {
        let chunked = decode_chunked(input, chunk_size);
        assert_eq!(
            chunked, whole,
            "chunk size {chunk_size} produced a different outcome than feeding the whole input"
        );
    }
    whole
}

#[test]
fn multi_frame_stream_decodes_regardless_of_chunking() {
    let input = b"{\"id\":1,\"op\":\"hello\",\"params\":{\"protocol\":[\"1\"]}}\n\
                  {\"id\":2,\"op\":\"balances.read\",\"params\":{}}\n\
                  {\"id\":3,\"ok\":{\"observations\":[],\"statuses\":[]}}\n";
    let (frames, violation) = assert_split_invariant(input);
    assert_eq!(frames.len(), 3);
    assert!(violation.is_none());
}

#[test]
fn frame_split_exactly_on_the_lf_boundary() {
    let input = b"{\"a\":1}\n{\"b\":2}\n";
    // Chunk boundary lands exactly at the LF between the two frames.
    let mut decoder = FrameDecoder::new();
    let mut out = Vec::new();
    must(decoder.push(&input[..8], &mut out));
    assert_eq!(out, vec![r#"{"a":1}"#.to_owned()]);
    must(decoder.push(&input[8..], &mut out));
    assert_eq!(out, vec![r#"{"a":1}"#.to_owned(), r#"{"b":2}"#.to_owned()]);
}

#[test]
fn multibyte_utf8_character_split_across_a_chunk_boundary() {
    // A frame containing a 4-byte emoji, whose encoding is split mid-way
    // across two `push` calls -- the decoder must not validate UTF-8 until
    // the full (LF-delimited) frame is assembled.
    let input = "{\"note\":\"\u{1F600}\"}\n".as_bytes().to_vec();
    let (frames, violation) = assert_split_invariant(&input);
    assert!(violation.is_none());
    // The frame is handed on as its original bytes, emoji intact.
    assert_eq!(frames[0], "{\"note\":\"\u{1F600}\"}");
}

#[test]
fn oversize_frame_split_invariant() {
    let mut input = vec![b'9'; MAX_FRAME_BYTES + 1];
    input.push(b'\n');
    let (frames, violation) = assert_split_invariant(&input);
    assert!(frames.is_empty());
    assert_eq!(violation, Some(ProtocolViolationKind::OversizeFrame));
}

#[test]
fn oversize_frame_never_accumulates_past_the_cap_before_reporting() {
    // Feed the cap's worth of bytes plus a handful more, one byte at a
    // time, and confirm the violation fires the instant the cap is
    // crossed -- not after the whole (much larger) input has been fed.
    let mut decoder = FrameDecoder::new();
    let mut out = Vec::new();
    let mut failed_at = None;
    let total = MAX_FRAME_BYTES + 1000;
    for i in 0..total {
        match decoder.push(b"9", &mut out) {
            Ok(()) => {}
            Err(ProtocolViolationKind::OversizeFrame) => {
                failed_at = Some(i);
                break;
            }
            Err(other) => panic!("unexpected violation: {other:?}"),
        }
    }
    assert_eq!(failed_at, Some(MAX_FRAME_BYTES));
    assert_eq!(decoder.pending_bytes(), MAX_FRAME_BYTES + 1);
}

#[test]
fn non_utf8_split_invariant() {
    let mut input = vec![b'{', b'"', b'a', b'"', b':', b'"'];
    input.extend_from_slice(&[0xC3, 0x28]); // invalid UTF-8 sequence
    input.extend_from_slice(b"\"}\n");
    let (frames, violation) = assert_split_invariant(&input);
    assert!(frames.is_empty());
    assert_eq!(violation, Some(ProtocolViolationKind::NonUtf8));
}

#[test]
fn unparseable_json_split_invariant() {
    let input = b"this is not json at all\n";
    let (frames, violation) = assert_split_invariant(input);
    assert!(frames.is_empty());
    assert_eq!(violation, Some(ProtocolViolationKind::NotJson));
}

#[test]
fn no_resync_after_violation_within_same_stream() {
    // A perfectly valid frame following a corrupt one must NOT be
    // recovered -- there is no resync point in a truncated JSONL stream.
    let mut decoder = FrameDecoder::new();
    let mut out = Vec::new();
    let err = must_err(decoder.push(b"garbage\n{\"valid\":true}\n", &mut out));
    assert_eq!(err, ProtocolViolationKind::NotJson);
    assert!(
        out.is_empty(),
        "no frame after the violation should ever surface"
    );
    assert!(decoder.is_finished());

    // Feeding more input (even split differently) still refuses.
    let err2 = must_err(decoder.push(b"{\"valid\":true}\n", &mut out));
    assert_eq!(err2, ProtocolViolationKind::NotJson);
    assert!(out.is_empty());
}

#[test]
fn frames_before_violation_survive_regardless_of_chunking() {
    let input = b"{\"ok\":1}\ngarbage\n";
    for chunk_size in [1, 2, 5, input.len()] {
        let (frames, violation) = decode_chunked(input, chunk_size);
        assert_eq!(frames, vec![r#"{"ok":1}"#.to_owned()]);
        assert_eq!(violation, Some(ProtocolViolationKind::NotJson));
    }
}
