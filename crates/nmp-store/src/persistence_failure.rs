/// A durable-persistence failure: the store could not read or write what was
/// asked of it.
///
/// There is nothing to branch on. A store operation that fails, fails — the
/// caller propagates with `?` and the engine carries on. NMP models no
/// degraded mode, no latched fault, no reopen, and no classification of local
/// disk failure: local persistence is best-effort, and losing it loses
/// progress, never an accepted write.
///
/// What that costs is bounded by acceptance atomicity. `accept_write` commits
/// the intent, the receipt, the frozen body and the canonical pending row in
/// one transaction, so a `publish()` that returned `Ok` is already durable;
/// everything a later failure can destroy is progress, which boot recovery
/// reconstructs from those durable rows.
///
/// Realistic runtime failures (disk full, I/O error) must never panic the
/// embedding app, and neither may a persisted row that does not decode
/// (#790): a malformed, truncated, or schema-incompatible value is a fact
/// about the file, not a reason to abort the host, so every production
/// decoder of store-owned bytes reports it through its owning door instead of
/// `.expect()`ing.
#[derive(Debug)]
pub struct PersistenceError {
    message: String,
}

impl PersistenceError {
    /// Build a failure from the message the backend or decoder produced.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The backend's own message, without the display framing.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "durable-store persistence failure: {}", self.message)
    }
}

impl std::error::Error for PersistenceError {}
