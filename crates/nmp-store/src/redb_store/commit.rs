//! The one transaction exit for direct `RedbStore` mutations.
//!
//! Callers must finish every fallible read, decode, validation, and return
//! value construction before entering this function. Once redb reports a
//! successful commit, returning the already-prepared value is an infallible
//! move: no store callback can turn a durable mutation into an `Err` after
//! the fact.

use super::schema::persist_err;
use super::PersistenceError;

/// Commit one fully prepared transaction and return its already-built result.
pub(super) fn commit_prepared<T>(
    write_txn: redb::WriteTransaction,
    prepared: T,
) -> Result<T, PersistenceError> {
    write_txn.commit().map_err(persist_err)?;
    Ok(prepared)
}
