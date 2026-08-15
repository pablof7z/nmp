/// A durable-persistence failure at the acceptance boundary
/// (`docs/design/durable-write-signing-and-retry.md` §1): a returned `Err`
/// gives the caller an acceptance error, never an `Accepted` answer. For I/O,
/// durability remains unknown until reconstruction: correlation lookup may
/// reveal one committed pending row.
/// Realistic runtime failures (disk full, I/O error) at `accept_write`/
/// `accept_refused`/`promote_signed`/`compensate_write` must never panic
/// the embedding app. Neither may a *persisted row that does not decode*
/// (#790): a malformed, truncated, or schema-incompatible value is a fact
/// about the file, not a reason to abort the host, so every production
/// decoder of store-owned bytes/JSON reports it through its owning door as
/// [`PersistenceFault::Invariant`] instead of `.expect()`ing.
///
/// "Do not panic" was only half the contract (#895). The other half is
/// telling the embedder *what kind* of failure this was, so recovery is a
/// branch on a type rather than a `contains("Previous I/O")` on a string:
/// [`PersistenceError::fault`] carries the backend classification and
/// [`PersistenceError::durability`] carries the durability outcome. The
/// message is preserved verbatim for display.
#[derive(Debug)]
pub struct PersistenceError {
    fault: PersistenceFault,
    message: String,
}

impl PersistenceError {
    /// A failure the store raised about its own contents or arguments, not
    /// a backend I/O failure: a decode/encode failure, a schema invariant,
    /// an index disagreement, an exhausted counter. Nothing durable was
    /// written, and reopening the handle changes nothing.
    ///
    /// This is the constructor for every in-store `Err(...)` that is not a
    /// backend error funneled through `redb_store::schema::persist_err`.
    pub fn invariant(message: impl Into<String>) -> Self {
        Self::new(PersistenceFault::Invariant, message)
    }

    /// Build an explicitly classified failure. Store-adjacent engine
    /// validation may need to report a latch or an indeterminate I/O failure,
    /// not only an invariant.
    ///
    /// Claim the weakest fault that is true. [`PersistenceFault::Latched`]
    /// asserts the operation was never attempted; if that is not provable,
    /// [`PersistenceFault::Io`] is the honest answer.
    pub fn new(fault: PersistenceFault, message: impl Into<String>) -> Self {
        Self {
            fault,
            message: message.into(),
        }
    }

    /// How the backend failed. Branch on this — never on [`Self::message`].
    pub fn fault(&self) -> PersistenceFault {
        self.fault
    }

    /// What this failure says about the durability of the transaction that
    /// was in flight. Shorthand for `self.fault().durability()`.
    pub fn durability(&self) -> DurabilityOutcome {
        self.fault.durability()
    }

    /// The backend's own message, without the display framing.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The classification travels in the rendered line, not only in the
        // type (#895 §2). In the incident that motivated this, every logged
        // line was redb's `PreviousIo` string — the latch reports — and the
        // one transaction whose durability was actually unknown was
        // indistinguishable from them in the log. An embedder that logs
        // `{err}` now gets `fault=io` (the originating failure, durability
        // unknown) versus `fault=latched` (a report that the handle was
        // already dead and this write was never attempted) for free.
        //
        // `Invariant` renders exactly as it always did: it carries no
        // durability question, and annotating ~200 decode/schema messages
        // with `fault=invariant durability=absent` would bury the two lines
        // that matter under noise.
        if self.fault == PersistenceFault::Invariant {
            return write!(f, "durable-store persistence failure: {}", self.message);
        }
        write!(
            f,
            "durable-store persistence failure [fault={} durability={} reopen={}]: {}",
            self.fault.label(),
            self.fault.durability().label(),
            if self.fault.requires_reopen() {
                "required"
            } else {
                "not-required"
            },
            self.message
        )
    }
}

impl std::error::Error for PersistenceError {}

/// How the durable backend failed, as far as it can honestly be told apart
/// from inside this crate (#895).
///
/// Two questions are answered separately, because they have different
/// answers: [`Self::durability`] (did the in-flight transaction land?) and
/// [`Self::requires_reopen`] (is this handle still usable?).
///
/// `#[non_exhaustive]`: the set is bounded by what the *current* backend
/// can report, and both redb and any future backend can grow states.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersistenceFault {
    /// The backend refused *before* attempting the operation, because a
    /// previous I/O failure latched the handle (redb `StorageError::
    /// PreviousIo`) or the handle is closed (`DatabaseClosed`). Both come
    /// from `CheckedBackend::check_failure()`, which runs ahead of the
    /// backend op, so this write was never tried: determinate-absent.
    ///
    /// This state is sticky. Every subsequent write on the same handle
    /// reports it until the database is closed and reopened; the incident
    /// behind #895 logged thirty of these in four minutes. Retrying in
    /// place can only produce more of them — see [`Self::requires_reopen`].
    Latched,
    /// The *originating* I/O failure (redb `StorageError::Io`) — the one
    /// that sets the latch. **Not determinate.**
    ///
    /// It covers three underlying states that are indistinguishable from
    /// outside redb, because all three surface as this one variant with the
    /// same errno underneath on a full disk:
    ///
    /// 1. the failure happened before the durability flush, so the
    ///    transaction is absent;
    /// 2. the failure happened *inside* the flush
    ///    (`TransactionalMemory::commit_inner` → `storage.flush()`), which
    ///    issues the real per-page writes in arbitrary iteration order and
    ///    then `sync_data()`s — so whether the header page landed before
    ///    the disk filled is not decidable from here;
    /// 3. the flush returned `Ok` — the transaction *is* durable — and the
    ///    post-durability `resize` that follows it failed, returning `Err`
    ///    to the caller anyway.
    ///
    /// So the honest reading is the conservative union: **may be absent,
    /// may be durable, unknowable from here**. This is reachable on redb
    /// today; it is not a reserved concern for some other backend. Treating
    /// it as "the write failed, retry it" is how an embedder duplicates a
    /// write that is already on disk.
    ///
    /// The resolution is readback, not a decision procedure: durable writes
    /// are keyed (`AcceptWrite`'s intent, `promote_signed`'s `intent_id`),
    /// so after a reopen the caller can read that key back and observe what
    /// actually landed. Correctness in the meantime comes from idempotent
    /// replay, which survives either outcome without deciding which.
    Io,
    /// The backend reports its on-disk structure is corrupt (redb
    /// `StorageError::Corrupted`). Nothing this crate can retry, and the
    /// report says nothing about where the in-flight transaction got to.
    Corrupted,
    /// The value exceeds the backend's hard size limit (redb
    /// `StorageError::ValueTooLarge`). Deterministic and local: the handle
    /// is healthy, the same value will fail identically forever, and no
    /// durable state changed.
    ValueTooLarge,
    /// An internal backend lock was poisoned by a panic in another thread
    /// (redb `StorageError::LockPoisoned`). The handle cannot be trusted,
    /// and the panicking thread may have been mid-commit.
    LockPoisoned,
    /// A backend error variant this version of NMP does not understand.
    ///
    /// Backend error enums can grow without a semver-breaking release. An
    /// unrecognized variant may describe a commit or storage failure, so it
    /// cannot honestly inherit either [`Self::Io`] or [`Self::Invariant`].
    /// Its durability is conservatively unknown and the handle must be
    /// reopened before further use.
    UnknownBackend,
    /// Not a backend failure at all: this crate refusing its own inputs or
    /// its own decoded rows — a decode/encode failure, a schema invariant,
    /// an index disagreement, an exhausted counter. See
    /// [`PersistenceError::invariant`].
    ///
    /// **This, not [`Self::Corrupted`], is where a persisted row that fails
    /// to decode lands (#790.)** The two are not synonyms. `Corrupted` is
    /// redb's own report that the *file structure* is damaged — redb cannot
    /// vouch for what it did or did not write, so it is
    /// [`DurabilityOutcome::Unknown`]. A row that redb returned intact but
    /// whose bytes violate *this crate's* schema is the opposite situation:
    /// the backend is healthy and the failure is raised while decoding.
    /// Every fallible decode and validation precedes the enclosing write
    /// transaction's commit, so the requested mutation provably did not land
    /// and [`DurabilityOutcome::Absent`] is the true claim, not a convenient
    /// one. Reporting such a row as
    /// `Corrupted` would additionally assert [`Self::requires_reopen`],
    /// which is false: reopening the handle cannot change what the row says.
    Invariant,
}

impl PersistenceFault {
    /// What this fault says about the transaction that was in flight.
    ///
    /// [`DurabilityOutcome::Absent`] is a positive claim — "this did not
    /// happen" — so only faults that can prove it get it. A latch is proven
    /// (the backend refused before acting); an oversized value is proven
    /// (redb rejects it on the way in and the transaction is dropped
    /// uncommitted); an invariant is proven because all fallible validation
    /// precedes commit. Corruption, a poisoned lock, and an unrecognized
    /// backend error are not proven: each can describe a thread that was
    /// somewhere inside a commit, and none says where. They join `Io` in the
    /// conservative union rather than claim more than is known.
    ///
    /// `Absent` is only a fact about this store mutation. It is never, by
    /// itself, authority to repeat a signer request, wire send, user action,
    /// allocation, or any other operation. Replay requires a separate
    /// operation-specific recovery proof with retained input, stable
    /// identity, an atomic boundary, and a still-valid typed precondition.
    pub fn durability(self) -> DurabilityOutcome {
        match self {
            Self::Io | Self::Corrupted | Self::LockPoisoned | Self::UnknownBackend => {
                DurabilityOutcome::Unknown
            }
            Self::Latched | Self::ValueTooLarge | Self::Invariant => DurabilityOutcome::Absent,
        }
    }

    /// Whether the store handle must be closed and reopened before any
    /// further write can succeed.
    ///
    /// This is never a licence to retry the failed commit against the
    /// *same* handle. redb sets `needs_recovery` on the transactional
    /// memory for any commit error and `commit_inner` opens by asserting
    /// that flag is clear, so an in-place commit retry panics rather than
    /// erroring. Reopen is the only safe recovery, and `nmp-store` exposes
    /// no retry door of any kind.
    pub fn requires_reopen(self) -> bool {
        match self {
            Self::Latched
            | Self::Io
            | Self::Corrupted
            | Self::LockPoisoned
            | Self::UnknownBackend => true,
            Self::ValueTooLarge | Self::Invariant => false,
        }
    }

    /// Stable lowercase token for logs. Not a display string for humans —
    /// [`PersistenceError`]'s `Display` embeds this.
    pub fn label(self) -> &'static str {
        match self {
            Self::Latched => "latched",
            Self::Io => "io",
            Self::Corrupted => "corrupted",
            Self::ValueTooLarge => "value-too-large",
            Self::LockPoisoned => "lock-poisoned",
            Self::UnknownBackend => "unknown-backend",
            Self::Invariant => "invariant",
        }
    }
}

/// What a [`PersistenceError`] says about whether the in-flight transaction
/// reached durable storage (#895).
///
/// `#[non_exhaustive]`: a third state — determinate-durable-but-errored — is
/// real on redb today (a post-flush `resize` failure returns `Err` on an
/// already-durable transaction) but is not distinguishable from the other
/// `Io` cases from outside redb, so it is folded into [`Self::Unknown`]
/// rather than claimed. If it ever becomes observable it arrives as a new
/// variant, not as a silent change of meaning for these two.
///
/// This axis is reachable on redb today. It is not a reserved concern for a
/// future backend, and reading it that way is how an embedder concludes an
/// `Io` is determinate and builds the wrong retry.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DurabilityOutcome {
    /// Nothing was written. The backend either refused before attempting
    /// the operation or failed before the durability flush.
    Absent,
    /// Not determinate. The transaction may be absent, or it may already be
    /// durable. Do not retry it as though it were absent — replay it
    /// idempotently, or reopen and read the key back.
    Unknown,
}

impl DurabilityOutcome {
    /// Stable lowercase token for logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }
}
