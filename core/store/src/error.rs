//! One error type for the whole store. Nothing in this crate panics on a
//! bad database, a bad profile directory, or a hostile adapter reply.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("money: {0}")]
    Money(#[from] sumer_money::MoneyError),

    #[error("adapter: {0}")]
    Host(String),

    /// A second writing command is already running against this profile.
    /// The CLI turns this into exit status 3.
    #[error("another sumer process is writing to the profile at {0}")]
    ProfileLocked(PathBuf),

    #[error("no profile at {0} -- run `sumer init` first")]
    NoProfile(PathBuf),

    #[error("profile at {path} is schema version {found}, this build speaks {expected}")]
    SchemaVersion {
        path: PathBuf,
        found: i64,
        expected: i64,
    },

    /// A row this build wrote came back in a shape it cannot read: a
    /// corrupted database, or a file another version wrote. Never a panic.
    #[error("stored row is unreadable: {0}")]
    CorruptRow(String),

    #[error("{0}")]
    Usage(String),
}

impl From<sumer_host::HostError> for StoreError {
    fn from(e: sumer_host::HostError) -> StoreError {
        StoreError::Host(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
