//! The profile directory: where the database lives, and the single-writer
//! lock that guards it.
//!
//! **Why one writer.** `refresh` loads a resource's live set at the start
//! of a sweep and derives retractions from it at the end. Two writers --
//! cron and a manual run, say -- each load that set, each assign revisions
//! from it, and the one that finishes second writes an *older* crawl's
//! conclusions over the newer one's: a record the first correctly retracted
//! comes back live, or two chains claim the same revision. PR 3 died of an
//! adapter racing itself on a file; this is the same class one layer up,
//! and the fix is the same shape -- refuse, do not merge.
//!
//! The lock is `std::fs::File::try_lock` on `<profile>/lock`. Stdlib, no
//! dependency, released by the kernel when the process dies however it
//! dies -- which a lock file holding a PID is not.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};

/// What `sumer init` prints. It names full-disk encryption **by name**
/// because that is the correct tool for the loss this store actually
/// faces, and it says what the file modes do and do not buy, because
/// `0700` is worthless the moment the directory is synced somewhere.
pub const INIT_NOTICE: &str = "\
Profile created.

  What is stored here: your full transaction history and, for a Bitcoin
  wallet, its address set. No credentials -- Milestone 1 is watch-only.

  This database is NOT encrypted. The two realistic losses are a stolen or
  discarded disk and a profile directory synced to a cloud backup.

  * For the disk: use FULL-DISK ENCRYPTION (LUKS on Linux, FileVault on
    macOS, BitLocker on Windows). That is the correct tool, and nothing
    this program can do replaces it.
  * For the backup: this directory is 0700 and the database 0600, which
    keeps another local user out and does NOT survive being copied into a
    backup that is readable elsewhere. Exclude it, or encrypt the backup.

  Application-level encryption ships at Milestone 3, with the first stored
  credential. See adr/0005.";

/// A profile directory.
#[derive(Debug, Clone)]
pub struct Profile {
    dir: PathBuf,
}

impl Profile {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Profile {
        Profile { dir: dir.into() }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.dir.join("sumer.db")
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("lock")
    }

    /// Creates the directory (0700) if it is not already there. The
    /// database file's own 0600 is set when it is first opened.
    pub fn create_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        set_mode(&self.dir, 0o700)?;
        Ok(())
    }

    /// Takes the exclusive writer lock for the whole run. Every writing
    /// command (`connect`, `refresh`, `import`) takes it; read-only
    /// commands do not, so `history` still answers while a refresh runs.
    ///
    /// # Errors
    /// [`StoreError::ProfileLocked`] if another process holds it.
    pub fn lock(&self) -> Result<ProfileLock> {
        self.create_dir()?;
        let file = File::create(self.lock_path())?;
        set_mode(&self.lock_path(), 0o600)?;
        match file.try_lock() {
            Ok(()) => Ok(ProfileLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(StoreError::ProfileLocked(self.dir.clone()))
            }
            Err(std::fs::TryLockError::Error(e)) => Err(StoreError::Io(e)),
        }
    }
}

/// Held for the duration of a writing command. Dropping it closes the
/// file, which releases the lock.
#[derive(Debug)]
pub struct ProfileLock {
    _file: File,
}

/// Best-effort on platforms with Unix permissions; a no-op elsewhere.
/// Failing to tighten a mode is reported, not swallowed: a profile that is
/// world-readable when it claimed otherwise is a lie the user acted on.
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
