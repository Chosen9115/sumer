//! `sumer-conformance`: the black-box conformance suite for the Sumer wire
//! protocol (spec/wire.md, spec/observation.md). See `conformance/README.md`
//! for how to run it against your own adapter, in any language.

pub mod assert;
mod exec;
mod ledger;
pub mod runner;
