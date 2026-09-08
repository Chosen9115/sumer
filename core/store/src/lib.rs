//! `sumer-store`: the local store (ADR 0002), the sweep that fills it, and
//! the host-derived retraction that keeps it honest about absence.
//!
//! Three things live here that live nowhere else.
//!
//! **The sweep gate** ([`sweep`]): nine conditions that decide whether one
//! `history.read` is strong enough evidence to conclude that a record the
//! store holds live is *gone*. Fail any one and the observations still
//! persist -- they are evidence -- but nothing is retracted.
//!
//! **The retraction table** ([`schema`]): absence is host-authored and has
//! its own table. A host cannot author an `Observation`: it has no amount,
//! no surface, no posting and no provider provenance, and fabricating them
//! would be fabricating provider evidence. A record is live iff its chain's
//! highest revision exceeds every retraction revision for its key, which is
//! why revival needs no special case anywhere.
//!
//! **The single writer** ([`profile`]): every writing command holds an
//! exclusive lock on the profile for its whole run. Two refreshes that each
//! load a live set and each derive retractions from it will write one
//! another's conclusions away.
//!
//! This crate is the first caller of [`sumer_host::fold`] and
//! [`sumer_host::paging`], which until now had none.

#![forbid(unsafe_code)]

pub mod error;
pub mod hash;
pub mod profile;
pub mod refresh;
pub mod render;
pub mod schema;
pub mod store;
pub mod sweep;

pub use error::{Result, StoreError};
pub use profile::{Profile, ProfileLock};
pub use store::Store;

/// The host's own clock. One implementation, in `sumer_host::time`: this
/// crate had a second copy of the same twenty lines and the two had already
/// drifted -- the host's panicked on the arm it calls unreachable, this
/// one silently fell back to the Unix epoch, which is how a wrong
/// `retracted_at` reaches an audit row. Re-exported rather than wrapped so
/// there is nothing here to drift again.
pub use sumer_host::time::now_rfc3339;
