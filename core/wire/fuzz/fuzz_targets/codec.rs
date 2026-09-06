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
//!     sequence of 1-byte chunks must produce the identical frame sequence
//!     (contents, not just a matching count) and agree on the exact
//!     violation, if any (its `kind`, not just whether one occurred).
//!
//! Run with `cargo fuzz run codec` from `core/wire/` (requires the
//! `cargo-fuzz` subcommand and a nightly toolchain; wired into
//! `.github/workflows/nightly.yml`).

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

    // Contents, not just counts: a decoder that mangles a frame's bytes
    // while still splitting the input into the right number of pieces
    // must not pass. Likewise the violation's exact `kind`, not just
    // whether one occurred at all -- a decoder that reports the right
    // *number* of errors but classifies one of them wrong (`NotJson`
    // where the other found `NonUtf8`, say) must not pass either.
    assert_eq!(
        whole_frames, split_frames,
        "whole-input and byte-by-byte decoding produced different frame contents"
    );
    assert_eq!(
        whole_result, split_result,
        "whole-input and byte-by-byte decoding disagreed on the violation"
    );
});
