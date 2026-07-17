//! Process-local ownership for destructive persistent-store reset (#489).
//!
//! This lives below every engine/facade construction path: a `RedbStore`
//! itself owns the registration, so moving it through `Engine::from_parts`
//! or a raw `EngineThread` cannot bypass the guard.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

static OPEN_STORES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

fn open_stores() -> &'static Mutex<HashMap<PathBuf, usize>> {
    OPEN_STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A prior panic cannot turn the corruption guard into a second panic. Every
/// governed operation recovers the protected map and re-establishes its own
/// invariant while holding the mutex.
fn lock_open_stores() -> MutexGuard<'static, HashMap<PathBuf, usize>> {
    open_stores()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

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

/// RAII ownership attached directly to every successfully opened
/// `RedbStore`. The store's database field is declared before this field, so
/// database teardown completes before the last count can be released.
pub(crate) struct OpenStoreRegistration {
    path: PathBuf,
}

impl Drop for OpenStoreRegistration {
    fn drop(&mut self) {
        let mut open = lock_open_stores();
        let remove = match open.get_mut(&self.path) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };
        if remove {
            open.remove(&self.path);
        }
    }
}

/// A value opened and registered under one uninterrupted registry lock.
/// Field order is load-bearing: on any later construction error, `value`
/// (the live database) drops before `registration` releases reset.
pub(crate) struct RegisteredOpen<T> {
    pub(crate) value: T,
    pub(crate) registration: OpenStoreRegistration,
}

/// Hold the shared registry mutex across pre-resolution, create/open,
/// post-open canonicalization, and registration. `open` receives the exact
/// caller path so a dangling final symlink is followed by the OS; the
/// post-open identity names the actual created target.
pub(crate) fn open_and_register<T, E>(
    path: &Path,
    open: impl FnOnce(&Path) -> Result<T, E>,
) -> Result<RegisteredOpen<T>, E>
where
    E: From<io::Error>,
{
    let mut stores = lock_open_stores();
    let pre_open_identity = resolve_store_path(path).map_err(E::from)?;
    let value = open(path)?;
    let path = resolve_store_path(path).map_err(E::from)?;
    if path != pre_open_identity {
        return Err(E::from(io::Error::other(format!(
            "persistent store target changed during open: {} -> {}",
            pre_open_identity.display(),
            path.display()
        ))));
    }
    let count = stores.entry(path.clone()).or_default();
    *count = count.checked_add(1).ok_or_else(|| {
        E::from(io::Error::other(
            "persistent store registration count exhausted",
        ))
    })?;
    let registration = OpenStoreRegistration { path };
    drop(stores);
    Ok(RegisteredOpen {
        value,
        registration,
    })
}

/// Typed store-layer result for destructive reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedbStoreResetError {
    StoreStillOpen { path: PathBuf },
    ResetFailed { reason: String },
}

impl std::fmt::Display for RedbStoreResetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreStillOpen { path } => {
                write!(f, "persistent store is still open: {}", path.display())
            }
            Self::ResetFailed { reason } => write!(f, "could not reset store: {reason}"),
        }
    }
}

impl std::error::Error for RedbStoreResetError {}

pub(crate) fn reset_store(path: &Path) -> Result<(), RedbStoreResetError> {
    reset_store_with_hooks(path, || {}, || {})
}

fn reset_store_with_hooks(
    path: &Path,
    before_lock: impl FnOnce(),
    before_remove: impl FnOnce(),
) -> Result<(), RedbStoreResetError> {
    before_lock();
    let stores = lock_open_stores();
    let path = resolve_store_path(path).map_err(|error| RedbStoreResetError::ResetFailed {
        reason: error.to_string(),
    })?;
    if stores.contains_key(&path) {
        return Err(RedbStoreResetError::StoreStillOpen { path });
    }
    before_remove();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RedbStoreResetError::ResetFailed {
            reason: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    fn create_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    #[test]
    fn open_and_reset_interleavings_are_serialized_in_both_directions() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("interleaving.redb");

        // A reset that begins while create/open is paused cannot observe the
        // file before registration. It resumes only afterward and refuses.
        let (created_tx, created_rx) = mpsc::sync_channel(0);
        let (finish_open_tx, finish_open_rx) = mpsc::sync_channel(0);
        let open_path = path.clone();
        let opener = thread::spawn(move || {
            open_and_register::<(), io::Error>(&open_path, |path| {
                create_bytes(path, b"opened-before-registration")?;
                created_tx.send(()).unwrap();
                finish_open_rx.recv().unwrap();
                Ok(())
            })
            .unwrap()
        });
        created_rx.recv().unwrap();
        let (reset_started_tx, reset_started_rx) = mpsc::sync_channel(0);
        let reset_path = path.clone();
        let resetter = thread::spawn(move || {
            reset_store_with_hooks(&reset_path, || reset_started_tx.send(()).unwrap(), || {})
        });
        reset_started_rx.recv().unwrap();
        finish_open_tx.send(()).unwrap();
        let owner = opener.join().unwrap();
        assert_eq!(
            resetter.join().unwrap(),
            Err(RedbStoreResetError::StoreStillOpen {
                path: path.canonicalize().unwrap(),
            })
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"opened-before-registration");
        drop(owner);

        // Conversely, reset holds the same mutex from its closed check
        // through removal. A concurrent open starts only after deletion and
        // therefore creates fresh bytes rather than losing a live file.
        create_bytes(&path, b"old-store").unwrap();
        let (checked_tx, checked_rx) = mpsc::sync_channel(0);
        let (finish_reset_tx, finish_reset_rx) = mpsc::sync_channel(0);
        let reset_path = path.clone();
        let resetter = thread::spawn(move || {
            reset_store_with_hooks(
                &reset_path,
                || {},
                || {
                    checked_tx.send(()).unwrap();
                    finish_reset_rx.recv().unwrap();
                },
            )
        });
        checked_rx.recv().unwrap();
        let (open_started_tx, open_started_rx) = mpsc::sync_channel(0);
        let open_path = path.clone();
        let opener = thread::spawn(move || {
            open_started_tx.send(()).unwrap();
            open_and_register::<(), io::Error>(&open_path, |path| {
                assert!(!path.exists(), "reset must remove before open proceeds");
                create_bytes(path, b"fresh-store")?;
                Ok(())
            })
            .unwrap()
        });
        open_started_rx.recv().unwrap();
        finish_reset_tx.send(()).unwrap();
        resetter.join().unwrap().unwrap();
        let owner = opener.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh-store");
        drop(owner);
        reset_store(&path).unwrap();
    }

    #[test]
    fn two_live_owners_require_the_last_registration_to_close() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("two-owners.redb");
        let first = open_and_register::<(), io::Error>(&path, |path| create_bytes(path, b"stable"))
            .unwrap();
        let second = open_and_register::<(), io::Error>(&path, |_| Ok(())).unwrap();

        assert!(matches!(
            reset_store(&path),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        drop(first);
        assert!(matches!(
            reset_store(&path),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        drop(second);
        reset_store(&path).expect("last owner must release reset");
    }

    #[cfg(unix)]
    #[test]
    fn existing_and_dangling_final_symlinks_resolve_to_the_store_target() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target.redb");
        let existing_alias = fixture.path().join("existing-alias.redb");
        drop(crate::RedbStore::open(&target).unwrap());
        symlink(&target, &existing_alias).unwrap();
        let owner = crate::RedbStore::open(&existing_alias).unwrap();
        assert_eq!(
            reset_store(&existing_alias),
            Err(RedbStoreResetError::StoreStillOpen {
                path: target.canonicalize().unwrap(),
            })
        );
        assert!(matches!(
            reset_store(&target),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        drop(owner);
        reset_store(&existing_alias).unwrap();

        let dangling_target = fixture.path().join("created-through-target.redb");
        let dangling_alias = fixture.path().join("dangling-alias.redb");
        symlink("created-through-target.redb", &dangling_alias).unwrap();
        let owner = crate::RedbStore::open(&dangling_alias).unwrap();
        let canonical_target = dangling_target.canonicalize().unwrap();
        assert_eq!(
            reset_store(&dangling_alias),
            Err(RedbStoreResetError::StoreStillOpen {
                path: canonical_target.clone(),
            })
        );
        assert_eq!(
            reset_store(&dangling_target),
            Err(RedbStoreResetError::StoreStillOpen {
                path: canonical_target,
            })
        );
        assert!(dangling_target.exists());
        drop(owner);
        reset_store(&dangling_alias).unwrap();
        assert!(!dangling_target.exists());
        assert!(
            std::fs::symlink_metadata(&dangling_alias).is_ok(),
            "reset removes the resolved store target, not the alias inode"
        );
    }

    #[test]
    fn poisoned_registry_recovers_for_later_governed_operations() {
        let poisoned = thread::spawn(|| {
            let _guard = open_stores().lock().unwrap();
            panic!("poison registry for deterministic recovery proof");
        });
        assert!(poisoned.join().is_err());

        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("after-poison.redb");
        let owner =
            open_and_register::<(), io::Error>(&path, |path| create_bytes(path, b"after poison"))
                .expect("open must recover poisoned registry");
        assert!(matches!(
            reset_store(&path),
            Err(RedbStoreResetError::StoreStillOpen { .. })
        ));
        drop(owner);
        reset_store(&path).expect("reset must recover poisoned registry");
    }

    #[test]
    fn post_create_schema_failure_releases_registration_after_database_drop() {
        const FOREIGN_TABLE: redb::TableDefinition<&str, &str> =
            redb::TableDefinition::new("foreign_table");

        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("unsupported-schema.redb");
        let db = redb::Database::create(&path).unwrap();
        let write = db.begin_write().unwrap();
        write.open_table(FOREIGN_TABLE).unwrap();
        write.commit().unwrap();
        drop(db);

        assert!(matches!(
            crate::RedbStore::open(&path),
            Err(redb::Error::UpgradeRequired(7))
        ));
        reset_store(&path)
            .expect("post-create schema refusal must release database then registration");
        assert!(!path.exists());
    }
}
