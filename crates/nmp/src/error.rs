//! [`EngineError`] -- the semantic error subset [`Engine`](crate::Engine)'s
//! verbs can fail with (canonical-facade-52-plan.md §1).
//!
//! This set is deliberately SMALL: construction-time failures, identity
//! parsing, and the closed-lifecycle state. The one thing it explicitly
//! does NOT contain is a "bad signed event" variant -- that guarantee lives
//! at `nmp_engine::core::EngineCore::on_publish`'s acceptance boundary now
//! (Unit A0, #56, per the Fable checkpoint's Q2 ruling), so a tampered
//! `WritePayload::Signed` surfaces on the [`WriteFact`](crate::WriteFact)
//! receipt stream `publish` returns, not as a sync `Err` here. Duplicating a
//! second verify at this layer would recreate the exact entry-point-
//! dependent hole #52 exists to kill.

/// Every way [`Engine::new`](crate::Engine::new) or a subsequent verb can
/// fail closed. Errors are values across this boundary -- a call made after
/// [`Engine::shutdown`](crate::Engine::shutdown) is [`Self::EngineClosed`],
/// never a panic and never a silently disconnected channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// One of [`EngineConfig`](crate::EngineConfig)'s `indexer_relays`/
    /// `app_relays`/`fallback_relays` entries did not parse as a valid relay
    /// URL.
    InvalidRelayUrl { url: String },
    /// [`EngineConfig::store_path`](crate::EngineConfig::store_path) pointed
    /// at a file the on-disk store could not open: damaged bytes, a refused
    /// lock, an unresolvable path, an I/O failure.
    ///
    /// This is a POSITIVE claim, not a catch-all (#920): whatever refused
    /// this open, **discarding the store is not the recovery**. The one
    /// refusal a fresh store does fix is
    /// [`Self::StoreUnsupportedSchema`], and it is never rendered here.
    /// Treating a damaged current-epoch file as a discardable old one
    /// destroys accepted-but-unpublished writes that the network cannot
    /// re-supply, so the two stay separate variants in both directions.
    StoreOpenFailed { reason: String },
    /// [`EngineConfig::store_path`](crate::EngineConfig::store_path) names a
    /// persistent store already owned by this or another process. No second
    /// database owner and no partial engine were created (#489).
    StoreAlreadyOpen { path: String },
    /// [`EngineConfig::store_path`](crate::EngineConfig::store_path)'s
    /// durable bytes are not the one schema epoch this build supports
    /// (#867/#920). Nothing was migrated, adopted, drained, or reset, and no
    /// engine was constructed.
    ///
    /// NMP states the fact; the app owns the response. The only response
    /// that lets this build run is to close every owner, discard the store,
    /// and create a fresh one
    /// (`docs/internals/conventions/schema-epoch-discard.md`) — but that
    /// discard is permanent and it is the app's call, so
    /// [`Engine::reset_persistent_store`](crate::Engine::reset_persistent_store)
    /// stays a separate deliberate act. The relay-backed read cache can be
    /// reacquired; the publish queue cannot, so accepted but unpublished
    /// writes and their receipts, correlation tokens, route revisions, and
    /// attempt evidence are lost with it.
    ///
    /// `expected` is this build's epoch. `found` is the marker actually
    /// present, and is `None` when the store carries no marker this build
    /// can read — which includes a store whose marker lives at an address a
    /// superseded epoch owned. `None` therefore means "not this epoch", not
    /// "no data".
    StoreUnsupportedSchema {
        path: String,
        expected: u64,
        found: Option<u64>,
    },
    /// [`Engine::reset_persistent_store`](crate::Engine::reset_persistent_store)
    /// could not remove the requested unowned persistent store.
    StoreResetFailed { reason: String },
    /// Destructive reset was refused because an engine in this or any other
    /// process still owns the same canonical persistent-store path.
    StoreStillOpen { path: String },
    /// The engine could not be constructed: the OS refused one engine-owned
    /// transport/runtime thread, or the configured relay budget could not be
    /// represented safely. No partial engine escapes construction. This is an
    /// engine-start (`Engine::new`) failure only — it never surfaces from an
    /// ordinary operation (#704).
    EngineStartFailed { component: String, reason: String },
    /// Construction named a store that retains a replaceable operation whose
    /// compiled program/format is absent from the supplied capability set.
    /// No engine was started and the store was not mutated.
    MissingReplaceableCapability { program: [u8; 16], format: [u8; 16] },
    /// Construction named two capabilities for the same compiled
    /// (program, format) pair. Capability identity is (program, format); a
    /// second implementation for the same pair is not a replacement (there is
    /// no replacement lifecycle after #1624), it is a construction error. No
    /// engine was started and the store was not mutated.
    DuplicateReplaceableCapability { program: [u8; 16], format: [u8; 16] },
    /// An ordinary or windowed [`Engine::observe`](crate::Engine::observe)
    /// could not open its initial canonical projection because the store
    /// degraded during setup. The failed open leaves no observation owner.
    /// Relay connection or relay-worker failure is ordinary acquisition
    /// evidence in the observation stream and never constructs this error.
    /// It is also never a worker-pool-busy, task-admission, permit, or
    /// queue-full outcome. The engine-closed case is [`Self::EngineClosed`].
    ObservationUnavailable { reason: String },
    /// The shared account-signer/AUTH-policy capability registry reached its
    /// configured [`EngineConfig::max_auth_capabilities`](crate::EngineConfig::max_auth_capabilities)
    /// bound (#8). Nothing was registered behind this refusal.
    AuthCapabilityRegistryFull { limit: usize },
    /// The monotonic capability-instance namespace that distinguishes stale
    /// registrations from their replacements has been exhausted (#8). It
    /// never wraps or reuses an identity; registration fails closed instead.
    AuthCapabilityInstanceExhausted,
    /// A windowed [`Engine::observe`](crate::Engine::observe) declared an
    /// `initial` window size greater than its `max` ceiling (#485). Caught
    /// before the engine is touched; zero sizes are unrepresentable via
    /// `NonZeroUsize`.
    WindowInitialExceedsMax { initial: usize, max: usize },
    /// A windowed [`Engine::observe`](crate::Engine::observe) was given a
    /// selection that already carries a NIP-01 `limit` (#485). A window and a
    /// `limit` would be two competing owners of row membership.
    WindowSelectionHasLimit,
    /// A windowed [`Engine::observe`](crate::Engine::observe) was given a
    /// live query that already declares an aggregate result limit (#1108).
    /// The window and the aggregate bound would be two competing owners of
    /// the same merged row-membership count.
    WindowAggregateResultLimit,
    /// [`Engine::shutdown`](crate::Engine::shutdown) has already run --
    /// every other verb fails closed with this variant instead of racing
    /// the engine thread's own teardown (see [`crate::Engine`]'s doc for
    /// the serialized lifecycle gate that makes this exhaustive).
    EngineClosed,
    /// `publish()` refused the call outright: either NMP could not write
    /// anything down, or the instruction could not resolve. Nothing durable
    /// exists and there is no queue entry to inspect — see
    /// [`crate::PublishError`] for the exhaustive typed reason, which this
    /// boundary carries as its rendered sentence.
    PublishRefused { reason: String },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRelayUrl { url } => write!(f, "invalid relay url: {url:?}"),
            Self::StoreOpenFailed { reason } => write!(f, "could not open store: {reason}"),
            Self::StoreAlreadyOpen { path } => {
                write!(f, "persistent store is already open: {path}")
            }
            Self::StoreUnsupportedSchema {
                path,
                expected,
                found,
            } => {
                match found {
                    Some(found) => write!(
                        f,
                        "persistent store {path} is schema epoch {found}, not the one supported \
                         epoch {expected}"
                    ),
                    None => write!(
                        f,
                        "persistent store {path} carries no readable schema marker and is not the \
                         one supported epoch {expected}"
                    ),
                }?;
                // #1017's operator contract: whoever reads the reachable
                // refusal learns the full cost before choosing the discard.
                // The facade is where an app's operator reads it, so it
                // states the same two facts the store-level refusal does.
                write!(
                    f,
                    "; it was not migrated, adopted, drained, or reset; discard and recreate this \
                     store to continue; NMP can reacquire the relay-backed read cache, but the \
                     publish queue state (accepted but unpublished writes, receipts, correlation \
                     tokens, route revisions, and attempt evidence) will be permanently lost"
                )
            }
            Self::StoreResetFailed { reason } => write!(f, "could not reset store: {reason}"),
            Self::StoreStillOpen { path } => {
                write!(f, "persistent store is still open: {path}")
            }
            Self::EngineStartFailed { component, reason } => {
                write!(f, "engine could not start ({component}): {reason}")
            }
            Self::MissingReplaceableCapability { program, format } => {
                write!(
                    f,
                    "store retains replaceable operations for missing compiled capability program {:02x?} format {:02x?}",
                    program,
                    format
                )
            }
            Self::DuplicateReplaceableCapability { program, format } => {
                write!(
                    f,
                    "duplicate compiled capability for program {:02x?} format {:02x?}: identity is (program, format), not a replacement",
                    program,
                    format
                )
            }
            Self::ObservationUnavailable { reason } => {
                write!(f, "observation could not be established: {reason}")
            }
            Self::AuthCapabilityRegistryFull { limit } => {
                write!(f, "AUTH capability registry is full at {limit} entries")
            }
            Self::AuthCapabilityInstanceExhausted => {
                write!(f, "AUTH capability instance space exhausted")
            }
            Self::WindowInitialExceedsMax { initial, max } => {
                write!(f, "window initial size {initial} exceeds its max {max}")
            }
            Self::WindowSelectionHasLimit => {
                write!(f, "a windowed selection must not also declare a limit")
            }
            Self::WindowAggregateResultLimit => write!(
                f,
                "a windowed observation must not also declare an aggregate result limit"
            ),
            Self::PublishRefused { reason } => write!(f, "{reason}"),
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for EngineError {}

impl EngineError {
    /// Map an engine-thread failure raised during engine CONSTRUCTION
    /// (`Engine::new`) to its engine-start error. A genuine OS thread refusal
    /// or an unrepresentable relay budget both mean no engine was built (#704).
    pub(crate) fn from_start_error(error: nmp_runtime::EngineThreadError) -> Self {
        match error {
            nmp_runtime::EngineThreadError::ThreadUnavailable { component, reason } => {
                Self::EngineStartFailed { component, reason }
            }
            nmp_runtime::EngineThreadError::RelayBudgetOverflow { relay_limit } => {
                Self::EngineStartFailed {
                    component: "relay worker budget".to_string(),
                    reason: format!(
                        "configured max_relays {relay_limit} cannot represent its finite retirement envelope"
                    ),
                }
            }
            nmp_runtime::EngineThreadError::ObservationUnavailable { reason } => {
                Self::EngineStartFailed {
                    component: "initial observation projection".to_string(),
                    reason,
                }
            }
            nmp_runtime::EngineThreadError::MissingReplaceableCapability { program, format } => {
                Self::MissingReplaceableCapability { program, format }
            }
            // The runtime's finite shutdown drain (#8 U4) refuses new work
            // with a typed engine-level error; at this facade it is the same
            // closed-engine fact `EngineClosed` already names.
            nmp_runtime::EngineThreadError::EngineShuttingDown => Self::EngineClosed,
        }
    }

    /// Map an engine-thread failure returned while opening an observation.
    /// Canonical row/history projection refusal has its own exact internal
    /// variant; relay opens deliberately have no error edge into this mapping.
    /// `ThreadUnavailable` and `RelayBudgetOverflow` are construction-only
    /// defensive arms, while `EngineShuttingDown` remains the closed-engine
    /// fact.
    pub(crate) fn from_observe_error(error: nmp_runtime::EngineThreadError) -> Self {
        match error {
            nmp_runtime::EngineThreadError::ObservationUnavailable { reason } => {
                Self::ObservationUnavailable { reason }
            }
            nmp_runtime::EngineThreadError::ThreadUnavailable { component, reason } => {
                Self::ObservationUnavailable {
                    reason: format!("{component}: {reason}"),
                }
            }
            nmp_runtime::EngineThreadError::RelayBudgetOverflow { relay_limit } => {
                Self::ObservationUnavailable {
                    reason: format!(
                        "configured max_relays {relay_limit} cannot represent its finite retirement envelope"
                    ),
                }
            }
            nmp_runtime::EngineThreadError::MissingReplaceableCapability { program, format } => {
                Self::MissingReplaceableCapability { program, format }
            }
            nmp_runtime::EngineThreadError::EngineShuttingDown => Self::EngineClosed,
        }
    }

    pub(crate) fn from_publish_error(err: nmp_engine::core::PublishError) -> Self {
        match err {
            nmp_engine::core::PublishError::EngineShuttingDown => Self::EngineClosed,
            other => Self::PublishRefused {
                reason: other.to_string(),
            },
        }
    }

    pub(crate) fn from_add_auth_policy_error(error: nmp_runtime::AddAuthPolicyError) -> Self {
        match error {
            nmp_runtime::AddAuthPolicyError::CapabilityInstanceExhausted => {
                Self::AuthCapabilityInstanceExhausted
            }
            nmp_runtime::AddAuthPolicyError::RegistryFull { limit } => {
                Self::AuthCapabilityRegistryFull { limit }
            }
            nmp_runtime::AddAuthPolicyError::EngineShuttingDown => Self::EngineClosed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_publish_refusal_reaches_the_boundary_with_its_own_sentence() {
        // Rule 2 refusals are not a generic "publish failed": each names a
        // different instruction that cannot resolve, and each has a different
        // repair.
        for (error, expected) in [
            (
                nmp_engine::core::PublishError::NoCurrentAccount,
                "publishing with the current-account identity requires a current account",
            ),
            (
                nmp_engine::core::PublishError::SignatureInvalid {
                    reason: "bad sig".to_string(),
                },
                "the supplied signature does not verify: bad sig",
            ),
            (
                nmp_engine::core::PublishError::ReservedKind { kind: 22242 },
                "kind:22242 is reserved for reducer-owned relay authentication",
            ),
            (
                nmp_engine::core::PublishError::PersistenceFailed {
                    reason: "disk".to_string(),
                },
                "the write could not be recorded: disk",
            ),
            (
                nmp_engine::core::PublishError::AlreadyExpired,
                "the event was already expired at acceptance",
            ),
        ] {
            let mapped = EngineError::from_publish_error(error);
            assert_eq!(
                mapped,
                EngineError::PublishRefused {
                    reason: expected.to_string()
                }
            );
            assert_eq!(mapped.to_string(), expected);
        }
        // Shutdown stays its own lifecycle fact rather than folding into the
        // refusal class.
        assert_eq!(
            EngineError::from_publish_error(nmp_engine::core::PublishError::EngineShuttingDown),
            EngineError::EngineClosed
        );
    }
}
