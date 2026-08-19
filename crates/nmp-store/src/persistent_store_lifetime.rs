//! Cross-process exclusive ownership for persistent stores (#489).
//!
//! The database door and the destructive-reset door acquire the same durable
//! sidecar file lock. A production database open then acquires NMP's required
//! target-inode lock; reset joins that same lock before removal, so a hard-link
//! alias cannot bypass live backend ownership. The locks are owned by RAII
//! values, so no process-global registry, caller convention, or engine-only
//! construction path can bypass them.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// How many times resolution may follow a symlink or re-read a component that
/// changed under it before giving up. It bounds progress, not link depth
/// alone (#1001).
const MAX_RESOLUTION_STEPS: usize = 40;

/// Resolve the stable target identity used by both open and reset. Existing
/// files (and existing final symlinks) canonicalize completely. A missing
/// ordinary final component canonicalizes its existing parent. A dangling
/// final symlink follows its target, including relative targets and chains,
/// so pre-create and post-create identities converge.
///
/// Resolution is a sequence of syscalls over a filesystem other threads and
/// processes are mutating, so each step re-reads what it needs rather than
/// trusting a previous step's answer. Both outcomes for a final component are
/// legitimate -- the canonical path of a file that exists, or the
/// parent-derived path of one that does not -- but a `NotFound` observed
/// mid-flight is never one of them (#1001).
fn resolve_store_path(path: &Path) -> io::Result<PathBuf> {
    let mut candidate = path.to_path_buf();
    for _ in 0..MAX_RESOLUTION_STEPS {
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
                    // #1001: the entry appeared between the two syscalls --
                    // `canonicalize` saw nothing, and by the time
                    // `symlink_metadata` ran another opener had created the
                    // target. This is a plain time-of-check window, not an
                    // impossible state: a non-symlink that exists now is one
                    // `canonicalize` away from resolving, so re-resolve
                    // instead of surfacing the `NotFound` we no longer
                    // believe. Returning it made a racing `RedbStore::open`
                    // fail with `ENOENT` rather than the typed
                    // `StoreAlreadyOpen` it is owed.
                    Ok(_) => continue,
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
        format!(
            "persistent store path did not resolve in {MAX_RESOLUTION_STEPS} steps: a symlink \
             chain that long, or a final component being created and removed without pause"
        ),
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
/// it takes effect no matter how many inherited duplicates exist. NMP's
/// required target-inode backend does exactly this for the database file it
/// locks alongside this sidecar; without it, only half of the pair would be
/// fork-safe.
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

/// NMP's one required, long-lived owner of the actual database inode.
///
/// Redb's built-in file backend deliberately continues when file locking is
/// unsupported. That is unsafe for NMP's destructive reset contract: hard-link
/// aliases have different pathname sidecars, so the target inode is the only
/// shared authority. NMP therefore acquires the target lock itself and passes
/// this backend to `create_with_backend`.
///
/// `Option::take` is the release state machine. Redb's required single
/// [`redb::StorageBackend::close`] call and a construction-failure `Drop` race
/// through the same door, so exactly one path explicitly unlocks and owns the
/// file. There is no adjacent lifecycle boolean.
#[derive(Debug)]
struct RequiredTargetLock {
    file: Arc<File>,
}

#[derive(Debug)]
pub(crate) struct RequiredLockedFileBackend {
    file: Arc<File>,
    target_lock: Mutex<Option<RequiredTargetLock>>,
    #[cfg(not(any(unix, windows)))]
    position: Mutex<()>,
}

impl RequiredLockedFileBackend {
    pub(crate) fn open(target: &Path) -> Result<Self, RedbStoreOpenError> {
        let file = Arc::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(target)
                .map_err(|source| RedbStoreOpenError::LockFailed {
                    path: target.to_path_buf(),
                    source,
                })?,
        );

        match try_lock_required_target(&file) {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(RedbStoreOpenError::StoreAlreadyOpen {
                    path: target.to_path_buf(),
                });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(RedbStoreOpenError::LockFailed {
                    path: target.to_path_buf(),
                    source,
                });
            }
        }

        // Ownership is represented immediately after the successful syscall,
        // before redb initialization or any other fallible work. Every later
        // return therefore runs the same exactly-once release door.
        Ok(Self {
            target_lock: Mutex::new(Some(RequiredTargetLock {
                file: Arc::clone(&file),
            })),
            file,
            #[cfg(not(any(unix, windows)))]
            position: Mutex::new(()),
        })
    }

    fn release(&self) -> io::Result<()> {
        let target_lock = self
            .target_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        match target_lock {
            Some(target_lock) => unlock_required_target(&target_lock.file),
            None => Ok(()),
        }
    }
}

impl Drop for RequiredLockedFileBackend {
    fn drop(&mut self) {
        let target_lock = self
            .target_lock
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(target_lock) = target_lock {
            let _ = unlock_required_target(&target_lock.file);
        }
    }
}

impl redb::StorageBackend for RequiredLockedFileBackend {
    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    #[cfg(unix)]
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;

        self.file.read_exact_at(out, offset)
    }

    #[cfg(windows)]
    fn read(&self, mut offset: u64, out: &mut [u8]) -> io::Result<()> {
        use std::os::windows::fs::FileExt;

        let mut data_offset = 0;
        while data_offset < out.len() {
            let read = self.file.seek_read(&mut out[data_offset..], offset)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "failed to fill persistent-store buffer",
                ));
            }
            offset += read as u64;
            data_offset += read;
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};

        let _position = self
            .position
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.file.as_ref();
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    #[cfg(unix)]
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;

        self.file.write_all_at(data, offset)
    }

    #[cfg(windows)]
    fn write(&self, mut offset: u64, data: &[u8]) -> io::Result<()> {
        use std::os::windows::fs::FileExt;

        let mut data_offset = 0;
        while data_offset < data.len() {
            let written = self.file.seek_write(&data[data_offset..], offset)?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write persistent-store buffer",
                ));
            }
            offset += written as u64;
            data_offset += written;
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};

        let _position = self
            .position
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.file.as_ref();
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)
    }

    fn close(&self) -> io::Result<()> {
        self.release()
    }
}

fn try_lock_required_target(file: &File) -> Result<(), std::fs::TryLockError> {
    file.try_lock()
}

fn unlock_required_target(file: &File) -> io::Result<()> {
    file.unlock()
}

/// Reset's ownership of the actual database inode.
///
/// `RedbStore` owns this same class of exclusive file lock through NMP's
/// required file backend. Reset joins that authoritative inode lock rather than
/// treating pathname ownership as proof that no hard-link alias is live.
struct LockedStoreTarget {
    file: File,
}

impl LockedStoreTarget {
    fn require_single_link(&self, target: &Path) -> Result<(), RedbStoreResetError> {
        let links =
            hard_link_count(&self.file).map_err(|source| RedbStoreResetError::LockFailed {
                path: target.to_path_buf(),
                source,
            })?;
        if links > 1 {
            return Err(RedbStoreResetError::LockFailed {
                path: target.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "persistent store has {links} hard links; reset cannot prove physical erasure"
                    ),
                ),
            });
        }
        Ok(())
    }
}

impl Drop for LockedStoreTarget {
    fn drop(&mut self) {
        // Match NMP's required backend teardown and the sidecar's #936 rule: an
        // explicit unlock releases the open-file-description lock even if a
        // concurrent fork transiently inherited a duplicate descriptor.
        let _ = self.file.unlock();
    }
}

#[cfg(unix)]
fn hard_link_count(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    Ok(file.metadata()?.nlink())
}

#[cfg(windows)]
fn hard_link_count(file: &File) -> io::Result<u64> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `file` keeps the handle valid for the call, and the API writes
    // the complete structure before returning nonzero.
    let succeeded = unsafe {
        GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonzero result initializes the complete structure.
        Ok(u64::from(
            unsafe { information.assume_init() }.nNumberOfLinks,
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn hard_link_count(_file: &File) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "persistent store hard-link count is unavailable on this platform",
    ))
}

fn acquire_target_for_reset(
    target: &Path,
) -> Result<Option<LockedStoreTarget>, RedbStoreResetError> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .truncate(false)
        .open(target)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(RedbStoreResetError::LockFailed {
                path: target.to_path_buf(),
                source,
            });
        }
    };

    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(RedbStoreResetError::StoreStillOpen {
                path: target.to_path_buf(),
            });
        }
        Err(std::fs::TryLockError::Error(source)) => {
            return Err(RedbStoreResetError::LockFailed {
                path: target.to_path_buf(),
                source,
            });
        }
    }

    let target_ownership = LockedStoreTarget { file };
    target_ownership.require_single_link(target)?;
    Ok(Some(target_ownership))
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

/// Every reachable failure from constructing a temporary or persistent Redb database.
#[derive(Debug)]
pub enum RedbStoreOpenError {
    /// The filesystem could not provide an isolated temporary directory for
    /// a temporary Redb database. No store or ownership token was created.
    TemporaryDirectoryFailed { source: io::Error },
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
    /// Nothing was migrated, adopted, aliased, drained, or reset. The caller
    /// must discard and recreate the store to continue. Relay-backed cache
    /// rows can be reacquired; publish queue state cannot, so accepted but
    /// unpublished writes and their receipts, route
    /// revisions, and attempt evidence are permanently lost.
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
            Self::TemporaryDirectoryFailed { source } => {
                write!(f, "could not create temporary Redb store directory: {source}")
            }
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
                     it was not migrated, adopted, drained, or reset; discard and recreate this \
                     store to continue; NMP can reacquire the relay-backed read cache, but the \
                     publish queue state (accepted but unpublished writes, receipts, route \
                     revisions, and attempt evidence) will be permanently lost",
                    path.display()
                ),
                None => write!(
                    f,
                    "persistent store {} predates the schema marker and is not the one supported \
                     epoch {expected}; it was not migrated, adopted, drained, or reset; discard and \
                     recreate this store to continue; NMP can reacquire the relay-backed read cache, \
                     but the publish queue state (accepted but unpublished writes, receipts, \
                     route revisions, and attempt evidence) will be permanently \
                     lost",
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
            Self::TemporaryDirectoryFailed { source }
            | Self::PathResolutionFailed { source, .. }
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
    /// The OS refused an ownership lock or the locked target could not prove
    /// single-link physical-erasure semantics.
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
                "could not establish persistent store ownership for {}: {source}",
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

/// Reset first owns the pathname lifecycle, then joins the same target-inode
/// lock held by NMP's live required file backend. Both guards remain held
/// through single-link validation and removal.
fn reset_store_with_hooks(
    path: &Path,
    before_remove: impl FnOnce(),
) -> Result<(), RedbStoreResetError> {
    let ownership = acquire_ownership(path)?;
    ownership.revalidate(path)?;
    let target = ownership.target().to_path_buf();
    let target_ownership = acquire_target_for_reset(&target)?;
    ownership.revalidate(path)?;
    before_remove();
    if let Some(target_ownership) = &target_ownership {
        target_ownership.require_single_link(&target)?;
    }
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RedbStoreResetError::RemoveFailed {
            path: target,
            source,
        }),
    }
}

