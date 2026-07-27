//! Cross-process exclusive ownership for persistent stores (#489).
//!
//! The database door and the destructive-reset door acquire the same durable
//! sidecar file lock. The lock is owned by the [`RedbStore`](crate::RedbStore)
//! value itself, so no process-global registry, caller convention, or
//! engine-only construction path can bypass it: one resolved store target has
//! exactly one owner at a time, in this process and every other one.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Resolve the stable target identity used by both open and reset. Existing
/// files (and existing final symlinks) canonicalize completely. A missing
/// ordinary final component canonicalizes its existing parent. A dangling
/// final symlink follows its target, including relative targets and chains,
/// so pre-create and post-create identities converge.
fn resolve_store_path(path: &Path) -> io::Result<PathBuf> {
    let mut candidate = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::canonicalize(&candidate) {
            Ok(path) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let target = std::fs::read_link(&candidate)?;
                        candidate = if target.is_absolute() {
                            target
                        } else {
                            candidate
                                .parent()
                                .filter(|parent| !parent.as_os_str().is_empty())
                                .unwrap_or_else(|| Path::new("."))
                                .join(target)
                        };
                    }
                    Ok(_) => return Err(error),
                    Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                        let file_name = candidate.file_name().ok_or(error)?;
                        let parent = candidate
                            .parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .unwrap_or_else(|| Path::new("."));
                        return Ok(std::fs::canonicalize(parent)?.join(file_name));
                    }
                    Err(metadata_error) => return Err(metadata_error),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "persistent store symlink chain exceeds 40 links",
    ))
}

/// The sidecar deliberately outlives the database file, including across a
/// destructive reset. Keeping one stable inode per canonical target is
/// load-bearing: deleting and recreating the database file must not mint a
/// second lock identity while an older database handle is still alive.
///
/// The sidecar is mechanism state — it holds no store content, has no schema,
/// and is never read.
fn ownership_sidecar_path(target: &Path) -> io::Result<PathBuf> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "persistent store target has no parent directory",
        )
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(target.as_os_str().as_encoded_bytes());
    Ok(parent.join(format!(
        ".nmp-store-owner-{}.lock",
        hasher.finalize().to_hex()
    )))
}

/// Every way acquiring exclusive ownership of one target can fail. Both
/// public door errors project this one set, so open and reset cannot drift
/// into differently-shaped refusals for the same mechanism.
#[derive(Debug)]
pub(crate) enum StoreOwnershipError {
    Resolve { path: PathBuf, source: io::Error },
    SidecarOpen { path: PathBuf, source: io::Error },
    AlreadyOpen { path: PathBuf },
    Lock { path: PathBuf, source: io::Error },
    TargetChanged { expected: PathBuf, actual: PathBuf },
}

/// The one exclusive owner of a resolved persistent-store target.
///
/// This value deliberately contains the live locked file handle rather than
/// an in-memory registration token. Dropping it releases the OS lock in every
/// process, including on unwind, on partial construction, and on process
/// death — none of which an in-process registry could have covered.
pub(crate) struct StoreOwnership {
    target: PathBuf,
    sidecar: File,
}

/// Releasing by closing the descriptor is NOT enough (#936).
///
/// The lock lives on the *open file description*, not on the descriptor. Any
/// child forked while this owner is alive — including the transient child
/// every `Command::spawn` creates between `fork` and `exec` — inherits a
/// descriptor onto that same description, so the lock survives until the last
/// of them is closed. `FD_CLOEXEC` (which Rust already sets) cannot close that
/// window, because it acts at `exec`, not at `fork`. The consequence is the
/// #936 regression: a sequential drop-then-reopen of one path, in one process,
/// with no concurrency of its own, could be refused with `StoreAlreadyOpen`
/// because an unrelated concurrent spawn was holding a duplicate descriptor.
///
/// An explicit unlock releases the lock on the shared description itself, so
/// it takes effect no matter how many inherited duplicates exist. `redb` does
/// exactly this for the database file it locks alongside this sidecar; without
/// it, only half of the pair was fork-safe.
///
/// Nothing wants a forked child to inherit ownership: a child that means to
/// own the target opens the sidecar itself and takes its own lock.
impl Drop for StoreOwnership {
    fn drop(&mut self) {
        // Best effort by necessity — drop cannot report. Closing the
        // descriptor immediately afterwards remains the backstop for the
        // single-descriptor case, and process death remains the backstop for
        // everything else.
        let _ = self.sidecar.unlock();
    }
}

impl StoreOwnership {
    /// The canonical target this ownership protects. Callers open and delete
    /// exactly this path, never the alias they were handed.
    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    /// Re-resolve the caller path after the lock is held, after database
    /// open, and immediately before destructive removal. A symlink retarget
    /// racing acquisition cannot make the lock protect one target while the
    /// operation touches another.
    pub(crate) fn revalidate(&self, path: &Path) -> Result<(), StoreOwnershipError> {
        let actual = resolve_store_path(path).map_err(|source| StoreOwnershipError::Resolve {
            path: path.to_path_buf(),
            source,
        })?;
        if actual == self.target {
            Ok(())
        } else {
            Err(StoreOwnershipError::TargetChanged {
                expected: self.target.clone(),
                actual,
            })
        }
    }
}

fn acquire_ownership(path: &Path) -> Result<StoreOwnership, StoreOwnershipError> {
    acquire_ownership_with_hooks(path, || {})
}

fn acquire_ownership_with_hooks(
    path: &Path,
    after_lock: impl FnOnce(),
) -> Result<StoreOwnership, StoreOwnershipError> {
    let target = resolve_store_path(path).map_err(|source| StoreOwnershipError::Resolve {
        path: path.to_path_buf(),
        source,
    })?;
    let sidecar_path =
        ownership_sidecar_path(&target).map_err(|source| StoreOwnershipError::Resolve {
            path: target.clone(),
            source,
        })?;
    let sidecar = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&sidecar_path)
        .map_err(|source| StoreOwnershipError::SidecarOpen {
            path: sidecar_path.clone(),
            source,
        })?;
    // Nonblocking by construction: a live owner is a typed refusal, never a
    // caller that silently waits behind another process.
    match sidecar.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(StoreOwnershipError::AlreadyOpen { path: target });
        }
        Err(std::fs::TryLockError::Error(source)) => {
            return Err(StoreOwnershipError::Lock {
                path: sidecar_path,
                source,
            });
        }
    }
    // The handle is bound to `target` the instant the lock is held, and
    // before anything that can fail or unwind, so every later exit — a
    // failing revalidation, a panicking hook — drops it and releases the
    // lock exactly once instead of leaking an orphan owner.
    let ownership = StoreOwnership { target, sidecar };
    after_lock();
    ownership.revalidate(path)?;
    Ok(ownership)
}

/// Every reachable failure from opening one persistent database.
#[derive(Debug)]
pub enum RedbStoreOpenError {
    /// Another live owner — in this process or any other — holds the same
    /// canonical store target. No second database owner was created.
    StoreAlreadyOpen { path: PathBuf },
    /// The caller path could not be resolved to a stable target identity.
    PathResolutionFailed { path: PathBuf, source: io::Error },
    /// The durable ownership sidecar could not be created or opened.
    LockFileOpenFailed { path: PathBuf, source: io::Error },
    /// The OS refused the exclusive lock for a reason other than contention.
    LockFailed { path: PathBuf, source: io::Error },
    /// The caller path resolved to a different target while the operation was
    /// in flight. Nothing was exposed and the partial ownership was released.
    TargetChanged { expected: PathBuf, actual: PathBuf },
    /// The target's durable bytes are not the exact current schema epoch
    /// (#867). NMP carries no persistent-schema compatibility obligation in
    /// this architecture cut: there is no pre-current decoder to fall back on,
    /// so this is the ONE outcome for any nonempty non-current store, raised
    /// before a `RedbStore` is exposed and before a single byte is mutated.
    /// Nothing was migrated, adopted, aliased, or reset — the caller decides
    /// whether to recreate the store.
    ///
    /// It is deliberately NOT a `Database` error: corruption of the CURRENT
    /// epoch stays `Database(redb::Error::Corrupted(..))`, so an operator can
    /// never read "unsupported schema" and conclude their current-epoch data
    /// was merely old, nor read "corrupted" and conclude a recreate is enough.
    ///
    /// `found` is the marker actually present: `None` when the store predates
    /// the schema marker entirely.
    UnsupportedSchema {
        path: PathBuf,
        expected: u64,
        found: Option<u64>,
    },
    /// The database itself refused the open (corruption, I/O, redb-level
    /// misuse). Schema-epoch refusal is [`Self::UnsupportedSchema`], never
    /// this.
    Database(redb::Error),
}

impl RedbStoreOpenError {
    /// redb's own single-process exclusion is a second owner refusing a
    /// second owner. Project it as the same typed fact rather than leaking a
    /// mechanism-specific error for the case the sidecar already covers.
    pub(crate) fn database(error: redb::Error, target: &Path) -> Self {
        if matches!(error, redb::Error::DatabaseAlreadyOpen) {
            Self::StoreAlreadyOpen {
                path: target.to_path_buf(),
            }
        } else {
            Self::Database(error)
        }
    }
}

impl From<redb::Error> for RedbStoreOpenError {
    fn from(error: redb::Error) -> Self {
        Self::Database(error)
    }
}

macro_rules! redb_open_error_from {
    ($($error:ty),+ $(,)?) => {
        $(
            impl From<$error> for RedbStoreOpenError {
                fn from(error: $error) -> Self {
                    Self::Database(error.into())
                }
            }
        )+
    };
}

redb_open_error_from!(
    redb::CommitError,
    redb::DatabaseError,
    redb::StorageError,
    redb::TableError,
    redb::TransactionError,
);

impl From<StoreOwnershipError> for RedbStoreOpenError {
    fn from(error: StoreOwnershipError) -> Self {
        match error {
            StoreOwnershipError::Resolve { path, source } => {
                Self::PathResolutionFailed { path, source }
            }
            StoreOwnershipError::SidecarOpen { path, source } => {
                Self::LockFileOpenFailed { path, source }
            }
            StoreOwnershipError::AlreadyOpen { path } => Self::StoreAlreadyOpen { path },
            StoreOwnershipError::Lock { path, source } => Self::LockFailed { path, source },
            StoreOwnershipError::TargetChanged { expected, actual } => {
                Self::TargetChanged { expected, actual }
            }
        }
    }
}

impl std::fmt::Display for RedbStoreOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreAlreadyOpen { path } => {
                write!(f, "persistent store is already open: {}", path.display())
            }
            Self::PathResolutionFailed { path, source } => write!(
                f,
                "could not resolve persistent store target {}: {source}",
                path.display()
            ),
            Self::LockFileOpenFailed { path, source } => write!(
                f,
                "could not open persistent store ownership lock {}: {source}",
                path.display()
            ),
            Self::LockFailed { path, source } => write!(
                f,
                "could not acquire persistent store ownership lock {}: {source}",
                path.display()
            ),
            Self::TargetChanged { expected, actual } => write!(
                f,
                "persistent store target changed during open: {} -> {}",
                expected.display(),
                actual.display()
            ),
            Self::UnsupportedSchema {
                path,
                expected,
                found,
            } => match found {
                Some(found) => write!(
                    f,
                    "persistent store {} is schema epoch {found}, not the one supported epoch {expected}; \
                     it was not migrated, adopted, or reset",
                    path.display()
                ),
                None => write!(
                    f,
                    "persistent store {} predates the schema marker and is not the one supported \
                     epoch {expected}; it was not migrated, adopted, or reset",
                    path.display()
                ),
            },
            Self::Database(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RedbStoreOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PathResolutionFailed { source, .. }
            | Self::LockFileOpenFailed { source, .. }
            | Self::LockFailed { source, .. } => Some(source),
            Self::Database(error) => Some(error),
            Self::StoreAlreadyOpen { .. }
            | Self::TargetChanged { .. }
            | Self::UnsupportedSchema { .. } => None,
        }
    }
}

/// Acquire the one exclusive owner token a `RedbStore` holds for its
/// lifetime. Nothing may open the database before this succeeds.
pub(crate) fn acquire_for_open(path: &Path) -> Result<StoreOwnership, RedbStoreOpenError> {
    acquire_ownership(path).map_err(Into::into)
}

/// Every reachable failure from destructively resetting one store.
#[derive(Debug)]
pub enum RedbStoreResetError {
    /// A live owner — in this process or any other — still holds the target.
    /// Nothing was removed.
    StoreStillOpen { path: PathBuf },
    /// The caller path could not be resolved to a stable target identity.
    PathResolutionFailed { path: PathBuf, source: io::Error },
    /// The durable ownership sidecar could not be created or opened.
    LockFileOpenFailed { path: PathBuf, source: io::Error },
    /// The OS refused the exclusive lock for a reason other than contention.
    LockFailed { path: PathBuf, source: io::Error },
    /// The caller path resolved to a different target while reset held its
    /// lock. Nothing was deleted.
    TargetChanged { expected: PathBuf, actual: PathBuf },
    /// The exclusive owner could not remove the resolved target.
    RemoveFailed { path: PathBuf, source: io::Error },
}

impl From<StoreOwnershipError> for RedbStoreResetError {
    fn from(error: StoreOwnershipError) -> Self {
        match error {
            StoreOwnershipError::Resolve { path, source } => {
                Self::PathResolutionFailed { path, source }
            }
            StoreOwnershipError::SidecarOpen { path, source } => {
                Self::LockFileOpenFailed { path, source }
            }
            StoreOwnershipError::AlreadyOpen { path } => Self::StoreStillOpen { path },
            StoreOwnershipError::Lock { path, source } => Self::LockFailed { path, source },
            StoreOwnershipError::TargetChanged { expected, actual } => {
                Self::TargetChanged { expected, actual }
            }
        }
    }
}

impl std::fmt::Display for RedbStoreResetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreStillOpen { path } => {
                write!(f, "persistent store is still open: {}", path.display())
            }
            Self::PathResolutionFailed { path, source } => write!(
                f,
                "could not resolve persistent store target {}: {source}",
                path.display()
            ),
            Self::LockFileOpenFailed { path, source } => write!(
                f,
                "could not open persistent store ownership lock {}: {source}",
                path.display()
            ),
            Self::LockFailed { path, source } => write!(
                f,
                "could not acquire persistent store ownership lock {}: {source}",
                path.display()
            ),
            Self::TargetChanged { expected, actual } => write!(
                f,
                "persistent store target changed during reset: {} -> {}",
                expected.display(),
                actual.display()
            ),
            Self::RemoveFailed { path, source } => write!(
                f,
                "could not remove persistent store {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for RedbStoreResetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PathResolutionFailed { source, .. }
            | Self::LockFileOpenFailed { source, .. }
            | Self::LockFailed { source, .. }
            | Self::RemoveFailed { source, .. } => Some(source),
            Self::StoreStillOpen { .. } | Self::TargetChanged { .. } => None,
        }
    }
}

pub(crate) fn reset_store(path: &Path) -> Result<(), RedbStoreResetError> {
    reset_store_with_hooks(path, || {})
}

/// Reset acquires the SAME exclusive ownership an open would and holds it
/// through removal. There is no check-then-delete window: the owner token is
/// still live when `remove_file` runs and is released only afterwards.
fn reset_store_with_hooks(
    path: &Path,
    before_remove: impl FnOnce(),
) -> Result<(), RedbStoreResetError> {
    let ownership = acquire_ownership(path)?;
    ownership.revalidate(path)?;
    before_remove();
    let target = ownership.target().to_path_buf();
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RedbStoreResetError::RemoveFailed {
            path: target,
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::{mpsc, Arc, Barrier};
    use std::thread;

    #[test]
    fn one_owner_refuses_second_open_and_reset_until_drop() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("one-owner.redb");
        let owner = crate::RedbStore::open(&path).unwrap();

        assert!(matches!(
            crate::RedbStore::open(&path),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        assert!(matches!(
            reset_store(&path),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        assert!(path.exists(), "a refused reset must not touch the target");

        drop(owner);
        let replacement = crate::RedbStore::open(&path).unwrap();
        drop(replacement);
        reset_store(&path).unwrap();
        assert!(!path.exists());
    }

    /// Fork a child that outlives the caller and holds every descriptor it
    /// inherited. Returns the pid; the child runs no destructor and touches
    /// no store byte.
    ///
    /// SAFETY: the child executes nothing but `nanosleep` and `_exit`, both
    /// async-signal-safe, so forking this multi-threaded binary is sound.
    #[cfg(unix)]
    fn fork_descriptor_holder() -> libc::pid_t {
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", io::Error::last_os_error());
        if child == 0 {
            let hold = libc::timespec {
                tv_sec: 120,
                tv_nsec: 0,
            };
            unsafe {
                libc::nanosleep(&hold, std::ptr::null_mut());
                libc::_exit(0);
            }
        }
        child
    }

    #[cfg(unix)]
    fn reap_descriptor_holder(child: libc::pid_t) {
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, std::ptr::null_mut(), 0);
        }
    }

    /// #936, the open/reset half. The lock lives on the open file
    /// description, so every child forked while a store is open inherits a
    /// descriptor onto it — including the transient child inside every
    /// `Command::spawn`, between `fork` and `exec`, which `FD_CLOEXEC` cannot
    /// cover. Releasing by closing the owner's own descriptor therefore left
    /// the lock alive in those children, and the next open of the same path
    /// was refused with `StoreAlreadyOpen` — sequentially, in one process,
    /// with no concurrency of the caller's own.
    ///
    /// A live forked child is the deterministic form of that window: it holds
    /// the inherited descriptor for far longer than the reopen below takes.
    #[cfg(unix)]
    #[test]
    fn a_forked_child_cannot_hold_the_sidecar_lock_past_the_owner_drop() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("forked-child.redb");
        let owner = crate::RedbStore::open(&path).unwrap();
        let child = fork_descriptor_holder();

        drop(owner);
        // Both ownership doors, taken sequentially by this one process while
        // the child is still alive and still holding its inherited
        // descriptor.
        let reopened = crate::RedbStore::open(&path);
        let reopened_is_owned = reopened.is_ok();
        drop(reopened);
        let reset = reset_store(&path);
        reap_descriptor_holder(child);

        assert!(
            reopened_is_owned,
            "a sequential reopen must never be refused because a forked child \
             inherited the sidecar descriptor"
        );
        assert!(
            reset.is_ok(),
            "the reset door must be released by the same drop: {reset:?}"
        );
        assert!(!path.exists());

        // Cross-process exclusion is unchanged: the release is the owner's
        // drop, not the child's exit, and a live owner still refuses.
        let owner = crate::RedbStore::open(&path).unwrap();
        assert!(matches!(
            crate::RedbStore::open(&path),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        drop(owner);
    }

    /// #936, the fail-closed half — the shape #936's original report caught
    /// in `target_retargeted_during_lock_acquisition_fails_closed`, made
    /// deterministic. The child is forked while the doomed acquisition holds
    /// the lock, so it inherits that descriptor; the acquisition then fails
    /// closed and drops its partial ownership. Releasing by close alone left
    /// the abandoned lock alive in the child, and the immediately following
    /// acquisition of the same target was refused with `AlreadyOpen`.
    #[cfg(unix)]
    #[test]
    fn a_fail_closed_acquisition_releases_its_lock_past_a_forked_child() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("fail-closed-first.redb");
        let second = fixture.path().join("fail-closed-second.redb");
        let alias = fixture.path().join("fail-closed-alias.redb");
        symlink(&first, &alias).unwrap();

        let mut child = 0;
        let result = acquire_ownership_with_hooks(&alias, || {
            child = fork_descriptor_holder();
            std::fs::remove_file(&alias).unwrap();
            symlink(&second, &alias).unwrap();
        });
        assert!(matches!(
            result,
            Err(StoreOwnershipError::TargetChanged { .. })
        ));

        let owner = acquire_ownership(&first);
        reap_descriptor_holder(child);
        assert!(
            owner.is_ok(),
            "a fail-closed acquisition must release its lock even though a \
             child forked mid-acquisition inherited the sidecar descriptor: \
             {:?}",
            owner.err()
        );
    }

    #[test]
    fn a_relative_alias_of_a_live_store_is_refused() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("relative-alias.redb");
        let owner = crate::RedbStore::open(&path).unwrap();
        let alias = fixture.path().join(".").join("relative-alias.redb");

        assert!(matches!(
            crate::RedbStore::open(&alias),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        assert!(matches!(
            reset_store(&alias),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        drop(owner);
        reset_store(&alias).unwrap();
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_and_dangling_final_symlinks_share_the_target_owner() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target.redb");
        let existing_alias = fixture.path().join("existing-alias.redb");
        let owner = crate::RedbStore::open(&target).unwrap();
        symlink(&target, &existing_alias).unwrap();
        assert!(matches!(
            crate::RedbStore::open(&existing_alias),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        assert!(matches!(
            reset_store(&existing_alias),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        drop(owner);
        reset_store(&existing_alias).unwrap();

        // A pre-create dangling final symlink must select the SAME owner
        // identity the created target later resolves to.
        let dangling_target = fixture.path().join("created-through-target.redb");
        let dangling_alias = fixture.path().join("dangling-alias.redb");
        symlink("created-through-target.redb", &dangling_alias).unwrap();
        let owner = crate::RedbStore::open(&dangling_alias).unwrap();
        assert!(matches!(
            crate::RedbStore::open(&dangling_target),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        assert!(matches!(
            reset_store(&dangling_target),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        drop(owner);
        reset_store(&dangling_alias).unwrap();
        assert!(!dangling_target.exists());
        assert!(
            std::fs::symlink_metadata(&dangling_alias).is_ok(),
            "reset removes the resolved store target, not the alias inode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn target_retargeted_during_lock_acquisition_fails_closed() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first.redb");
        let second = fixture.path().join("second.redb");
        let alias = fixture.path().join("alias.redb");
        symlink(&first, &alias).unwrap();

        let result = acquire_ownership_with_hooks(&alias, || {
            std::fs::remove_file(&alias).unwrap();
            symlink(&second, &alias).unwrap();
        });
        assert!(matches!(
            result,
            Err(StoreOwnershipError::TargetChanged { expected, actual })
                if expected == resolve_store_path(&first).unwrap()
                    && actual == resolve_store_path(&second).unwrap()
        ));
        assert!(
            !first.exists() && !second.exists(),
            "a fail-closed acquisition creates no database"
        );

        // The failed acquisition released its lock exactly once, so the same
        // target is immediately ownable.
        let owner = acquire_ownership(&first).expect("failed acquisition released its lock");
        drop(owner);
    }

    #[test]
    fn concurrent_openers_have_exactly_one_owner() {
        let fixture = tempfile::tempdir().unwrap();
        let path = Arc::new(fixture.path().join("raced.redb"));
        let start = Arc::new(Barrier::new(9));
        let release = Arc::new(Barrier::new(2));
        let (winner_tx, winner_rx) = mpsc::sync_channel(1);
        let threads = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let start = Arc::clone(&start);
                let release = Arc::clone(&release);
                let winner_tx = winner_tx.clone();
                thread::spawn(move || {
                    start.wait();
                    match crate::RedbStore::open(path.as_path()) {
                        Ok(store) => {
                            winner_tx.send(()).unwrap();
                            release.wait();
                            drop(store);
                            true
                        }
                        Err(RedbStoreOpenError::StoreAlreadyOpen { .. }) => false,
                        Err(error) => panic!("unexpected open result: {error}"),
                    }
                })
            })
            .collect::<Vec<_>>();
        drop(winner_tx);
        start.wait();
        winner_rx.recv().unwrap();
        assert!(winner_rx.try_recv().is_err());
        release.wait();
        let winners = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn racing_open_against_reset_leaves_one_owner_and_no_lost_bytes() {
        let fixture = tempfile::tempdir().unwrap();
        let path = Arc::new(fixture.path().join("open-vs-reset.redb"));
        drop(crate::RedbStore::open(path.as_path()).unwrap());
        let start = Arc::new(Barrier::new(2));

        let open_path = Arc::clone(&path);
        let open_start = Arc::clone(&start);
        let opener = thread::spawn(move || {
            open_start.wait();
            crate::RedbStore::open(open_path.as_path())
        });
        let reset_path = Arc::clone(&path);
        let reset_start = Arc::clone(&start);
        let resetter = thread::spawn(move || {
            reset_start.wait();
            reset_store(reset_path.as_path())
        });

        let opened = opener.join().unwrap();
        let reset = resetter.join().unwrap();
        match (opened, reset) {
            // Reset ran first: the file was removed, then recreated by the
            // opener. Never a half-deleted store handed to a live owner.
            (Ok(store), Ok(())) => {
                assert!(path.exists());
                drop(store);
            }
            (Ok(store), Err(RedbStoreResetError::StoreStillOpen { .. })) => {
                assert!(path.exists(), "a refused reset must leave the bytes");
                drop(store);
            }
            (Err(RedbStoreOpenError::StoreAlreadyOpen { .. }), Ok(())) => {
                assert!(!path.exists(), "the winning reset removed the target");
            }
            (opened, reset) => panic!(
                "exactly one side must own the target: open={} reset={reset:?}",
                if opened.is_ok() { "ok" } else { "err" }
            ),
        }
    }

    #[test]
    fn schema_failure_releases_database_and_sidecar_ownership() {
        const FOREIGN_TABLE: redb::TableDefinition<&str, &str> =
            redb::TableDefinition::new("foreign_table");

        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("unsupported-schema.redb");
        let db = redb::Database::create(&path).unwrap();
        let write = db.begin_write().unwrap();
        write.open_table(FOREIGN_TABLE).unwrap();
        write.commit().unwrap();
        drop(db);

        // The exact current epoch is pinned by `redb_store::tests`; this test
        // owns only the lifetime consequence of the unsupported-schema
        // refusal, so it deliberately does not restate the version number.
        assert!(matches!(
            crate::RedbStore::open(&path),
            Err(RedbStoreOpenError::UnsupportedSchema { .. })
        ));
        reset_store(&path).expect("schema refusal must release database then sidecar ownership");
        assert!(!path.exists());
    }

    /// Child-only body driven by
    /// `subprocess_owner_is_refused_through_direct_relative_and_symlink_aliases`.
    /// Under an ordinary test run the environment variable is absent and this
    /// returns immediately.
    #[test]
    fn subprocess_owner_helper() {
        let Some(path) = std::env::var_os("NMP_STORE_OWNER_HELPER_PATH") else {
            return;
        };
        let _store = crate::RedbStore::open(PathBuf::from(path)).unwrap();
        println!("NMP_STORE_OWNER_READY");
        std::io::stdout().flush().unwrap();
        // Hold ownership until the parent closes our stdin.
        let mut byte = [0u8; 1];
        let _ = std::io::stdin().read(&mut byte);
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_owner_is_refused_through_direct_relative_and_symlink_aliases() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("cross-process.redb");
        let alias = fixture.path().join("alias.redb");
        symlink("cross-process.redb", &alias).unwrap();

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("persistent_store_lifetime::tests::subprocess_owner_helper")
            .arg("--nocapture")
            .current_dir(fixture.path())
            // A relative path in the child: the owner identity must not
            // depend on the spelling either side used.
            .env("NMP_STORE_OWNER_HELPER_PATH", "cross-process.redb")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = std::io::BufReader::new(stdout).lines();
        loop {
            let line = lines
                .next()
                .expect("child exited before taking ownership")
                .unwrap();
            if line.contains("NMP_STORE_OWNER_READY") {
                break;
            }
        }

        let before = std::fs::read(&target).expect("the child's store must be readable");
        assert!(matches!(
            crate::RedbStore::open(&target),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        assert!(matches!(
            crate::RedbStore::open(&alias),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        assert!(matches!(
            reset_store(&alias),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        assert!(matches!(
            reset_store(&target),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        assert_eq!(
            std::fs::read(&target).expect("a refused reset must leave the store readable"),
            before,
            "a refused cross-process reset must not touch the bytes"
        );

        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());

        // Once the owning process exits, exactly one new opener succeeds and
        // reset succeeds only after that owner is gone in turn.
        let reopened = crate::RedbStore::open(&alias).unwrap();
        assert!(matches!(
            crate::RedbStore::open(&target),
            Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
        ));
        drop(reopened);
        reset_store(&target).unwrap();
        assert!(!target.exists());
    }
}
