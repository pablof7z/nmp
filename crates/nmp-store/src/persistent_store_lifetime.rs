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
                #[cfg(test)]
                call_path_resolution_test_hook(&candidate);
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

#[cfg(test)]
type PathResolutionTestHook = Box<dyn FnMut(&Path)>;

#[cfg(test)]
std::thread_local! {
    static PATH_RESOLUTION_TEST_HOOK:
        std::cell::RefCell<Option<PathResolutionTestHook>> = const {
            std::cell::RefCell::new(None)
        };
}

#[cfg(test)]
struct ClearPathResolutionTestHook;

#[cfg(test)]
impl Drop for ClearPathResolutionTestHook {
    fn drop(&mut self) {
        PATH_RESOLUTION_TEST_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

#[cfg(test)]
fn with_path_resolution_test_hook<T>(
    hook: impl FnMut(&Path) + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    PATH_RESOLUTION_TEST_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(
            slot.is_none(),
            "one path-resolution operation cannot install two test hooks"
        );
        *slot = Some(Box::new(hook));
    });
    let _clear = ClearPathResolutionTestHook;
    operation()
}

#[cfg(test)]
fn call_path_resolution_test_hook(path: &Path) {
    PATH_RESOLUTION_TEST_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(path);
        }
    });
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
    /// Nothing was migrated, adopted, aliased, drained, or reset. The caller
    /// must discard and recreate the store to continue. Relay-backed cache
    /// rows can be reacquired; the durable outbox cannot, so accepted but
    /// unpublished writes and their receipts, correlation tokens, route
    /// revisions, and attempt evidence are permanently lost
    /// (`docs/internals/conventions/schema-epoch-discard.md`).
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
                     it was not migrated, adopted, drained, or reset; discard and recreate this \
                     store to continue; NMP can reacquire the relay-backed read cache, but the \
                     durable write outbox (accepted but unpublished writes, receipts, correlation \
                     tokens, route revisions, and attempt evidence) will be permanently lost",
                    path.display()
                ),
                None => write!(
                    f,
                    "persistent store {} predates the schema marker and is not the one supported \
                     epoch {expected}; it was not migrated, adopted, drained, or reset; discard and \
                     recreate this store to continue; NMP can reacquire the relay-backed read cache, \
                     but the durable write outbox (accepted but unpublished writes, receipts, \
                     correlation tokens, route revisions, and attempt evidence) will be permanently \
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Barrier, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    /// #1001 falsifier: resolution of a path whose parent exists must never
    /// report `ENOENT`, no matter who is creating or removing the final
    /// component at the same time.
    ///
    /// Both answers are legitimate -- the canonical path of the file that now
    /// exists, or the parent-derived path of the file that does not -- and
    /// resolution has to pick one of them rather than surface a `NotFound` it
    /// observed mid-flight. A racing `RedbStore::open` is owed a typed
    /// `StoreAlreadyOpen`, and it got `ENOENT` instead whenever the winner
    /// created the database between resolution's two syscalls.
    ///
    /// The loop only ever fails on a real observation of that window, so a run
    /// that never hits it passes rather than flaking red.
    #[test]
    fn resolution_never_reports_enoent_while_the_target_is_being_created() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("churn.redb");
        let stop = Arc::new(AtomicBool::new(false));

        let churn_path = path.clone();
        let churn_stop = Arc::clone(&stop);
        let churn = thread::spawn(move || {
            while !churn_stop.load(Ordering::Relaxed) {
                let _ = File::create(&churn_path);
                let _ = std::fs::remove_file(&churn_path);
            }
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut observed = None;
        while Instant::now() < deadline {
            if let Err(error) = resolve_store_path(&path) {
                observed = Some(error);
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        churn.join().unwrap();

        assert!(
            observed.is_none(),
            "resolving a path whose parent exists must not fail while the final \
             component is being created: {observed:?}"
        );
    }

    const PHASE_TIMEOUT: Duration = Duration::from_secs(5);
    const PHASE_LEDGER_CAPACITY: usize = 8;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ResolutionRacePhase {
        ContenderObservedMissing,
        TargetCreated,
        WinnerOpened,
        ContenderResumed,
        TypedRefusal,
        Resolved,
    }

    #[derive(Debug)]
    struct PhaseLedger {
        entries: [Option<ResolutionRacePhase>; PHASE_LEDGER_CAPACITY],
        len: usize,
    }

    impl PhaseLedger {
        fn new() -> Self {
            Self {
                entries: [None; PHASE_LEDGER_CAPACITY],
                len: 0,
            }
        }

        fn record(&mut self, phase: ResolutionRacePhase) {
            assert!(
                self.len < self.entries.len(),
                "fixed phase ledger exhausted before recording {phase:?}: {self:?}"
            );
            self.entries[self.len] = Some(phase);
            self.len += 1;
        }

        fn recorded(&self) -> &[Option<ResolutionRacePhase>] {
            &self.entries[..self.len]
        }
    }

    fn record_phase(ledger: &Arc<Mutex<PhaseLedger>>, phase: ResolutionRacePhase) {
        ledger.lock().unwrap().record(phase);
    }

    fn phase_snapshot(
        ledger: &Arc<Mutex<PhaseLedger>>,
    ) -> [Option<ResolutionRacePhase>; PHASE_LEDGER_CAPACITY] {
        ledger.lock().unwrap().entries
    }

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
        // Never signal 0 or a negative pid: those mean "my process group" and
        // "some other group", either of which would take down the test run.
        assert!(child > 0, "not a forked descriptor holder: {child}");
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
    fn missing_target_created_between_resolution_observations_retries_coherently() {
        for attempt in 0..50 {
            let fixture = tempfile::tempdir().unwrap();
            let path = fixture.path().join(format!("appeared-{attempt}.redb"));
            let phases = Arc::new(Mutex::new(PhaseLedger::new()));
            let (missing_tx, missing_rx) = mpsc::sync_channel(1);
            let (resume_tx, resume_rx) = mpsc::sync_channel(1);

            let contender_path = path.clone();
            let contender_phases = Arc::clone(&phases);
            let contender = thread::spawn(move || {
                let mut missing_tx = Some(missing_tx);
                let mut resume_rx = Some(resume_rx);
                with_path_resolution_test_hook(
                    move |_| {
                        record_phase(
                            &contender_phases,
                            ResolutionRacePhase::ContenderObservedMissing,
                        );
                        missing_tx
                            .take()
                            .expect("the controlled path must be observed missing once")
                            .send(())
                            .unwrap();
                        resume_rx
                            .take()
                            .expect("the controlled path must resume once")
                            .recv_timeout(PHASE_TIMEOUT)
                            .unwrap_or_else(|error| {
                                panic!(
                                    "resolver was not released within {PHASE_TIMEOUT:?}: {error}; \
                                     phases={:?}",
                                    phase_snapshot(&contender_phases)
                                )
                            });
                        record_phase(&contender_phases, ResolutionRacePhase::ContenderResumed);
                    },
                    || resolve_store_path(&contender_path),
                )
            });

            missing_rx
                .recv_timeout(PHASE_TIMEOUT)
                .unwrap_or_else(|error| {
                    panic!(
                        "resolver did not observe the missing target within {PHASE_TIMEOUT:?}: \
                         {error}; attempt={attempt}; phases={:?}",
                        phase_snapshot(&phases)
                    )
                });
            std::fs::write(&path, b"appeared between observations").unwrap();
            record_phase(&phases, ResolutionRacePhase::TargetCreated);
            resume_tx.send(()).unwrap();

            let resolved = contender.join().unwrap().unwrap_or_else(|error| {
                panic!(
                    "a target that appeared between observations returned a stale error: \
                     {error}; attempt={attempt}; phases={:?}",
                    phase_snapshot(&phases)
                )
            });
            record_phase(&phases, ResolutionRacePhase::Resolved);
            assert_eq!(resolved, std::fs::canonicalize(&path).unwrap());
            assert_eq!(
                phases.lock().unwrap().recorded(),
                [
                    Some(ResolutionRacePhase::ContenderObservedMissing),
                    Some(ResolutionRacePhase::TargetCreated),
                    Some(ResolutionRacePhase::ContenderResumed),
                    Some(ResolutionRacePhase::Resolved),
                ],
                "attempt={attempt}"
            );
        }
    }

    #[test]
    fn target_created_between_resolution_observations_has_exactly_one_owner() {
        const OPENER_COUNT: usize = 8;

        for attempt in 0..50 {
            let fixture = tempfile::tempdir().unwrap();
            let path = fixture.path().join(format!("raced-{attempt}.redb"));
            let phases = Arc::new(Mutex::new(PhaseLedger::new()));
            let (missing_tx, missing_rx) = mpsc::sync_channel(1);
            let (resume_tx, resume_rx) = mpsc::sync_channel(1);

            let contender_path = path.clone();
            let contender_phases = Arc::clone(&phases);
            let contender = thread::spawn(move || {
                let mut missing_tx = Some(missing_tx);
                let mut resume_rx = Some(resume_rx);
                with_path_resolution_test_hook(
                    move |_| {
                        record_phase(
                            &contender_phases,
                            ResolutionRacePhase::ContenderObservedMissing,
                        );
                        missing_tx
                            .take()
                            .expect("the controlled opener must observe missing once")
                            .send(())
                            .unwrap();
                        resume_rx
                            .take()
                            .expect("the controlled opener must resume once")
                            .recv_timeout(PHASE_TIMEOUT)
                            .unwrap_or_else(|error| {
                                panic!(
                                    "contender was not released within {PHASE_TIMEOUT:?}: \
                                     {error}; phases={:?}",
                                    phase_snapshot(&contender_phases)
                                )
                            });
                        record_phase(&contender_phases, ResolutionRacePhase::ContenderResumed);
                    },
                    || crate::RedbStore::open(&contender_path),
                )
            });

            missing_rx
                .recv_timeout(PHASE_TIMEOUT)
                .unwrap_or_else(|error| {
                    panic!(
                        "contender did not observe the missing target within {PHASE_TIMEOUT:?}: \
                         {error}; attempt={attempt}; phases={:?}",
                        phase_snapshot(&phases)
                    )
                });
            let winner = crate::RedbStore::open(&path).unwrap();
            record_phase(&phases, ResolutionRacePhase::WinnerOpened);

            let other_losers = (0..OPENER_COUNT - 2)
                .map(|_| {
                    let path = path.clone();
                    thread::spawn(move || crate::RedbStore::open(&path))
                })
                .collect::<Vec<_>>();
            let mut typed_refusals = 0;
            for loser in other_losers {
                assert!(matches!(
                    loser.join().unwrap(),
                    Err(RedbStoreOpenError::StoreAlreadyOpen { .. })
                ));
                typed_refusals += 1;
            }

            resume_tx.send(()).unwrap();
            match contender.join().unwrap() {
                Err(RedbStoreOpenError::StoreAlreadyOpen { .. }) => {
                    record_phase(&phases, ResolutionRacePhase::TypedRefusal);
                    typed_refusals += 1;
                }
                Err(error) => {
                    panic!(
                        "the resumed contender returned an invalid third outcome: \
                         {error}; attempt={attempt}; phases={:?}",
                        phase_snapshot(&phases)
                    );
                }
                Ok(second_owner) => {
                    drop(second_owner);
                    panic!(
                        "the resumed contender became a second owner; attempt={attempt}; \
                         phases={:?}",
                        phase_snapshot(&phases)
                    );
                }
            }

            assert_eq!(
                phases.lock().unwrap().recorded(),
                [
                    Some(ResolutionRacePhase::ContenderObservedMissing),
                    Some(ResolutionRacePhase::WinnerOpened),
                    Some(ResolutionRacePhase::ContenderResumed),
                    Some(ResolutionRacePhase::TypedRefusal),
                ],
                "attempt={attempt}"
            );
            assert_eq!(
                typed_refusals,
                OPENER_COUNT - 1,
                "one owner requires every other opener to receive the typed refusal"
            );
            drop(winner);
        }
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
                        // Deliberately still exactly two outcomes (#1001): a
                        // loser that sees anything else is reporting a real
                        // defect, so this arm names it rather than widening
                        // to accommodate it.
                        Err(error) => {
                            panic!("a losing opener must get StoreAlreadyOpen, got: {error}")
                        }
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
            // #1001: `join().unwrap()` reported a racing thread's panic as
            // `Any { .. }`, so the run that mattered said nothing about what
            // the opener actually saw. Carry the child's message up instead.
            .map(|thread| {
                thread.join().unwrap_or_else(|payload| {
                    let detail = payload
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| payload.downcast_ref::<&str>().copied())
                        .unwrap_or("<non-string panic payload>");
                    panic!("a racing opener panicked: {detail}")
                })
            })
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

    /// #723 falsifier: pathname ownership alone cannot protect a live
    /// database opened through another hard link. Reset must join the
    /// database inode's lock before removing either name.
    #[cfg(unix)]
    #[test]
    fn hard_link_alias_reset_refuses_live_subprocess_owner_without_mutation() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("hard-link-owner.redb");
        let alias = fixture.path().join("hard-link-alias.redb");

        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("persistent_store_lifetime::tests::subprocess_owner_helper")
            .arg("--nocapture")
            .env("NMP_STORE_OWNER_HELPER_PATH", &target)
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

        std::fs::hard_link(&target, &alias).unwrap();
        let before = std::fs::read(&target).unwrap();
        let before_digest = blake3::hash(&before);

        assert!(matches!(
            reset_store(&alias),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        assert_eq!(std::fs::read(&target).unwrap(), before);
        assert_eq!(std::fs::read(&alias).unwrap(), before);
        assert_eq!(
            blake3::hash(&std::fs::read(&target).unwrap()),
            before_digest
        );
        assert_eq!(blake3::hash(&std::fs::read(&alias).unwrap()), before_digest);

        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
    }
}
