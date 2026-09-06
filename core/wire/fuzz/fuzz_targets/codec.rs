//! Fuzzes the incremental JSON-Lines framer (`sumer_wire::FrameDecoder`)
//! directly on arbitrary bytes.
//!
//! Invariants under fuzzing (see `core/wire/src/codec.rs` for the full
//! rationale):
//!   - never panics on any input;
//!   - the in-progress buffer never grows past `MAX_FRAME_BYTES + 1` bytes
//!     (enforced by construction: `FrameDecoder::new` reserves exactly that
//!     capacity once and never grows it further);
//!   - split-invariance: feeding the fuzz input as one chunk and as a
//!     sequence of 1-byte chunks must produce the same number of frames and
//!     agree on whether a violation occurred.
//!
//! Run with `cargo fuzz run codec` from `core/wire/fuzz/` (requires the
//! `cargo-fuzz` subcommand and a nightly toolchain -- see this crate's PR
//! report for why that could not be exercised in this environment).

#![no_main]

use libfuzzer_sys::fuzz_target;
use sumer_wire::FrameDecoder;

fuzz_target!(|data: &[u8]| {
    let mut whole = FrameDecoder::new();
    let mut whole_frames = Vec::new();
    let whole_result = whole.push(data, &mut whole_frames);

    let mut split = FrameDecoder::new();
    let mut split_frames = Vec::new();
    let mut split_result = Ok(());
    for byte in data {
        if split_result.is_err() {
            break;
        }
        split_result = split.push(std::slice::from_ref(byte), &mut split_frames);
    }

    assert_eq!(
        whole_frames.len(),
        split_frames.len(),
        "whole-input and byte-by-byte decoding produced different frame counts"
    );
    assert_eq!(
        whole_result.is_err(),
        split_result.is_err(),
        "whole-input and byte-by-byte decoding disagreed on whether a violation occurred"
    );
});
