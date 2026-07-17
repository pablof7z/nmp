//! `FfiFilter -> nmp_grammar::Filter` (and back, for the round-trip test)
//! plus `nostr::Event -> FfiRow`/`nmp` value mirrors (M4 plan §2 step A).
//! Every value mirrored from the engine side (`Durability`/`WriteIntent`/
//! `DiagnosticsSnapshot`/etc.) is sourced through the `nmp` facade's
//! re-exports, never `nmp-engine` directly (#52 Unit B) -- `nmp-ffi` has no
//! dependency on `nmp-engine` at all. Every parse of a foreign-supplied
//! string (hex ids/keys, a tag-name character, a relay URL) returns a typed
//! [`FfiError`], never a panic -- errors are values across this boundary
//! (plan §2/§6).

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;

use nmp::{
    AcquisitionEvidence, AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, AuthPhase,
    CancelWriteError, CancelWriteOutcome, CoverageInterval, DiagnosticsSnapshot,
    Durability as GDurability, FilterCoverageEntry, Frame, Lane, RelayDiagnosticsSnapshot,
    RequestRowsError, Row, RowDelta, ShortfallFact, SourceEvidence, SourceStatus, Window,
    WindowLoad, WriteIntent as GWriteIntent, WritePayload as GWritePayload,
    WriteRouting as GWriteRouting, WriteStatus as GWriteStatus,
};
use nmp_grammar::{
    AccessContext as GAccessContext, Binding as GBinding, CacheMode as GCacheMode,
    Demand as GDemand, DemandError as GDemandError, Derived as GDerived, Filter as GFilter,
    Freshness as GFreshness, IdentityField as GIdentityField, IndexedTagName,
    Selector as GSelector, SetAlgebra as GSetAlgebra, SetOp as GSetOp,
    SourceAuthority as GSourceAuthority,
};
use nostr::secp256k1::schnorr::Signature;
use nostr::{
    Event as SignedEvent, EventId, JsonUtil, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent,
};

use crate::types::{
    FfiAccessContext, FfiAcquisitionEvidence, FfiAuthDiagnostics, FfiAuthPhase, FfiBinding,
    FfiCacheMode, FfiCancelWriteError, FfiCancelWriteOutcome, FfiCoverageInterval, FfiDemand,
    FfiDerived, FfiDiagnosticsSnapshot, FfiDurability, FfiFilter, FfiFilterCoverage, FfiFrame,
    FfiFreshness, FfiIdentityField, FfiKindCount, FfiLaneCount, FfiRelayDiagnostics,
    FfiRelayInformationErrorKind, FfiRow, FfiRowDelta, FfiSelector, FfiSetAlgebra, FfiSetOp,
    FfiShortfallFact, FfiSignEventFailure, FfiSignEventRequest, FfiSignedEvent, FfiSourceAuthority,
    FfiSourceEvidence, FfiSourceStatus, FfiWindow, FfiWindowContents, FfiWindowLoad,
    FfiWriteIntent, FfiWritePayload, FfiWriteRouting, FfiWriteStatus,
};

/// Every typed failure crossing this boundary -- parse, lifecycle, storage,
/// or pre-receipt allocation states; never a panic (plan §2/§6).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiError {
    /// A `FfiFilter.tags` key was not exactly one ASCII letter (`a`-`z` or
    /// `A`-`Z`) -- the wire/local INDEXED filter alphabet (NIP-01
    /// `#<letter>` queries). This is NOT a judgment that the string is a
    /// malformed event tag (see [`Self::InvalidTag`] for that) -- a
    /// multi-character or punctuation name is perfectly valid *event* data,
    /// it simply cannot be a generic filter key. `FfiSelector::Tag`'s `name`
    /// is never checked against this rule (#64).
    NonIndexableFilterTag {
        got: String,
    },
    InvalidPublicKey {
        got: String,
    },
    /// A `FfiBinding::Literal` value in the `ids` field position was not a
    /// valid 32-byte-hex event id.
    InvalidEventId {
        got: String,
    },
    InvalidRelayUrl {
        got: String,
    },
    /// A raw `[String; N]` tag in a `FfiWriteIntent` did not parse as a
    /// valid nostr tag (`Tag::parse`) -- e.g. an empty array. Rejecting the
    /// whole intent here (rather than silently dropping the malformed tag)
    /// is what keeps the signed event identical to what the app composed.
    InvalidTag {
        got: Vec<String>,
    },
    /// `add_account`'s secret key did not parse as a valid nostr key (hex or
    /// bech32 `nsec`).
    InvalidSecretKey,
    InvalidSigner {
        reason: String,
    },
    /// The shared signer/AUTH-policy registry reached its configured bound.
    AuthCapabilityRegistryFull {
        limit: u64,
    },
    /// The exact capability-instance namespace was exhausted.
    AuthCapabilityInstanceExhausted,
    /// The sign-only operation has no active account with a registered
    /// signing capability. No operation was accepted.
    NoActiveSigner,
    /// The immutable sign-only request was malformed or named an author
    /// other than the active account. No signer was invoked.
    InvalidSignRequest {
        reason: String,
    },
    /// No upper-half correlation id remains for a publish rejected before
    /// durable acceptance. No receipt or status stream was created.
    ReceiptCorrelationIdExhausted,
    /// `NmpEngine::new`'s `store_path` pointed at a file `RedbStore::open`
    /// could not open.
    StoreOpenFailed {
        reason: String,
    },
    /// The requested closed persistent store could not be removed.
    StoreResetFailed {
        reason: String,
    },
    /// Destructive reset was refused because a live engine in this process
    /// owns the same canonical persistent-store path.
    StoreStillOpen {
        path: String,
    },
    /// One named engine/native thread could not be created. The component
    /// and safe OS reason survive Swift/Kotlin error translation unchanged.
    ThreadUnavailable {
        component: String,
        reason: String,
    },
    /// The finite native executor had no immediately-startable slot. The
    /// associated stream or operation was not accepted.
    ExecutorSaturated {
        component: String,
        capacity: u64,
    },
    /// A `FfiWritePayload::Signed`'s `sig` did not parse as a valid 64-byte
    /// hex schnorr signature.
    InvalidSignature {
        got: String,
    },
    /// [`nmp::Engine::shutdown`] has already run -- every other verb fails
    /// closed with this variant instead of racing the engine thread's own
    /// teardown. NOTE: there is deliberately no `InvalidSignedEvent` variant
    /// here anymore -- a `FfiWritePayload::Signed` that fails
    /// `nostr::Event::verify` is no longer rejected synchronously at this
    /// boundary (#52 Unit B). That guarantee moved to
    /// `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary
    /// (Unit A0/#56) so it holds for every entry point, not only this one;
    /// it surfaces on the `WriteStatus` receipt stream as `Failed` instead.
    EngineClosed,
    /// `decode_nostr_entity`'s input was not valid bech32, had an
    /// unrecognized HRP prefix, or (for `nprofile`/`nevent`/`naddr`) had a
    /// malformed inner TLV payload (#116).
    InvalidNostrEntity {
        reason: String,
    },
    /// `decode_nostr_entity`'s input decoded to `nsec`/`ncryptsec` -- refused
    /// rather than decoded, since a secret-key entity is never a valid
    /// target for a display/mention codec (#116).
    NostrEntitySecretKeyRejected,
    /// An `FfiDemand` declared `source: AuthorOutboxes` over a selection
    /// whose `authors` field is unbound (`nmp_grammar::DemandError::
    /// AuthorOutboxesRequiresBoundAuthors` mirror, #107's `demand_from_ffi`
    /// boundary).
    AuthorOutboxesRequiresBoundAuthors,
    /// An `FfiDemand` declared `source: Pinned` with an empty relay set
    /// (`nmp_grammar::DemandError::PinnedRequiresNonemptyRelaySet` mirror,
    /// #107 Contract: "the pinned relay set must be nonempty").
    EmptyPinnedRelaySet,
    /// A windowed observe declared `initial == 0` or `max == 0` in its
    /// [`FfiWindow::Expandable`] -- a window must hold at least one row.
    /// (On sub-64-bit targets this also covers a bound too large to be a
    /// platform row count -- unreachable on every supported 64-bit target --
    /// so "representable non-zero row count" is the precise invariant.)
    WindowZeroRows,
    /// A windowed observe declared `initial > max` -- the window could never
    /// legally hold its own starting set.
    WindowInitialExceedsMax {
        initial: u64,
        max: u64,
    },
    /// A windowed observe was declared over a selection that already carries
    /// a NIP-01 `limit` -- the window IS the bound; carrying both would give
    /// two competing row ceilings (`nmp::EngineError::WindowSelectionHasLimit`
    /// mirror).
    WindowSelectionHasLimit,
    /// #156: `NmpEngine::group_message_intent` requires an active account
    /// because NMP, not the native caller, owns the unsigned event author.
    NoActiveAccount,
    /// #115: `NmpEngine::publish_composed` was called a second time on the
    /// same `FfiComposedWriteIntent` handle -- it is take-once by design
    /// (recompose via `group_message_intent` again is the correct retry path,
    /// since NMP-owned event time must refresh).
    IntentAlreadyConsumed,
    /// NIP-11 acquisition failed before any last-good document existed.
    /// `ExecutorSaturated`/`RelayInformationWaitersSaturated`/
    /// `ThreadUnavailable` above already carry these three acquisition
    /// discriminants with full fidelity (#494); every other
    /// `nmp::RelayInformationError` variant carries here instead of
    /// collapsing to a message string.
    RelayInformationUnavailable {
        kind: FfiRelayInformationErrorKind,
    },
    /// A single relay's in-flight NIP-11 waiter set reached its finite
    /// admission bound. The refused caller was not retained.
    RelayInformationWaitersSaturated {
        capacity: u64,
    },
    /// #591: `FfiWriteIntent.correlation` was `Some` but failed
    /// `nmp_grammar::CorrelationToken`'s `TryFrom<&str>` bounded/non-empty
    /// validation (empty, or over `CorrelationToken::MAX_LEN` bytes). Synchronous,
    /// before any engine call -- same discipline as `InvalidPublicKey`/
    /// `InvalidTag` above.
    InvalidCorrelationToken {
        got: String,
        reason: String,
    },
}

/// Exact failure returned by `NmpQueryHandle::request_rows`
/// (`nmp::RequestRowsError` mirror). Growth is declarative -- there is no
/// token to misuse and no generation to go stale, so the only failures left
/// are the structural one (`Unwindowed`) and genuine advance failures.
/// `AtBound` is deliberately NOT here: reaching the declared `max` is a FACT
/// delivered in frames ([`FfiWindowLoad::AtBound`]), never a thrown error.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiRequestRowsError {
    /// This subscription observes the full live set; there is no window to
    /// grow.
    Unwindowed,
    EngineClosed,
    /// The canonical store could not serve the advance (the staged load was
    /// rolled back).
    StoreUnavailable,
    /// No planned source could serve the advance (the staged load was rolled
    /// back).
    TransportUnavailable {
        reason: String,
    },
}

impl From<nmp::EngineError> for FfiError {
    fn from(err: nmp::EngineError) -> Self {
        match err {
            nmp::EngineError::InvalidRelayUrl { url } => Self::InvalidRelayUrl { got: url },
            nmp::EngineError::StoreOpenFailed { reason } => Self::StoreOpenFailed { reason },
            nmp::EngineError::StoreResetFailed { reason } => Self::StoreResetFailed { reason },
            nmp::EngineError::StoreStillOpen { path } => Self::StoreStillOpen { path },
            nmp::EngineError::ThreadUnavailable { component, reason } => {
                Self::ThreadUnavailable { component, reason }
            }
            nmp::EngineError::ExecutorSaturated {
                component,
                capacity,
            } => Self::ExecutorSaturated {
                component,
                capacity: capacity as u64,
            },
            nmp::EngineError::InvalidSecretKey => Self::InvalidSecretKey,
            nmp::EngineError::SignerMissingPublicKey => Self::InvalidSigner {
                reason: "signer has no public key".to_string(),
            },
            nmp::EngineError::AuthCapabilityRegistryFull { limit } => {
                Self::AuthCapabilityRegistryFull {
                    limit: limit as u64,
                }
            }
            nmp::EngineError::AuthCapabilityInstanceExhausted => {
                Self::AuthCapabilityInstanceExhausted
            }
            nmp::EngineError::ReceiptCorrelationIdExhausted => Self::ReceiptCorrelationIdExhausted,
            nmp::EngineError::EngineClosed => Self::EngineClosed,
            nmp::EngineError::WindowInitialExceedsMax { initial, max } => {
                Self::WindowInitialExceedsMax {
                    initial: initial as u64,
                    max: max as u64,
                }
            }
            nmp::EngineError::WindowSelectionHasLimit => Self::WindowSelectionHasLimit,
        }
    }
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonIndexableFilterTag { got } => {
                write!(f, "not indexable as a filter key: {got:?}")
            }
            Self::InvalidPublicKey { got } => write!(f, "invalid public key hex: {got:?}"),
            Self::InvalidEventId { got } => write!(f, "invalid event id hex: {got:?}"),
            Self::InvalidRelayUrl { got } => write!(f, "invalid relay url: {got:?}"),
            Self::InvalidTag { got } => write!(f, "invalid tag: {got:?}"),
            Self::InvalidSecretKey => write!(f, "invalid secret key"),
            Self::InvalidSigner { reason } => write!(f, "invalid signer: {reason}"),
            Self::AuthCapabilityRegistryFull { limit } => {
                write!(f, "AUTH capability registry is full at {limit} entries")
            }
            Self::AuthCapabilityInstanceExhausted => {
                write!(f, "AUTH capability instance space exhausted")
            }
            Self::NoActiveSigner => write!(f, "the active account has no registered signer"),
            Self::InvalidSignRequest { reason } => {
                write!(f, "invalid sign request: {reason}")
            }
            Self::ReceiptCorrelationIdExhausted => {
                write!(f, "receipt correlation id namespace exhausted")
            }
            Self::StoreOpenFailed { reason } => write!(f, "could not open store: {reason}"),
            Self::StoreResetFailed { reason } => write!(f, "could not reset store: {reason}"),
            Self::StoreStillOpen { path } => {
                write!(f, "persistent store is still open: {path}")
            }
            Self::ThreadUnavailable { component, reason } => {
                write!(f, "{component} thread unavailable: {reason}")
            }
            Self::ExecutorSaturated {
                component,
                capacity,
            } => write!(
                f,
                "{component} refused: native task executor is at capacity {capacity}"
            ),
            Self::InvalidSignature { got } => write!(f, "invalid signature hex: {got:?}"),
            Self::EngineClosed => write!(f, "engine already shut down"),
            Self::InvalidNostrEntity { reason } => write!(f, "invalid nostr entity: {reason}"),
            Self::NostrEntitySecretKeyRejected => {
                write!(f, "refusing to decode a secret-key entity")
            }
            Self::AuthorOutboxesRequiresBoundAuthors => write!(
                f,
                "SourceAuthority::AuthorOutboxes requires a selection whose `authors` field is bound"
            ),
            Self::EmptyPinnedRelaySet => {
                write!(f, "SourceAuthority::Pinned requires a nonempty relay set")
            }
            Self::WindowZeroRows => {
                write!(f, "window initial/max must be representable non-zero row counts")
            }
            Self::WindowInitialExceedsMax { initial, max } => {
                write!(f, "window initial {initial} exceeds max {max}")
            }
            Self::WindowSelectionHasLimit => {
                write!(f, "a windowed selection must not also declare a limit")
            }
            Self::NoActiveAccount => write!(f, "group messages require an active account"),
            Self::IntentAlreadyConsumed => {
                write!(f, "this composed write intent was already published once")
            }
            Self::RelayInformationUnavailable { kind } => {
                write!(f, "relay information unavailable: {kind:?}")
            }
            Self::RelayInformationWaitersSaturated { capacity } => write!(
                f,
                "relay information refused: per-relay waiter capacity {capacity} is full"
            ),
            Self::InvalidCorrelationToken { got, reason } => {
                write!(f, "invalid correlation token {got:?}: {reason}")
            }
        }
    }
}

impl From<nmp::RelayInformationRequestError> for FfiError {
    fn from(error: nmp::RelayInformationRequestError) -> Self {
        match error {
            nmp::RelayInformationRequestError::Engine(error) => error.into(),
            nmp::RelayInformationRequestError::Acquisition(
                nmp::RelayInformationError::ExecutorSaturated { capacity },
            ) => Self::ExecutorSaturated {
                component: "NIP-11 acquisition".to_string(),
                capacity: capacity as u64,
            },
            nmp::RelayInformationRequestError::Acquisition(
                nmp::RelayInformationError::WaiterSaturated { capacity },
            ) => Self::RelayInformationWaitersSaturated {
                capacity: capacity as u64,
            },
            nmp::RelayInformationRequestError::Acquisition(
                nmp::RelayInformationError::ThreadUnavailable { reason },
            ) => Self::ThreadUnavailable {
                component: "NIP-11 acquisition".to_string(),
                reason,
            },
            nmp::RelayInformationRequestError::Acquisition(error) => {
                Self::RelayInformationUnavailable {
                    kind: relay_information_error_kind(error),
                }
            }
        }
    }
}

/// `nmp::RelayInformationError` -> [`FfiRelayInformationErrorKind`] (#494).
/// Every discriminant, not just the three the throw seam above special-cases
/// into shared/dedicated `FfiError` variants -- this is also the exact
/// conversion the `last_error` stale-on-error evidence field uses, so a
/// single typed carrier crosses both NIP-11 seams.
pub fn relay_information_error_kind(
    error: nmp::RelayInformationError,
) -> FfiRelayInformationErrorKind {
    match error {
        nmp::RelayInformationError::ExecutorSaturated { capacity } => {
            FfiRelayInformationErrorKind::ExecutorSaturated {
                capacity: capacity as u64,
            }
        }
        nmp::RelayInformationError::WaiterSaturated { capacity } => {
            FfiRelayInformationErrorKind::WaiterSaturated {
                capacity: capacity as u64,
            }
        }
        nmp::RelayInformationError::ThreadUnavailable { reason } => {
            FfiRelayInformationErrorKind::ThreadUnavailable { reason }
        }
        nmp::RelayInformationError::ServiceClosed => FfiRelayInformationErrorKind::ServiceClosed,
        nmp::RelayInformationError::CredentialedRelayUrl => {
            FfiRelayInformationErrorKind::CredentialedRelayUrl
        }
        nmp::RelayInformationError::Http { reason } => {
            FfiRelayInformationErrorKind::Http { reason }
        }
        nmp::RelayInformationError::ResponseTooLarge { limit_bytes } => {
            FfiRelayInformationErrorKind::ResponseTooLarge { limit_bytes }
        }
        nmp::RelayInformationError::InvalidDocument { reason } => {
            FfiRelayInformationErrorKind::InvalidDocument { reason }
        }
    }
}

pub fn sign_event_request_from_ffi(
    event: FfiSignEventRequest,
) -> Result<nmp::SignEventRequest, FfiError> {
    Ok(nmp::SignEventRequest {
        created_at: Timestamp::from(event.created_at),
        kind: nostr::Kind::from(event.kind),
        tags: tags_from_ffi(event.tags)?,
        content: event.content,
    })
}

pub fn signed_event_to_ffi(event: SignedEvent) -> FfiSignedEvent {
    FfiSignedEvent {
        id: event.id.to_hex(),
        pubkey: event.pubkey.to_hex(),
        created_at: event.created_at.as_secs(),
        kind: event.kind.as_u16(),
        tags: event.tags.iter().map(|tag| tag.clone().to_vec()).collect(),
        content: event.content,
        sig: event.sig.to_string(),
    }
}

pub fn sign_event_start_error(error: nmp::SignEventError) -> FfiError {
    match error {
        nmp::SignEventError::NoActiveSigner => FfiError::NoActiveSigner,
        nmp::SignEventError::InvalidRequest { reason } => FfiError::InvalidSignRequest { reason },
        nmp::SignEventError::ExecutorSaturated { capacity } => FfiError::ExecutorSaturated {
            component: "sign-event".to_string(),
            capacity: capacity as u64,
        },
        nmp::SignEventError::ThreadUnavailable { component, reason } => {
            FfiError::ThreadUnavailable { component, reason }
        }
        nmp::SignEventError::EngineClosed => FfiError::EngineClosed,
        nmp::SignEventError::SignerUnavailable { reason }
        | nmp::SignEventError::SignerRejected { reason }
        | nmp::SignEventError::InvalidSignerOutput { reason } => FfiError::InvalidSigner { reason },
        nmp::SignEventError::Cancelled => FfiError::EngineClosed,
    }
}

pub fn sign_event_failure(error: nmp::SignEventError) -> FfiSignEventFailure {
    match error {
        nmp::SignEventError::SignerUnavailable { reason } => {
            FfiSignEventFailure::SignerUnavailable { reason }
        }
        nmp::SignEventError::SignerRejected { reason } => {
            FfiSignEventFailure::SignerRejected { reason }
        }
        nmp::SignEventError::InvalidSignerOutput { reason } => {
            FfiSignEventFailure::InvalidSignerOutput { reason }
        }
        nmp::SignEventError::Cancelled => FfiSignEventFailure::Cancelled,
        other => FfiSignEventFailure::InvalidSignerOutput {
            reason: format!("unexpected post-acceptance sign failure: {other}"),
        },
    }
}

impl std::error::Error for FfiError {}

impl From<RequestRowsError> for FfiRequestRowsError {
    fn from(error: RequestRowsError) -> Self {
        match error {
            RequestRowsError::Unwindowed => Self::Unwindowed,
            RequestRowsError::EngineClosed => Self::EngineClosed,
            RequestRowsError::StoreUnavailable => Self::StoreUnavailable,
            RequestRowsError::TransportUnavailable { reason } => {
                Self::TransportUnavailable { reason }
            }
            // `nmp::RequestRowsError` is `#[non_exhaustive]`: a variant added
            // upstream before this seam learns it must still cross typed --
            // as an advance failure carrying the upstream Display -- never a
            // panic across the FFI boundary.
            other => Self::TransportUnavailable {
                reason: other.to_string(),
            },
        }
    }
}

impl std::fmt::Display for FfiRequestRowsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unwindowed => f.write_str(
                "this subscription observes the full live set; there is no window to grow",
            ),
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::StoreUnavailable => {
                f.write_str("window advance could not read or resolve the canonical store")
            }
            Self::TransportUnavailable { reason } => {
                write!(f, "window advance transport unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for FfiRequestRowsError {}

impl From<GDemandError> for FfiError {
    fn from(err: GDemandError) -> Self {
        match err {
            GDemandError::AuthorOutboxesRequiresBoundAuthors => {
                Self::AuthorOutboxesRequiresBoundAuthors
            }
            GDemandError::PinnedRequiresNonemptyRelaySet => Self::EmptyPinnedRelaySet,
        }
    }
}

impl From<nmp_nip29::GroupMessageError> for FfiError {
    fn from(err: nmp_nip29::GroupMessageError) -> Self {
        match err {
            nmp_nip29::GroupMessageError::Engine(error) => error.into(),
            nmp_nip29::GroupMessageError::SignedOut => Self::NoActiveAccount,
        }
    }
}

#[cfg(test)]
mod engine_error_tests {
    use super::*;

    #[test]
    fn receipt_correlation_exhaustion_remains_a_typed_ffi_error() {
        let error = FfiError::from(nmp::EngineError::ReceiptCorrelationIdExhausted);
        assert_eq!(error, FfiError::ReceiptCorrelationIdExhausted);
        assert_eq!(
            error.to_string(),
            "receipt correlation id namespace exhausted"
        );
    }

    #[test]
    fn live_store_reset_refusal_remains_a_typed_ffi_error() {
        let error = FfiError::from(nmp::EngineError::StoreStillOpen {
            path: "/canonical/nmp.redb".to_string(),
        });
        assert_eq!(
            error,
            FfiError::StoreStillOpen {
                path: "/canonical/nmp.redb".to_string(),
            }
        );
        assert_eq!(
            error.to_string(),
            "persistent store is still open: /canonical/nmp.redb"
        );
    }

    #[test]
    fn auth_capability_refusals_remain_typed_ffi_errors() {
        assert_eq!(
            FfiError::from(nmp::EngineError::AuthCapabilityRegistryFull { limit: 3 }),
            FfiError::AuthCapabilityRegistryFull { limit: 3 }
        );
        assert_eq!(
            FfiError::from(nmp::EngineError::AuthCapabilityInstanceExhausted),
            FfiError::AuthCapabilityInstanceExhausted
        );
    }
}

#[cfg(test)]
mod window_conversion_tests {
    use super::*;

    fn expandable(initial: u64, max: u64) -> Option<FfiWindow> {
        Some(FfiWindow::Expandable { initial, max })
    }

    #[test]
    fn absent_window_passes_through_as_the_unbounded_observation() {
        assert_eq!(window_from_ffi(None).unwrap(), None);
    }

    #[test]
    fn window_validation_stays_typed_at_the_ffi_boundary() {
        assert_eq!(
            window_from_ffi(expandable(0, 10)).unwrap_err(),
            FfiError::WindowZeroRows
        );
        assert_eq!(
            window_from_ffi(expandable(1, 0)).unwrap_err(),
            FfiError::WindowZeroRows
        );
        assert_eq!(
            window_from_ffi(expandable(11, 10)).unwrap_err(),
            FfiError::WindowInitialExceedsMax {
                initial: 11,
                max: 10,
            }
        );
    }

    #[test]
    fn valid_window_builds_exact_non_zero_bounds() {
        match window_from_ffi(expandable(2, 50)).unwrap() {
            Some(Window::Expandable { initial, max }) => {
                assert_eq!(initial.get(), 2);
                assert_eq!(max.get(), 50);
            }
            other => panic!("expected Window::Expandable, got {other:?}"),
        }
    }

    #[test]
    fn window_load_facts_map_variant_for_variant() {
        assert_eq!(window_load_to_ffi(WindowLoad::Idle), FfiWindowLoad::Idle);
        assert_eq!(
            window_load_to_ffi(WindowLoad::Requesting),
            FfiWindowLoad::Requesting
        );
        assert_eq!(
            window_load_to_ffi(WindowLoad::Returned { added: 3 }),
            FfiWindowLoad::Returned { added: 3 }
        );
        assert_eq!(
            window_load_to_ffi(WindowLoad::AtBound { max: 20 }),
            FfiWindowLoad::AtBound { max: 20 }
        );
    }

    #[test]
    fn request_rows_errors_preserve_every_failure_axis() {
        assert_eq!(
            FfiRequestRowsError::from(RequestRowsError::Unwindowed),
            FfiRequestRowsError::Unwindowed
        );
        assert_eq!(
            FfiRequestRowsError::from(RequestRowsError::EngineClosed),
            FfiRequestRowsError::EngineClosed
        );
        assert_eq!(
            FfiRequestRowsError::from(RequestRowsError::StoreUnavailable),
            FfiRequestRowsError::StoreUnavailable
        );
        assert_eq!(
            FfiRequestRowsError::from(RequestRowsError::TransportUnavailable {
                reason: "offline".to_string(),
            }),
            FfiRequestRowsError::TransportUnavailable {
                reason: "offline".to_string(),
            }
        );
    }

    #[test]
    fn windowed_frame_ships_the_snapshot_and_drops_deltas_on_the_wire() {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9_999), "windowed")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");
        let relay = RelayUrl::parse("wss://window.example").unwrap();
        let row = Row {
            event: event.clone(),
            sources: std::collections::BTreeSet::from([relay]),
        };
        let frame = Frame {
            // Receiver-derived deltas exist Rust-side for windowed frames --
            // the wire must drop them (never carry rows twice).
            deltas: vec![RowDelta::Added(row.clone())],
            window: Some(nmp::WindowContents {
                rows: vec![row],
                load: WindowLoad::Returned { added: 1 },
            }),
            evidence: AcquisitionEvidence {
                sources: vec![],
                shortfall: vec![],
            },
        };

        let ffi = frame_to_ffi(frame);
        assert!(
            ffi.deltas.is_empty(),
            "windowed frames must never ship wire deltas alongside the snapshot"
        );
        let window = ffi.window.expect("windowed frame must carry its contents");
        assert_eq!(window.rows.len(), 1);
        assert_eq!(window.rows[0].id, event.id.to_hex());
        assert_eq!(window.load, FfiWindowLoad::Returned { added: 1 });
    }

    #[test]
    fn unbounded_frame_ships_deltas_and_no_window() {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9_999), "unbounded")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");
        let relay = RelayUrl::parse("wss://unbounded.example").unwrap();
        let row = Row {
            event: event.clone(),
            sources: std::collections::BTreeSet::from([relay]),
        };
        let frame = Frame {
            deltas: vec![RowDelta::Added(row)],
            window: None,
            evidence: AcquisitionEvidence {
                sources: vec![],
                shortfall: vec![],
            },
        };

        let ffi = frame_to_ffi(frame);
        assert_eq!(ffi.window, None);
        assert_eq!(ffi.deltas.len(), 1);
        match &ffi.deltas[0] {
            FfiRowDelta::Added { row } => assert_eq!(row.id, event.id.to_hex()),
            other => panic!("expected FfiRowDelta::Added, got {other:?}"),
        }
    }
}

/// Parse an `FfiFilter.tags` key -- the wire/local INDEXED filter alphabet
/// only. Exactly one ASCII letter (`a`-`z`/`A`-`Z`) is accepted; anything
/// else (empty, multi-character, digit, punctuation) fails with a typed
/// [`FfiError::NonIndexableFilterTag`], never a whitelist rejection. This is
/// NOT used for `FfiSelector::Tag`'s `name` -- that is an arbitrary
/// event-tag key and passes through unchecked (#64).
pub fn indexed_tag_name_from_ffi(s: &str) -> Result<IndexedTagName, FfiError> {
    let mut chars = s.chars();
    let only = chars.next();
    match (only, chars.next()) {
        (Some(c), None) => IndexedTagName::new(c)
            .ok_or_else(|| FfiError::NonIndexableFilterTag { got: s.to_string() }),
        _ => Err(FfiError::NonIndexableFilterTag { got: s.to_string() }),
    }
}

fn identity_field_from_ffi(f: FfiIdentityField) -> GIdentityField {
    match f {
        FfiIdentityField::ActivePubkey => GIdentityField::ActivePubkey,
    }
}

fn identity_field_to_ffi(f: GIdentityField) -> FfiIdentityField {
    match f {
        GIdentityField::ActivePubkey => FfiIdentityField::ActivePubkey,
    }
}

fn selector_from_ffi(s: FfiSelector) -> Result<GSelector, FfiError> {
    Ok(match s {
        FfiSelector::Authors => GSelector::Authors,
        FfiSelector::Ids => GSelector::Ids,
        // Arbitrary event-tag key (#64) -- NOT run through
        // `indexed_tag_name_from_ffi`. Selector::Tag projects already-
        // acquired events locally; it never inherits the wire filter's
        // single-letter restriction, so every string is accepted verbatim.
        FfiSelector::Tag { name } => GSelector::Tag(name),
        FfiSelector::AddressCoord => GSelector::AddressCoord,
    })
}

fn selector_to_ffi(s: GSelector) -> FfiSelector {
    match s {
        GSelector::Authors => FfiSelector::Authors,
        GSelector::Ids => FfiSelector::Ids,
        GSelector::Tag(name) => FfiSelector::Tag { name },
        GSelector::AddressCoord => FfiSelector::AddressCoord,
    }
}

fn set_algebra_from_ffi(a: FfiSetAlgebra) -> GSetAlgebra {
    match a {
        FfiSetAlgebra::Union => GSetAlgebra::Union,
        FfiSetAlgebra::Intersect => GSetAlgebra::Intersect,
        FfiSetAlgebra::Diff => GSetAlgebra::Diff,
    }
}

fn set_algebra_to_ffi(a: GSetAlgebra) -> FfiSetAlgebra {
    match a {
        GSetAlgebra::Union => FfiSetAlgebra::Union,
        GSetAlgebra::Intersect => FfiSetAlgebra::Intersect,
        GSetAlgebra::Diff => FfiSetAlgebra::Diff,
    }
}

/// Which field a [`FfiBinding::Literal`] is being parsed for -- `authors`
/// and `ids` carry a hex-encoding invariant that `ConcreteFilter::to_nostr`
/// (nmp-grammar) later PANICS on if violated (its own doc: "a genuine
/// invariant violation upstream, not a reachable user input error"). This
/// boundary is exactly that upstream: a foreign-supplied `Literal` string is
/// unchecked until here, so an FFI caller passing a bad hex string must get
/// a typed [`FfiError`], never let the panic fire two crates downstream.
/// Tag values have no such invariant (`to_nostr` never parses them as
/// hex) so `Tag` values pass through unchecked, same as before.
#[derive(Clone, Copy)]
enum LiteralField {
    Authors,
    Ids,
    Tag,
}

fn validate_literal(field: LiteralField, value: String) -> Result<String, FfiError> {
    match field {
        LiteralField::Authors => {
            parse_pubkey(&value)?;
            Ok(value)
        }
        LiteralField::Ids => {
            nostr::EventId::from_hex(&value)
                .map_err(|_| FfiError::InvalidEventId { got: value.clone() })?;
            Ok(value)
        }
        LiteralField::Tag => Ok(value),
    }
}

fn binding_from_ffi(b: FfiBinding, field: LiteralField) -> Result<GBinding, FfiError> {
    Ok(match b {
        FfiBinding::Literal { values } => GBinding::Literal(
            values
                .into_iter()
                .map(|v| validate_literal(field, v))
                .collect::<Result<_, _>>()?,
        ),
        FfiBinding::Reactive { field: id_field } => {
            GBinding::Reactive(identity_field_from_ffi(id_field))
        }
        FfiBinding::Derived { derived } => GBinding::Derived(Box::new(GDerived {
            // #106: the FFI surface stays `Filter`-only (no `Demand`/
            // `SourceAuthority` exposed to apps yet) -- `Demand::
            // from_filter`'s static default applies transparently at this
            // boundary, identical to every direct-Rust `LiveQuery::
            // from_filter` call site.
            inner: GDemand::from_filter(filter_from_ffi(derived.inner.clone())?),
            project: selector_from_ffi(derived.project.clone())?,
        })),
        FfiBinding::SetOp { set_op } => GBinding::SetOp(Box::new(GSetOp {
            op: set_algebra_from_ffi(set_op.op),
            operands: set_op
                .operands
                .iter()
                .cloned()
                .map(|op| binding_from_ffi(op, field))
                .collect::<Result<_, _>>()?,
        })),
    })
}

pub fn binding_to_ffi(b: GBinding) -> FfiBinding {
    match b {
        GBinding::Literal(values) => FfiBinding::Literal {
            values: values.into_iter().collect(),
        },
        GBinding::Reactive(f) => FfiBinding::Reactive {
            field: identity_field_to_ffi(f),
        },
        GBinding::Derived(d) => FfiBinding::Derived {
            derived: std::sync::Arc::new(FfiDerived {
                // The inverse of `binding_from_ffi`'s `Derived` arm: only
                // the selection crosses back out (`source`/`access` are
                // not yet part of the FFI surface, #106) -- reconstructed
                // identically by `Demand::from_filter` on the way back in.
                inner: filter_to_ffi(d.inner.selection),
                project: selector_to_ffi(d.project),
            }),
        },
        GBinding::SetOp(s) => FfiBinding::SetOp {
            set_op: std::sync::Arc::new(FfiSetOp {
                op: set_algebra_to_ffi(s.op),
                operands: s.operands.into_iter().map(binding_to_ffi).collect(),
            }),
        },
    }
}

pub fn filter_from_ffi(f: FfiFilter) -> Result<GFilter, FfiError> {
    let mut tags = BTreeMap::new();
    for (k, v) in f.tags {
        tags.insert(
            indexed_tag_name_from_ffi(&k)?,
            binding_from_ffi(v, LiteralField::Tag)?,
        );
    }
    Ok(GFilter {
        kinds: f.kinds.map(|ks| ks.into_iter().collect()),
        authors: f
            .authors
            .map(|b| binding_from_ffi(b, LiteralField::Authors))
            .transpose()?,
        ids: f
            .ids
            .map(|b| binding_from_ffi(b, LiteralField::Ids))
            .transpose()?,
        tags,
        since: f.since,
        until: f.until,
        limit: f.limit.map(|l| l as usize),
    })
}

pub fn filter_to_ffi(f: GFilter) -> FfiFilter {
    FfiFilter {
        kinds: f.kinds.map(|ks| ks.into_iter().collect()),
        authors: f.authors.map(binding_to_ffi),
        ids: f.ids.map(binding_to_ffi),
        tags: f
            .tags
            .into_iter()
            .map(|(k, v)| (k.as_char().to_string(), binding_to_ffi(v)))
            .collect::<HashMap<_, _>>(),
        since: f.since,
        until: f.until,
        limit: f.limit.map(|l| l as u32),
    }
}

/// Parse+canonicalize a `FfiSourceAuthority::Pinned`'s raw URL strings --
/// `nostr::RelayUrl::parse` gives the canonicalization (#107 Contract:
/// "URL-canonicalized"), and collecting into a `BTreeSet` gives sort +
/// dedup for free (the rest of the Contract's clause).
fn source_authority_from_ffi(s: FfiSourceAuthority) -> Result<GSourceAuthority, FfiError> {
    Ok(match s {
        FfiSourceAuthority::AuthorOutboxes => GSourceAuthority::AuthorOutboxes,
        FfiSourceAuthority::Public => GSourceAuthority::Public,
        FfiSourceAuthority::Pinned { relays } => GSourceAuthority::Pinned(
            relays
                .into_iter()
                .map(|url| {
                    RelayUrl::parse(&url).map_err(|_| FfiError::InvalidRelayUrl { got: url })
                })
                .collect::<Result<_, _>>()?,
        ),
    })
}

fn source_authority_to_ffi(s: GSourceAuthority) -> FfiSourceAuthority {
    match s {
        GSourceAuthority::AuthorOutboxes => FfiSourceAuthority::AuthorOutboxes,
        GSourceAuthority::Public => FfiSourceAuthority::Public,
        GSourceAuthority::Pinned(relays) => FfiSourceAuthority::Pinned {
            relays: relays.into_iter().map(|r| r.to_string()).collect(),
        },
    }
}

fn access_context_from_ffi(a: FfiAccessContext) -> Result<GAccessContext, FfiError> {
    Ok(match a {
        FfiAccessContext::Public => GAccessContext::Public,
        FfiAccessContext::Nip42 { public_key } => GAccessContext::Nip42(parse_pubkey(&public_key)?),
    })
}

fn access_context_to_ffi(a: GAccessContext) -> FfiAccessContext {
    match a {
        GAccessContext::Public => FfiAccessContext::Public,
        GAccessContext::Nip42(public_key) => FfiAccessContext::Nip42 {
            public_key: public_key.to_hex(),
        },
    }
}

fn cache_mode_from_ffi(c: FfiCacheMode) -> GCacheMode {
    match c {
        FfiCacheMode::Agnostic => GCacheMode::Agnostic,
        FfiCacheMode::Strict => GCacheMode::Strict,
    }
}

fn cache_mode_to_ffi(c: GCacheMode) -> FfiCacheMode {
    match c {
        GCacheMode::Agnostic => FfiCacheMode::Agnostic,
        GCacheMode::Strict => FfiCacheMode::Strict,
    }
}

fn freshness_from_ffi(freshness: FfiFreshness) -> GFreshness {
    match freshness {
        FfiFreshness::Live => GFreshness::Live,
        FfiFreshness::MaxAge { seconds } => GFreshness::MaxAge { seconds },
        FfiFreshness::CacheOnly => GFreshness::CacheOnly,
    }
}

fn freshness_to_ffi(freshness: GFreshness) -> FfiFreshness {
    match freshness {
        GFreshness::Live => FfiFreshness::Live,
        GFreshness::MaxAge { seconds } => FfiFreshness::MaxAge { seconds },
        GFreshness::CacheOnly => FfiFreshness::CacheOnly,
    }
}

/// `FfiDemand -> nmp_grammar::Demand` -- the explicit, validating
/// constructor (#107). Unlike `Demand::from_filter`'s total static default,
/// this can fail: an unbound-author `AuthorOutboxes` selection or an empty
/// `Pinned` relay set is rejected here with a typed [`FfiError`], never a
/// panic, mirroring `Demand::new`'s own `DemandError` exactly.
pub fn demand_from_ffi(d: FfiDemand) -> Result<GDemand, FfiError> {
    let mut demand = GDemand::new(
        filter_from_ffi(d.selection)?,
        source_authority_from_ffi(d.source)?,
        access_context_from_ffi(d.access)?,
    )?;
    demand.cache = cache_mode_from_ffi(d.cache);
    demand.freshness = freshness_from_ffi(d.freshness);
    Ok(demand)
}

pub fn demand_to_ffi(d: GDemand) -> FfiDemand {
    FfiDemand {
        selection: filter_to_ffi(d.selection),
        source: source_authority_to_ffi(d.source),
        access: access_context_to_ffi(d.access),
        cache: cache_mode_to_ffi(d.cache),
        freshness: freshness_to_ffi(d.freshness),
    }
}

/// Raw tokens only (ledger #12) -- no formatted field is ever built here.
/// `sources` (#105) is likewise raw: the row's relay-observation set,
/// verbatim URLs, sorted (the caller's `BTreeSet<RelayUrl>` iteration order).
pub fn row_to_ffi_row(row: &Row) -> FfiRow {
    let e = &row.event;
    FfiRow {
        id: e.id.to_hex(),
        pubkey: e.pubkey.to_hex(),
        created_at: e.created_at.as_secs(),
        kind: e.kind.as_u16(),
        tags: e.tags.iter().map(|t| t.clone().to_vec()).collect(),
        content: e.content.clone(),
        sig: e.sig.to_string(),
        sources: row.sources.iter().map(RelayUrl::to_string).collect(),
    }
}

pub fn row_delta_to_ffi(d: &RowDelta) -> FfiRowDelta {
    match d {
        RowDelta::Added(row) => FfiRowDelta::Added {
            row: row_to_ffi_row(row),
        },
        RowDelta::SourcesGrew { id, sources } => FfiRowDelta::SourcesGrew {
            id: id.to_hex(),
            sources: sources.iter().map(RelayUrl::to_string).collect(),
        },
        RowDelta::Removed(id) => FfiRowDelta::Removed { id: id.to_hex() },
    }
}

fn auth_phase_to_ffi(p: AuthPhase) -> FfiAuthPhase {
    match p {
        AuthPhase::AwaitingChallenge => FfiAuthPhase::AwaitingChallenge,
        AuthPhase::AwaitingPolicy => FfiAuthPhase::AwaitingPolicy,
        AuthPhase::AwaitingSignature => FfiAuthPhase::AwaitingSignature,
        AuthPhase::AwaitingRelayAck => FfiAuthPhase::AwaitingRelayAck,
    }
}

fn source_status_to_ffi(s: SourceStatus) -> FfiSourceStatus {
    match s {
        SourceStatus::Requesting => FfiSourceStatus::Requesting,
        SourceStatus::Connecting => FfiSourceStatus::Connecting,
        SourceStatus::Disconnected => FfiSourceStatus::Disconnected,
        SourceStatus::AwaitingAuth { phase } => FfiSourceStatus::AwaitingAuth {
            phase: auth_phase_to_ffi(phase),
        },
        SourceStatus::AuthDenied => FfiSourceStatus::AuthDenied,
        SourceStatus::Error => FfiSourceStatus::Error,
    }
}

fn source_evidence_to_ffi(s: SourceEvidence) -> FfiSourceEvidence {
    FfiSourceEvidence {
        relay: s.relay.to_string(),
        access: access_context_to_ffi(s.access),
        reconciled_through: s.reconciled_through.map(|ts| ts.as_secs()),
        status: source_status_to_ffi(s.status),
    }
}

/// `ShortfallFact`'s `atom: ConcreteFilter` renders to the EXACT wire JSON
/// (`ConcreteFilter::to_nostr().as_json()`) -- the same rendering discipline
/// `diagnostics_snapshot_to_ffi`/`relay_diagnostics_to_ffi` already use for
/// every other `ConcreteFilter` crossing this boundary, never a fabricated
/// summary.
fn shortfall_fact_to_ffi(f: ShortfallFact) -> FfiShortfallFact {
    match f {
        ShortfallFact::NoPlannedSource { atom } => FfiShortfallFact::NoPlannedSource {
            atom: atom.to_nostr().as_json(),
        },
        ShortfallFact::NoResolvedDemand => FfiShortfallFact::NoResolvedDemand,
        ShortfallFact::LocalLimit { atom } => FfiShortfallFact::LocalLimit {
            atom: atom.to_nostr().as_json(),
        },
    }
}

/// `nmp::AcquisitionEvidence -> FfiAcquisitionEvidence` (the scoped,
/// per-query surface every `FfiFrame`/`RowObserver::on_frame` carries --
/// ratified codex-nova names, see `types.rs`'s own doc). Replaces the
/// deleted query-level collapse: every source's facts map faithfully, never
/// rolled up into a verdict.
pub fn evidence_to_ffi(e: AcquisitionEvidence) -> FfiAcquisitionEvidence {
    FfiAcquisitionEvidence {
        sources: e.sources.into_iter().map(source_evidence_to_ffi).collect(),
        shortfall: e.shortfall.into_iter().map(shortfall_fact_to_ffi).collect(),
    }
}

/// `Option<FfiWindow> -> Option<nmp::Window>` -- the windowed-observe
/// validation seam (#485). `None` passes through untouched (the unbounded
/// delta observation, semantics unchanged). `Some` is validated here, before
/// any engine resource is reserved: a zero bound is
/// [`FfiError::WindowZeroRows`], `initial > max` is
/// [`FfiError::WindowInitialExceedsMax`] -- typed values, never a panic.
/// (`WindowSelectionHasLimit` is NOT checked here: only the engine sees the
/// resolved selection, so that conflict surfaces from `Engine::observe`
/// through the same typed [`FfiError`].)
pub fn window_from_ffi(window: Option<FfiWindow>) -> Result<Option<Window>, FfiError> {
    match window {
        None => Ok(None),
        Some(FfiWindow::Expandable { initial, max }) => {
            if initial == 0 || max == 0 {
                return Err(FfiError::WindowZeroRows);
            }
            if initial > max {
                return Err(FfiError::WindowInitialExceedsMax { initial, max });
            }
            // `usize::try_from` can only fail on sub-64-bit targets (a bound
            // wider than the platform's addressable row count); it shares
            // the "not a representable non-zero row count" variant with the
            // zero case rather than panicking across the boundary.
            let initial = usize::try_from(initial)
                .ok()
                .and_then(NonZeroUsize::new)
                .ok_or(FfiError::WindowZeroRows)?;
            let max = usize::try_from(max)
                .ok()
                .and_then(NonZeroUsize::new)
                .ok_or(FfiError::WindowZeroRows)?;
            Ok(Some(Window::Expandable { initial, max }))
        }
    }
}

/// `nmp::WindowLoad -> FfiWindowLoad` -- the mechanical growth fact of an
/// expandable window, mapped variant-for-variant.
pub fn window_load_to_ffi(load: WindowLoad) -> FfiWindowLoad {
    match load {
        WindowLoad::Idle => FfiWindowLoad::Idle,
        WindowLoad::Requesting => FfiWindowLoad::Requesting,
        WindowLoad::Returned { added } => FfiWindowLoad::Returned {
            added: added as u64,
        },
        WindowLoad::AtBound { max } => FfiWindowLoad::AtBound { max: max as u64 },
        // `nmp::WindowLoad` is `#[non_exhaustive]`: a growth fact added
        // upstream before this seam learns it must degrade to the quiescent
        // fact, never panic across the FFI boundary. (Adding the real mirror
        // variant is the mechanical fix when the upstream enum grows.)
        _ => FfiWindowLoad::Idle,
    }
}

/// `nmp::Frame -> FfiFrame` -- the ONE wire shape both observation modes
/// share, with delivery derived from boundedness (#485):
///
/// - Unbounded (`frame.window == None`): map the engine-composed exact rebased
///   deltas; the full set is never redelivered (full-set redelivery is the
///   O(rows squared) P0).
/// - Windowed: ship the complete bounded row set + growth fact and DROP the
///   receiver-derived deltas on the wire -- the native bridges replace their
///   row state from the snapshot, so carrying deltas too would cross every
///   row the FFI boundary twice only to be folded and discarded.
pub fn frame_to_ffi(frame: Frame) -> FfiFrame {
    match frame.window {
        Some(contents) => FfiFrame {
            deltas: Vec::new(),
            window: Some(FfiWindowContents {
                rows: contents.rows.iter().map(row_to_ffi_row).collect(),
                load: window_load_to_ffi(contents.load),
            }),
            evidence: evidence_to_ffi(frame.evidence),
        },
        None => FfiFrame {
            deltas: frame.deltas.iter().map(row_delta_to_ffi).collect(),
            window: None,
            evidence: evidence_to_ffi(frame.evidence),
        },
    }
}

/// `nmp::CoverageInterval -> FfiCoverageInterval` -- the engine-global
/// DIAGNOSTICS watermark mirror, deliberately distinct from
/// [`evidence_to_ffi`]'s scoped query surface.
fn coverage_interval_to_ffi(i: CoverageInterval) -> FfiCoverageInterval {
    FfiCoverageInterval {
        from: i.from.as_secs(),
        through: i.through.as_secs(),
    }
}

pub fn write_status_to_ffi(s: WriteStatusRef<'_>) -> FfiWriteStatus {
    match s.0 {
        GWriteStatus::Accepted => FfiWriteStatus::Accepted,
        GWriteStatus::Cancelled => FfiWriteStatus::Cancelled,
        GWriteStatus::AwaitingCapability { pubkey } => FfiWriteStatus::AwaitingCapability {
            pubkey: pubkey.to_hex(),
        },
        GWriteStatus::Signed(id) => FfiWriteStatus::Signed {
            event_id: id.to_hex(),
        },
        GWriteStatus::Routed(relays) => FfiWriteStatus::Routed {
            relays: relays.iter().map(RelayUrl::to_string).collect(),
        },
        GWriteStatus::AwaitingRelay { relay } => FfiWriteStatus::AwaitingRelay {
            relay: relay.to_string(),
        },
        GWriteStatus::AwaitingAuth { relay } => FfiWriteStatus::AwaitingAuth {
            relay: relay.to_string(),
        },
        GWriteStatus::RetryEligible {
            relay,
            attempt,
            eligible_at,
        } => FfiWriteStatus::RetryEligible {
            relay: relay.to_string(),
            attempt: *attempt,
            eligible_at: eligible_at.as_secs(),
        },
        GWriteStatus::HandoffAmbiguous {
            relay,
            attempt,
            observed_at,
        } => FfiWriteStatus::HandoffAmbiguous {
            relay: relay.to_string(),
            attempt: *attempt,
            observed_at: observed_at.as_secs(),
        },
        GWriteStatus::Sent {
            relay,
            attempt,
            written_at,
        } => FfiWriteStatus::Sent {
            relay: relay.to_string(),
            attempt: *attempt,
            written_at: written_at.as_secs(),
        },
        GWriteStatus::Acked(relay) => FfiWriteStatus::Acked {
            relay: relay.to_string(),
        },
        GWriteStatus::Rejected(relay, reason) => FfiWriteStatus::Rejected {
            relay: relay.to_string(),
            reason: reason.clone(),
        },
        GWriteStatus::GaveUp(relay) => FfiWriteStatus::GaveUp {
            relay: relay.to_string(),
        },
        GWriteStatus::PersistenceBlocked(relay) => FfiWriteStatus::PersistenceBlocked {
            relay: relay.to_string(),
        },
        GWriteStatus::RoutePersistenceBlocked(relay) => FfiWriteStatus::RoutePersistenceBlocked {
            relay: relay.to_string(),
        },
        GWriteStatus::OutcomeUnknown(relay) => FfiWriteStatus::OutcomeUnknown {
            relay: relay.to_string(),
        },
        GWriteStatus::ReplaceableConflict { expected, actual } => {
            FfiWriteStatus::ReplaceableConflict {
                expected: expected.map(|id| id.to_hex()),
                actual: actual.map(|id| id.to_hex()),
            }
        }
        GWriteStatus::Failed(reason) => FfiWriteStatus::Failed {
            reason: reason.clone(),
        },
    }
}

pub fn cancel_write_error_to_ffi(error: CancelWriteError) -> FfiCancelWriteError {
    match error {
        CancelWriteError::UnknownReceipt { receipt_id } => FfiCancelWriteError::UnknownReceipt {
            receipt_id: receipt_id.0,
        },
        CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        } => FfiCancelWriteError::AlreadySigned {
            receipt_id: receipt_id.0,
            event_id: event_id.to_hex(),
        },
        CancelWriteError::AlreadyCompensated { receipt_id } => {
            FfiCancelWriteError::AlreadyCompensated {
                receipt_id: receipt_id.0,
            }
        }
        CancelWriteError::AlreadyAbandoned { receipt_id } => {
            FfiCancelWriteError::AlreadyAbandoned {
                receipt_id: receipt_id.0,
            }
        }
        CancelWriteError::PersistenceFailed { receipt_id, reason } => {
            FfiCancelWriteError::PersistenceFailed {
                receipt_id: receipt_id.0,
                reason,
            }
        }
        CancelWriteError::EngineClosed => FfiCancelWriteError::EngineClosed,
    }
}

pub fn cancel_write_outcome_to_ffi(outcome: CancelWriteOutcome) -> FfiCancelWriteOutcome {
    match outcome {
        CancelWriteOutcome::Cancelled => FfiCancelWriteOutcome::Cancelled,
    }
}

/// `nmp_router::Lane` -> a stable string label (M5 plan §1.1). Rendered as a
/// string rather than an `Enum` mirror because the diagnostics screen only
/// ever displays it -- there is no round-trip/construction need the way
/// `FfiSelector`/`FfiBinding` have for the filter grammar.
fn lane_to_ffi_string(lane: Lane) -> String {
    match lane {
        Lane::Nip65Write => "nip65_write",
        Lane::Hint => "hint",
        Lane::Provenance => "provenance",
        Lane::UserConfigured => "user_configured",
        Lane::IndexerDiscovery => "indexer_discovery",
        Lane::GroupHost => "group_host",
        Lane::DmInbox => "dm_inbox",
        Lane::Nip65Read => "nip65_read",
        Lane::AppRelay => "app_relay",
        Lane::Fallback => "fallback",
        Lane::ExplicitPinned => "explicit_pinned",
    }
    .to_string()
}

fn relay_diagnostics_to_ffi(r: RelayDiagnosticsSnapshot) -> FfiRelayDiagnostics {
    FfiRelayDiagnostics {
        relay: r.relay.to_string(),
        access: access_context_to_ffi(r.access),
        wire_sub_count: r.wire_sub_count as u32,
        authors_served: r.authors_served as u32,
        by_lane: r
            .by_lane
            .into_iter()
            .map(|(lane, count)| FfiLaneCount {
                lane: lane_to_ffi_string(lane),
                count: count as u32,
            })
            .collect(),
        filters: r.filters,
        events_by_kind: r
            .events_by_kind
            .into_iter()
            .map(|(kind, count)| FfiKindCount { kind, count })
            .collect(),
        coverage: r
            .coverage
            .into_iter()
            .map(|entry: FilterCoverageEntry| FfiFilterCoverage {
                filter: entry.filter,
                coverage: entry.coverage.map(coverage_interval_to_ffi),
            })
            .collect(),
        nip11_supported_nips: r.nip11_supported_nips,
        nip11_document_revision: r.nip11_document_revision,
        nip11_freshness: r.nip11_freshness.map(str::to_string),
        nip11_last_error: r.nip11_last_error,
        nip77_advertisement: r.nip77_advertisement.to_string(),
        nip77_behavior: r.nip77_behavior.to_string(),
        nip77_handoff: r.nip77_handoff.to_string(),
    }
}

fn auth_diagnostics_phase_to_ffi(phase: AuthDiagnosticsPhase) -> FfiAuthPhase {
    match phase {
        AuthDiagnosticsPhase::AwaitingChallenge => FfiAuthPhase::AwaitingChallenge,
        AuthDiagnosticsPhase::AwaitingPolicy => FfiAuthPhase::AwaitingPolicy,
        AuthDiagnosticsPhase::AwaitingSignature => FfiAuthPhase::AwaitingSignature,
        AuthDiagnosticsPhase::AwaitingSend | AuthDiagnosticsPhase::AwaitingRelayAck => {
            FfiAuthPhase::AwaitingRelayAck
        }
        AuthDiagnosticsPhase::Ready => FfiAuthPhase::Ready,
        AuthDiagnosticsPhase::Denied => FfiAuthPhase::Denied,
        AuthDiagnosticsPhase::Error => FfiAuthPhase::Error,
    }
}

fn auth_diagnostics_to_ffi(snapshot: AuthDiagnosticsSnapshot) -> FfiAuthDiagnostics {
    FfiAuthDiagnostics {
        relay: snapshot.relay.to_string(),
        access: access_context_to_ffi(snapshot.access),
        transport_generation: snapshot.transport_generation,
        epoch_sequence: snapshot.epoch_sequence,
        challenge_descriptor: snapshot.challenge_hash,
        phase: auth_diagnostics_phase_to_ffi(snapshot.phase),
        policy_bound: snapshot.policy_bound,
        signer_bound: snapshot.signer_bound,
        auth_event_id: snapshot.auth_event_id.map(|id| id.to_hex()),
        send_handoff_accepted: snapshot.send_handoff_accepted,
        relay_ok_accepted: snapshot.relay_ok_accepted,
    }
}

/// `nmp::DiagnosticsSnapshot -> FfiDiagnosticsSnapshot` (M5 plan §1.2 step
/// 5) -- the engine-global diagnostics projection, rendered whole for the
/// FFI boundary. Every number/string here is copied straight off the
/// engine-owned snapshot, never recomputed/estimated at this layer.
pub fn diagnostics_snapshot_to_ffi(s: DiagnosticsSnapshot) -> FfiDiagnosticsSnapshot {
    FfiDiagnosticsSnapshot {
        relays: s.relays.into_iter().map(relay_diagnostics_to_ffi).collect(),
        auth_sessions: s
            .auth_sessions
            .into_iter()
            .map(auth_diagnostics_to_ffi)
            .collect(),
        uncovered_author_count: s.uncovered_author_count as u32,
        dropped_merge_rules: s
            .dropped_merge_rules
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
        discovered_private_relays_rejected: s.discovered_private_relays_rejected,
        sessions_rejected_over_cap: s.sessions_rejected_over_cap,
        transport_degraded: s.transport_degraded,
    }
}

/// Newtype wrapper so `write_status_to_ffi` can take `&WriteStatus` without
/// this crate needing a `From<&WriteStatus>` orphan impl.
pub struct WriteStatusRef<'a>(pub &'a GWriteStatus);

#[cfg(test)]
mod write_status_tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_write_status_variant_maps_without_terminal_rollup() {
        let relay = RelayUrl::parse("wss://status.example").unwrap();
        let event_id = EventId::from_hex(&"00".repeat(32)).unwrap();
        let pubkey = nostr::Keys::generate().public_key();
        let cases = vec![
            (GWriteStatus::Accepted, FfiWriteStatus::Accepted),
            (GWriteStatus::Cancelled, FfiWriteStatus::Cancelled),
            (
                GWriteStatus::AwaitingCapability { pubkey },
                FfiWriteStatus::AwaitingCapability {
                    pubkey: pubkey.to_hex(),
                },
            ),
            (
                GWriteStatus::Signed(event_id),
                FfiWriteStatus::Signed {
                    event_id: event_id.to_hex(),
                },
            ),
            (
                GWriteStatus::Routed(BTreeSet::from([relay.clone()])),
                FfiWriteStatus::Routed {
                    relays: vec![relay.to_string()],
                },
            ),
            (
                GWriteStatus::AwaitingRelay {
                    relay: relay.clone(),
                },
                FfiWriteStatus::AwaitingRelay {
                    relay: relay.to_string(),
                },
            ),
            (
                GWriteStatus::AwaitingAuth {
                    relay: relay.clone(),
                },
                FfiWriteStatus::AwaitingAuth {
                    relay: relay.to_string(),
                },
            ),
            (
                GWriteStatus::RetryEligible {
                    relay: relay.clone(),
                    attempt: 3,
                    eligible_at: Timestamp::from(41),
                },
                FfiWriteStatus::RetryEligible {
                    relay: relay.to_string(),
                    attempt: 3,
                    eligible_at: 41,
                },
            ),
            (
                GWriteStatus::HandoffAmbiguous {
                    relay: relay.clone(),
                    attempt: 4,
                    observed_at: Timestamp::from(42),
                },
                FfiWriteStatus::HandoffAmbiguous {
                    relay: relay.to_string(),
                    attempt: 4,
                    observed_at: 42,
                },
            ),
            (
                GWriteStatus::Sent {
                    relay: relay.clone(),
                    attempt: 5,
                    written_at: Timestamp::from(43),
                },
                FfiWriteStatus::Sent {
                    relay: relay.to_string(),
                    attempt: 5,
                    written_at: 43,
                },
            ),
            (
                GWriteStatus::Acked(relay.clone()),
                FfiWriteStatus::Acked {
                    relay: relay.to_string(),
                },
            ),
            (
                GWriteStatus::Rejected(relay.clone(), "no".into()),
                FfiWriteStatus::Rejected {
                    relay: relay.to_string(),
                    reason: "no".into(),
                },
            ),
            (
                GWriteStatus::GaveUp(relay.clone()),
                FfiWriteStatus::GaveUp {
                    relay: relay.to_string(),
                },
            ),
            (
                GWriteStatus::PersistenceBlocked(relay.clone()),
                FfiWriteStatus::PersistenceBlocked {
                    relay: relay.to_string(),
                },
            ),
            (
                GWriteStatus::RoutePersistenceBlocked(relay.clone()),
                FfiWriteStatus::RoutePersistenceBlocked {
                    relay: relay.to_string(),
                },
            ),
            (
                GWriteStatus::OutcomeUnknown(relay.clone()),
                FfiWriteStatus::OutcomeUnknown {
                    relay: relay.to_string(),
                },
            ),
            (
                GWriteStatus::ReplaceableConflict {
                    expected: Some(EventId::all_zeros()),
                    actual: None,
                },
                FfiWriteStatus::ReplaceableConflict {
                    expected: Some(EventId::all_zeros().to_hex()),
                    actual: None,
                },
            ),
            (
                GWriteStatus::Failed("failed".into()),
                FfiWriteStatus::Failed {
                    reason: "failed".into(),
                },
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(write_status_to_ffi(WriteStatusRef(&source)), expected);
        }
    }

    #[test]
    fn every_cancel_refusal_maps_without_impossible_states() {
        let id = nmp::ReceiptId(41);
        assert_eq!(
            cancel_write_error_to_ffi(CancelWriteError::UnknownReceipt { receipt_id: id }),
            FfiCancelWriteError::UnknownReceipt { receipt_id: 41 }
        );
        assert_eq!(
            cancel_write_error_to_ffi(CancelWriteError::AlreadySigned {
                receipt_id: id,
                event_id: EventId::all_zeros(),
            }),
            FfiCancelWriteError::AlreadySigned {
                receipt_id: 41,
                event_id: EventId::all_zeros().to_hex(),
            }
        );
        assert_eq!(
            cancel_write_error_to_ffi(CancelWriteError::AlreadyCompensated { receipt_id: id }),
            FfiCancelWriteError::AlreadyCompensated { receipt_id: 41 }
        );
        assert_eq!(
            cancel_write_error_to_ffi(CancelWriteError::AlreadyAbandoned { receipt_id: id }),
            FfiCancelWriteError::AlreadyAbandoned { receipt_id: 41 }
        );
        assert_eq!(
            cancel_write_error_to_ffi(CancelWriteError::PersistenceFailed {
                receipt_id: id,
                reason: "disk full".to_string(),
            }),
            FfiCancelWriteError::PersistenceFailed {
                receipt_id: 41,
                reason: "disk full".to_string(),
            }
        );
        assert_eq!(
            cancel_write_error_to_ffi(CancelWriteError::EngineClosed),
            FfiCancelWriteError::EngineClosed
        );
    }
}

pub fn parse_pubkey(hex: &str) -> Result<PublicKey, FfiError> {
    PublicKey::from_hex(hex).map_err(|_| FfiError::InvalidPublicKey {
        got: hex.to_string(),
    })
}

/// #47 Unit A: `FfiWriteIntent.identity_override`'s dedicated parse. Hex
/// goes through the module-wide [`parse_pubkey`] rule verbatim; bech32
/// `npub` is ADDITIONALLY accepted for this one field (an identity is the
/// pubkey an app most plausibly holds in display form -- filter/recipient
/// inputs stay hex-only). Either way a malformed string is the same typed
/// [`FfiError::InvalidPublicKey`] naming the offending input, synchronously,
/// BEFORE any engine call. The override MISMATCH check (`Some` != the
/// payload author) deliberately does NOT live here: like `Signed`'s verify
/// (see `signed_event_from_ffi`'s doc), it runs at the engine's acceptance
/// boundary so the guarantee holds for every entry point, surfacing as
/// `WriteStatus::Failed` on the receipt stream with no `Accepted` before it.
fn parse_identity_override(input: &str) -> Result<PublicKey, FfiError> {
    use nostr::nips::nip19::FromBech32;
    parse_pubkey(input).or_else(|err| PublicKey::from_bech32(input).map_err(|_| err))
}

/// #591: `FfiWriteIntent.correlation`'s dedicated parse. Delegates entirely
/// to `nmp_grammar::CorrelationToken`'s `TryFrom<&str>` bounded/non-empty
/// validation; a rejection becomes a typed, synchronous
/// [`FfiError::InvalidCorrelationToken`] naming both the offending input and
/// the reason, BEFORE any engine call.
fn parse_correlation_token(input: &str) -> Result<nmp_grammar::CorrelationToken, FfiError> {
    nmp_grammar::CorrelationToken::try_from(input).map_err(|err| {
        FfiError::InvalidCorrelationToken {
            got: input.to_string(),
            reason: err.to_string(),
        }
    })
}

pub fn parse_relay_url(url: &str) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(url).map_err(|_| FfiError::InvalidRelayUrl {
        got: url.to_string(),
    })
}

/// A malformed raw tag array (empty, or otherwise unparseable) REJECTS the
/// whole intent rather than being silently dropped: a signer that drops one
/// tag from a template can sign a DIFFERENT event than the app composed
/// (e.g. a reply losing its `e` tag becomes a root note) -- exactly the
/// tag-integrity hole `filter_map(...).ok()` used to open. Every tag either
/// parses or the whole `write_intent_from_ffi` call fails closed with a
/// typed [`FfiError::InvalidTag`] naming the offending raw tag.
fn tags_from_ffi(tags: Vec<Vec<String>>) -> Result<Vec<Tag>, FfiError> {
    tags.into_iter()
        .map(|t| Tag::parse(t.clone()).map_err(|_| FfiError::InvalidTag { got: t }))
        .collect()
}

/// A `FfiWritePayload::Signed`'s fields -> a `nostr::Event`, PARSE ONLY --
/// every field is parsed with the same typed-error discipline as the rest
/// of this module (malformed hex/signature-shape input is still a typed
/// [`FfiError`], never a panic), but the reconstructed event is no longer
/// run through `Event::verify` here (#52 Unit B). That verify moved to
/// `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary (Unit
/// A0/#56) so the guarantee holds for every entry point, not only the one
/// that happens to verify locally -- a non-verifying (e.g. tampered) event
/// still parses fine at THIS boundary and is rejected downstream instead,
/// surfacing as `WriteStatus::Failed` on the receipt stream.
fn signed_event_from_ffi(
    id: String,
    pubkey: String,
    created_at: u64,
    kind: u16,
    tags: Vec<Vec<String>>,
    content: String,
    sig: String,
) -> Result<SignedEvent, FfiError> {
    let event_id = EventId::from_hex(&id).map_err(|_| FfiError::InvalidEventId { got: id })?;
    let public_key = parse_pubkey(&pubkey)?;
    let parsed_tags = tags_from_ffi(tags)?;
    let signature = sig
        .parse::<Signature>()
        .map_err(|_| FfiError::InvalidSignature { got: sig })?;

    Ok(SignedEvent::new(
        event_id,
        public_key,
        Timestamp::from(created_at),
        nostr::Kind::from(kind),
        parsed_tags,
        content,
        signature,
    ))
}

/// `FfiWriteIntent -> nmp::WriteIntent`. `Unsigned` builds an
/// `UnsignedEvent` template the engine signs internally; `Signed` (#32)
/// parses the caller-supplied event's fields and passes it through
/// verbatim -- see `signed_event_from_ffi`'s doc for where the verify now
/// happens. `identity_override` (#47 Unit A) parses first, so a malformed
/// override is a typed synchronous refusal before anything else is even
/// looked at -- see `parse_identity_override`'s doc for the parse/mismatch
/// boundary split.
pub fn write_intent_from_ffi(intent: FfiWriteIntent) -> Result<GWriteIntent, FfiError> {
    let identity_override = intent
        .identity_override
        .as_deref()
        .map(parse_identity_override)
        .transpose()?;
    let correlation = intent
        .correlation
        .as_deref()
        .map(parse_correlation_token)
        .transpose()?;

    let payload = match intent.payload {
        FfiWritePayload::Unsigned {
            pubkey,
            created_at,
            kind,
            tags,
            content,
        } => {
            let pubkey = parse_pubkey(&pubkey)?;
            let unsigned = UnsignedEvent::new(
                pubkey,
                Timestamp::from(created_at),
                nostr::Kind::from(kind),
                tags_from_ffi(tags)?,
                content,
            );
            GWritePayload::Unsigned(unsigned)
        }
        FfiWritePayload::Signed {
            id,
            pubkey,
            created_at,
            kind,
            tags,
            content,
            sig,
        } => {
            let event = signed_event_from_ffi(id, pubkey, created_at, kind, tags, content, sig)?;
            GWritePayload::Signed(event)
        }
    };

    let durability = match intent.durability {
        FfiDurability::Durable => GDurability::Durable,
        FfiDurability::Ephemeral => GDurability::Ephemeral,
        FfiDurability::AtMostOnce => GDurability::AtMostOnce,
    };

    // NOTE: there is deliberately no `FfiWriteRouting::PrivateNarrow` arm
    // here -- see that (deleted) variant's removal note in `types.rs`. A
    // `WriteRouting::PrivateNarrow` intent is still constructible from
    // direct Rust (`nmp::WriteRouting::PrivateNarrow`), just not from raw
    // FFI-supplied relay-URL strings. #115: `WriteRouting::PinnedHost`
    // gets the identical treatment -- `FfiWriteRouting` has no matching
    // variant at all, so this `match` staying exhaustive over exactly
    // `{AuthorOutbox, ToInboxes}` IS the enforcement; an app can only reach
    // a pinned-host write transitively through `NmpEngine::group_message_intent`'s
    // opaque `FfiComposedWriteIntent`, never through this conversion path.
    let routing = match intent.routing {
        FfiWriteRouting::AuthorOutbox => GWriteRouting::AuthorOutbox,
        FfiWriteRouting::ToInboxes { recipients } => {
            let pks = recipients
                .iter()
                .map(|hex| parse_pubkey(hex))
                .collect::<Result<Vec<_>, _>>()?;
            GWriteRouting::ToInboxes(pks)
        }
    };

    Ok(GWriteIntent {
        payload,
        durability,
        routing,
        identity_override,
        correlation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FfiIdentityField;

    fn pk_hex() -> String {
        "a".repeat(64)
    }

    #[test]
    fn acquisition_evidence_projects_every_fact_without_a_rollup() {
        let atom = nmp_grammar::ConcreteFilter {
            kinds: Some(std::collections::BTreeSet::from([9999])),
            authors: Some(std::collections::BTreeSet::from([pk_hex()])),
            ..nmp_grammar::ConcreteFilter::default()
        };
        let statuses = [
            SourceStatus::Requesting,
            SourceStatus::Connecting,
            SourceStatus::Disconnected,
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingChallenge,
            },
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingPolicy,
            },
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingSignature,
            },
            SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingRelayAck,
            },
            SourceStatus::AuthDenied,
            SourceStatus::Error,
        ];
        let sources = statuses
            .into_iter()
            .enumerate()
            .map(|(index, status)| SourceEvidence {
                relay: RelayUrl::parse(&format!("wss://source-{index}.example.com")).unwrap(),
                access: GAccessContext::Public,
                reconciled_through: (index % 2 == 0).then(|| Timestamp::from(index as u64 + 10)),
                status,
            })
            .collect();
        let ffi = evidence_to_ffi(AcquisitionEvidence {
            sources,
            shortfall: vec![
                ShortfallFact::NoPlannedSource { atom: atom.clone() },
                ShortfallFact::NoResolvedDemand,
                ShortfallFact::LocalLimit { atom: atom.clone() },
            ],
        });

        assert_eq!(ffi.sources.len(), 9);
        assert_eq!(ffi.sources[0].status, FfiSourceStatus::Requesting);
        assert_eq!(ffi.sources[0].reconciled_through, Some(10));
        assert_eq!(ffi.sources[1].status, FfiSourceStatus::Connecting);
        assert_eq!(ffi.sources[1].reconciled_through, None);
        assert_eq!(ffi.sources[2].status, FfiSourceStatus::Disconnected);
        assert_eq!(
            ffi.sources[3].status,
            FfiSourceStatus::AwaitingAuth {
                phase: FfiAuthPhase::AwaitingChallenge
            }
        );
        assert_eq!(
            ffi.sources[4].status,
            FfiSourceStatus::AwaitingAuth {
                phase: FfiAuthPhase::AwaitingPolicy
            }
        );
        assert_eq!(
            ffi.sources[5].status,
            FfiSourceStatus::AwaitingAuth {
                phase: FfiAuthPhase::AwaitingSignature
            }
        );
        assert_eq!(
            ffi.sources[6].status,
            FfiSourceStatus::AwaitingAuth {
                phase: FfiAuthPhase::AwaitingRelayAck
            }
        );
        assert_eq!(ffi.sources[7].status, FfiSourceStatus::AuthDenied);
        assert_eq!(ffi.sources[8].status, FfiSourceStatus::Error);

        let atom_json = atom.to_nostr().as_json();
        assert_eq!(
            ffi.shortfall,
            vec![
                FfiShortfallFact::NoPlannedSource {
                    atom: atom_json.clone()
                },
                FfiShortfallFact::NoResolvedDemand,
                FfiShortfallFact::LocalLimit { atom: atom_json },
            ]
        );
    }

    #[test]
    fn diagnostics_keeps_exact_intervals_distinct_from_query_evidence() {
        let relay = RelayUrl::parse("wss://diagnostics.example.com").unwrap();
        let public_key = PublicKey::from_hex(&pk_hex()).unwrap();
        let event_id = EventId::from_hex(&"b".repeat(64)).unwrap();
        let auth_phases = [
            AuthDiagnosticsPhase::AwaitingChallenge,
            AuthDiagnosticsPhase::AwaitingPolicy,
            AuthDiagnosticsPhase::AwaitingSignature,
            AuthDiagnosticsPhase::AwaitingSend,
            AuthDiagnosticsPhase::AwaitingRelayAck,
            AuthDiagnosticsPhase::Ready,
            AuthDiagnosticsPhase::Denied,
            AuthDiagnosticsPhase::Error,
        ];
        let auth_sessions = auth_phases
            .into_iter()
            .enumerate()
            .map(|(index, phase)| AuthDiagnosticsSnapshot {
                relay: relay.clone(),
                access: GAccessContext::Nip42(public_key),
                transport_slot: 900 + index as u32,
                transport_generation: 40 + index as u64,
                epoch_sequence: Some(80 + index as u64),
                challenge_hash: Some(format!("challenge-descriptor-{index}")),
                phase,
                policy_bound: index >= 2,
                signer_bound: index >= 3,
                auth_event_id: (index >= 3).then_some(event_id),
                send_handoff_accepted: index >= 4,
                relay_ok_accepted: index == 5,
            })
            .collect();
        let ffi = diagnostics_snapshot_to_ffi(DiagnosticsSnapshot {
            auth_sessions,
            relays: vec![RelayDiagnosticsSnapshot {
                relay: relay.clone(),
                access: GAccessContext::Public,
                wire_sub_count: 2,
                authors_served: 1,
                by_lane: vec![(Lane::AppRelay, 2)],
                filters: vec!["{\"kinds\":[9999]}".to_string()],
                events_by_kind: vec![(9999, 3)],
                coverage: vec![
                    FilterCoverageEntry {
                        filter: "proven".to_string(),
                        coverage: Some(CoverageInterval {
                            from: Timestamp::from(4),
                            through: Timestamp::from(9),
                        }),
                    },
                    FilterCoverageEntry {
                        filter: "unproven".to_string(),
                        coverage: None,
                    },
                ],
                nip11_supported_nips: Some(vec![11, 77]),
                nip11_document_revision: Some("revision".to_string()),
                nip11_freshness: Some("fresh"),
                nip11_last_error: None,
                nip77_advertisement: "advertised_supported",
                nip77_behavior: "behaviorally_proven",
                nip77_handoff: "reconciling",
            }],
            uncovered_author_count: 7,
            dropped_merge_rules: vec!["limit"],
            discovered_private_relays_rejected: 0,
            sessions_rejected_over_cap: 0,
            store_degraded: None,
            transport_degraded: Some("signature verification worker unavailable".to_string()),
        });

        assert_eq!(ffi.relays[0].relay, relay.to_string());
        assert_eq!(
            ffi.relays[0].coverage[0].coverage,
            Some(FfiCoverageInterval {
                from: 4,
                through: 9
            })
        );
        assert_eq!(ffi.relays[0].coverage[1].coverage, None);
        assert_eq!(ffi.relays[0].nip77_handoff, "reconciling");
        assert_eq!(
            ffi.auth_sessions
                .iter()
                .map(|session| session.phase)
                .collect::<Vec<_>>(),
            vec![
                FfiAuthPhase::AwaitingChallenge,
                FfiAuthPhase::AwaitingPolicy,
                FfiAuthPhase::AwaitingSignature,
                FfiAuthPhase::AwaitingRelayAck,
                FfiAuthPhase::AwaitingRelayAck,
                FfiAuthPhase::Ready,
                FfiAuthPhase::Denied,
                FfiAuthPhase::Error,
            ]
        );
        assert_eq!(ffi.auth_sessions[0].relay, relay.to_string());
        assert_eq!(
            ffi.auth_sessions[0].access,
            FfiAccessContext::Nip42 {
                public_key: pk_hex()
            }
        );
        assert_eq!(ffi.auth_sessions[0].transport_generation, 40);
        assert_eq!(ffi.auth_sessions[0].epoch_sequence, Some(80));
        assert_eq!(
            ffi.auth_sessions[0].challenge_descriptor.as_deref(),
            Some("challenge-descriptor-0")
        );
        assert!(!ffi.auth_sessions[0].policy_bound);
        assert!(!ffi.auth_sessions[0].signer_bound);
        assert_eq!(ffi.auth_sessions[3].auth_event_id, Some(event_id.to_hex()));
        assert!(ffi.auth_sessions[3].policy_bound);
        assert!(ffi.auth_sessions[3].signer_bound);
        assert!(!ffi.auth_sessions[3].send_handoff_accepted);
        assert!(ffi.auth_sessions[4].send_handoff_accepted);
        assert!(!ffi.auth_sessions[4].relay_ok_accepted);
        assert!(ffi.auth_sessions[5].relay_ok_accepted);
        assert_eq!(
            ffi.transport_degraded.as_deref(),
            Some("signature verification worker unavailable")
        );
    }

    #[test]
    fn literal_binding_round_trips() {
        let ffi = FfiFilter {
            kinds: Some(vec![1]),
            authors: Some(FfiBinding::Literal {
                values: vec![pk_hex()],
            }),
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("valid filter");
        let back = filter_to_ffi(grammar);
        assert_eq!(ffi, back);
    }

    #[test]
    fn reactive_and_tag_binding_round_trips() {
        let mut tags = HashMap::new();
        tags.insert(
            "p".to_string(),
            FfiBinding::Reactive {
                field: FfiIdentityField::ActivePubkey,
            },
        );
        let ffi = FfiFilter {
            kinds: Some(vec![1]),
            tags,
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("valid filter");
        let back = filter_to_ffi(grammar);
        assert_eq!(ffi, back);
    }

    #[test]
    fn nip29_h_tag_binding_round_trips() {
        let mut tags = HashMap::new();
        tags.insert(
            "h".to_string(),
            FfiBinding::Literal {
                values: vec!["group-id".to_string()],
            },
        );
        let ffi = FfiFilter {
            kinds: Some(vec![9, 30_315]),
            tags,
            ..FfiFilter::default()
        };

        let grammar = filter_from_ffi(ffi.clone()).expect("h is a valid ASCII-letter tag key");
        assert_eq!(filter_to_ffi(grammar), ffi);
    }

    #[test]
    fn derived_and_set_op_round_trip() {
        let derived = FfiBinding::Derived {
            derived: std::sync::Arc::new(FfiDerived {
                inner: FfiFilter {
                    kinds: Some(vec![3]),
                    authors: Some(FfiBinding::Reactive {
                        field: FfiIdentityField::ActivePubkey,
                    }),
                    ..FfiFilter::default()
                },
                project: FfiSelector::Tag {
                    name: "p".to_string(),
                },
            }),
        };
        let mutes = FfiBinding::Derived {
            derived: std::sync::Arc::new(FfiDerived {
                inner: FfiFilter {
                    kinds: Some(vec![10_000]),
                    authors: Some(FfiBinding::Reactive {
                        field: FfiIdentityField::ActivePubkey,
                    }),
                    ..FfiFilter::default()
                },
                project: FfiSelector::Tag {
                    name: "p".to_string(),
                },
            }),
        };
        let ffi = FfiFilter {
            kinds: Some(vec![1]),
            authors: Some(FfiBinding::SetOp {
                set_op: std::sync::Arc::new(FfiSetOp {
                    op: FfiSetAlgebra::Diff,
                    operands: vec![derived, mutes],
                }),
            }),
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("valid filter");
        let back = filter_to_ffi(grammar);
        assert_eq!(ffi, back);
    }

    #[test]
    fn multi_character_filter_tag_key_is_a_typed_non_indexable_error_not_a_panic() {
        let mut tags = HashMap::new();
        tags.insert(
            "zz".to_string(),
            FfiBinding::Literal {
                values: vec![pk_hex()],
            },
        );
        let ffi = FfiFilter {
            tags,
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::NonIndexableFilterTag {
                got: "zz".to_string()
            })
        );
    }

    /// Every ASCII letter, both cases, is a valid `FfiFilter.tags` key --
    /// structural, not a hand-picked subset. `x`/`Z` in particular are NOT
    /// in the old hard-coded M1 whitelist; round-tripping them here proves
    /// the fix is syntax-based, not another expanded list (#64 acceptance
    /// evidence).
    #[test]
    fn every_ascii_letter_is_a_valid_filter_tag_key_round_trip() {
        for c in ('a'..='z').chain('A'..='Z') {
            let mut tags = HashMap::new();
            tags.insert(
                c.to_string(),
                FfiBinding::Literal {
                    values: vec!["v".to_string()],
                },
            );
            let ffi = FfiFilter {
                tags,
                ..FfiFilter::default()
            };
            let grammar = filter_from_ffi(ffi.clone())
                .unwrap_or_else(|e| panic!("{c:?} must be a valid filter tag key: {e}"));
            assert_eq!(filter_to_ffi(grammar), ffi);
        }
    }

    /// `FfiSelector::Tag`'s `name` is an arbitrary event-tag key, never
    /// checked against the indexed-filter single-letter rule: `"-"`,
    /// `"poop"`, and `"alt"` must round-trip unchanged, not be rejected as
    /// "unknown" (#64 acceptance evidence).
    #[test]
    fn selector_tag_accepts_arbitrary_event_tag_names_unchecked() {
        for name in ["-", "poop", "alt"] {
            let ffi = FfiSelector::Tag {
                name: name.to_string(),
            };
            let grammar = selector_from_ffi(ffi.clone())
                .unwrap_or_else(|e| panic!("{name:?} must be a valid Selector::Tag key: {e}"));
            assert_eq!(grammar, GSelector::Tag(name.to_string()));
            assert_eq!(selector_to_ffi(grammar), ffi);
        }
    }

    /// The core regression test for the panic-turned-typed-error: a
    /// `Literal` value in the `authors` field position that is NOT valid
    /// hex used to sail through `binding_from_ffi` unchecked and only blow
    /// up later, as a PANIC, inside `ConcreteFilter::to_nostr` (nmp-grammar)
    /// -- two crates downstream of the actual bad input, and un-catchable
    /// by the caller. It must now fail AT THIS BOUNDARY with a typed error.
    #[test]
    fn invalid_literal_author_hex_is_a_typed_error_not_a_panic() {
        let ffi = FfiFilter {
            authors: Some(FfiBinding::Literal {
                values: vec!["not-valid-hex".to_string()],
            }),
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::InvalidPublicKey {
                got: "not-valid-hex".to_string()
            })
        );
    }

    /// Same invariant, `ids` field position (a distinct hex-decoding path
    /// in `ConcreteFilter::to_nostr` -- `EventId::from_hex`, not
    /// `PublicKey::from_hex` -- so it gets its own falsifier).
    #[test]
    fn invalid_literal_id_hex_is_a_typed_error_not_a_panic() {
        let ffi = FfiFilter {
            ids: Some(FfiBinding::Literal {
                values: vec!["also-not-hex".to_string()],
            }),
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::InvalidEventId {
                got: "also-not-hex".to_string()
            })
        );
    }

    /// A `Literal` nested inside a `SetOp` at the `authors` position must
    /// still be validated -- the field position propagates through
    /// `SetOp`'s operands, it isn't lost the moment a binding gets
    /// composite.
    #[test]
    fn invalid_literal_inside_set_op_authors_operand_is_a_typed_error() {
        let ffi = FfiFilter {
            authors: Some(FfiBinding::SetOp {
                set_op: std::sync::Arc::new(FfiSetOp {
                    op: FfiSetAlgebra::Union,
                    operands: vec![FfiBinding::Literal {
                        values: vec!["garbage".to_string()],
                    }],
                }),
            }),
            ..FfiFilter::default()
        };
        assert_eq!(
            filter_from_ffi(ffi),
            Err(FfiError::InvalidPublicKey {
                got: "garbage".to_string()
            })
        );
    }

    /// Tag VALUES (as opposed to the tag NAME/key) carry no hex invariant
    /// downstream (`ConcreteFilter::to_nostr` never parses a tag value as
    /// hex) -- a non-hex `Literal` at a tag position must still round-trip,
    /// not be rejected by the new authors/ids validation.
    #[test]
    fn non_hex_literal_tag_value_is_still_accepted() {
        let mut tags = HashMap::new();
        tags.insert(
            "d".to_string(),
            FfiBinding::Literal {
                values: vec!["my-identifier-not-hex".to_string()],
            },
        );
        let ffi = FfiFilter {
            tags,
            ..FfiFilter::default()
        };
        let grammar = filter_from_ffi(ffi.clone()).expect("tag values need no hex validation");
        assert_eq!(filter_to_ffi(grammar), ffi);
    }

    fn valid_write_intent() -> FfiWriteIntent {
        FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: pk_hex(),
                created_at: 100,
                kind: 1,
                tags: vec![vec!["e".to_string(), "e".repeat(64)]],
                content: "hello".to_string(),
            },
            durability: FfiDurability::Ephemeral,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        }
    }

    #[test]
    fn well_formed_write_intent_parses_ok() {
        let intent = valid_write_intent();
        let parsed = write_intent_from_ffi(intent).expect("well-formed intent must parse");
        match parsed.payload {
            GWritePayload::Unsigned(u) => assert_eq!(u.tags.len(), 1),
            GWritePayload::UnsignedReplaceableEdit { .. } => {
                panic!("the raw FFI write surface must not mint guarded replaceable edits")
            }
            GWritePayload::Signed(_) => {
                panic!("an Unsigned FfiWritePayload must build an Unsigned GWritePayload")
            }
        }
    }

    /// Arbitrary event tags survive the write boundary UNCHANGED and are
    /// never routed through indexed-key validation (#64 acceptance
    /// evidence / codex-nova review item 3): `"-"`/`"poop"`/`"alt"` are
    /// multi-character/punctuation tag NAMES that would fail
    /// `indexed_tag_name_from_ffi` (they are not filter keys at all here),
    /// yet `write_intent_from_ffi` must accept them verbatim -- raw tag
    /// arrays go through `tags_from_ffi`/`Tag::parse`, never
    /// `indexed_tag_name_from_ffi`.
    #[test]
    fn arbitrary_event_tags_survive_write_intent_from_ffi_unchanged() {
        let mut intent = valid_write_intent();
        let FfiWritePayload::Unsigned { tags, .. } = &mut intent.payload else {
            unreachable!("valid_write_intent always builds Unsigned")
        };
        *tags = vec![
            vec!["-".to_string()],
            vec!["poop".to_string(), "value".to_string()],
            vec!["alt".to_string(), "a human-readable summary".to_string()],
        ];
        let expected = tags.clone();

        let parsed = write_intent_from_ffi(intent)
            .expect("multi-character/punctuation event-tag names must not be rejected");
        let GWritePayload::Unsigned(unsigned) = parsed.payload else {
            unreachable!("valid_write_intent always builds Unsigned")
        };
        let round_tripped: Vec<Vec<String>> =
            unsigned.tags.iter().map(|t| t.clone().to_vec()).collect();
        assert_eq!(
            round_tripped, expected,
            "raw tag arrays must survive write_intent_from_ffi byte-for-byte, \
             never normalized/rejected as unknown"
        );
    }

    /// The tag-integrity regression test: a malformed raw tag (here, an
    /// empty array -- `Tag::parse` rejects it) used to be silently DROPPED
    /// by `tags_from_ffi`'s `filter_map(...).ok()`, so the signed event
    /// would differ from what the app composed (e.g. a reply silently
    /// losing its `e` tag and becoming a root note). The whole intent must
    /// now fail closed with a typed error instead.
    #[test]
    fn malformed_tag_rejects_whole_write_intent_not_silently_dropped() {
        let mut intent = valid_write_intent();
        let FfiWritePayload::Unsigned { tags, .. } = &mut intent.payload else {
            unreachable!("valid_write_intent always builds Unsigned")
        };
        tags.push(Vec::new()); // empty tag array: Tag::parse rejects this
        match write_intent_from_ffi(intent) {
            Err(err) => assert_eq!(err, FfiError::InvalidTag { got: Vec::new() }),
            Ok(_) => panic!("a malformed tag must fail closed, not silently drop"),
        }
    }

    /// #47 Unit A round-trip, hex form: a well-formed hex override lands in
    /// `nmp::WriteIntent::identity_override` as the parsed `PublicKey`,
    /// never dropped or rewritten. The override here names the payload
    /// author, exactly the contract the acceptance boundary later enforces.
    #[test]
    fn identity_override_hex_round_trips_to_the_parsed_pubkey() {
        let mut intent = valid_write_intent();
        intent.identity_override = Some(pk_hex());
        let parsed = write_intent_from_ffi(intent).expect("a hex override must parse");
        assert_eq!(
            parsed.identity_override,
            Some(PublicKey::from_hex(&pk_hex()).expect("fixture hex is a valid pubkey")),
            "a hex identity_override must cross the boundary as the exact parsed key"
        );
    }

    /// #47 Unit A round-trip, bech32 form: an `npub` override decodes to the
    /// IDENTICAL `PublicKey` its hex rendering does (fixture pair borrowed
    /// from `entity::tests`) -- display-form input, not a second identity.
    #[test]
    fn identity_override_npub_round_trips_to_the_same_pubkey_as_its_hex() {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
        let hex = "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4";
        let mut intent = valid_write_intent();
        let FfiWritePayload::Unsigned { pubkey, .. } = &mut intent.payload else {
            unreachable!("valid_write_intent always builds Unsigned")
        };
        *pubkey = hex.to_string();
        intent.identity_override = Some(npub.to_string());
        let parsed = write_intent_from_ffi(intent).expect("an npub override must parse");
        assert_eq!(
            parsed.identity_override,
            Some(PublicKey::from_hex(hex).expect("fixture hex is a valid pubkey")),
            "an npub identity_override must decode to the same key as its hex form"
        );
    }

    /// #47 Unit A default: an absent override converts to `None` -- the
    /// active-account default -- so every pre-#47 caller's intent is
    /// byte-identical in meaning to what it was before the field existed.
    #[test]
    fn absent_identity_override_converts_to_none() {
        let parsed = write_intent_from_ffi(valid_write_intent())
            .expect("a well-formed intent without an override must parse");
        assert_eq!(
            parsed.identity_override, None,
            "no override supplied must mean the active-account default, nothing else"
        );
    }

    /// #47 Unit A fail-closed parse: a string that is neither 64-char hex
    /// nor a decodable `npub` rejects the WHOLE intent with the same typed
    /// error every other malformed pubkey input gets, naming the offending
    /// string -- synchronously, before any engine call, so no receipt stream
    /// ever exists for it (the well-formed-but-MISMATCHED case is the
    /// acceptance boundary's `WriteStatus::Failed`, not this error).
    #[test]
    fn malformed_identity_override_is_a_typed_error_not_a_panic() {
        for garbage in ["not-a-pubkey", "npub1notvalidbech32"] {
            let mut intent = valid_write_intent();
            intent.identity_override = Some(garbage.to_string());
            match write_intent_from_ffi(intent) {
                Err(FfiError::InvalidPublicKey { got }) => assert_eq!(got, garbage),
                Err(other) => {
                    panic!("expected InvalidPublicKey, got a different FfiError: {other:?}")
                }
                Ok(_) => panic!("a malformed override must fail closed, not parse"),
            }
        }
    }

    /// A real signed event (`EventBuilder::sign_with_keys`), rendered field-
    /// for-field into a `FfiWritePayload::Signed` the same way an app would
    /// after receiving one from an external signer / NIP-46 bunker.
    fn signed_write_intent() -> (nostr::Event, FfiWriteIntent) {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::TextNote, "presigned")
            .sign_with_keys(&keys)
            .expect("test fixture must sign cleanly");
        let intent = FfiWriteIntent {
            payload: FfiWritePayload::Signed {
                id: event.id.to_hex(),
                pubkey: event.pubkey.to_hex(),
                created_at: event.created_at.as_secs(),
                kind: event.kind.as_u16(),
                tags: event.tags.iter().map(|t| t.clone().to_vec()).collect(),
                content: event.content.clone(),
                sig: event.sig.to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        };
        (event, intent)
    }

    /// #32's core contract: a pre-signed event round-trips to the engine's
    /// `WritePayload::Signed` byte-identical -- same id, same sig -- never
    /// re-derived.
    #[test]
    fn ffi_publishes_presigned_event_verbatim() {
        let (original, intent) = signed_write_intent();
        let parsed = write_intent_from_ffi(intent).expect("a genuinely signed event must parse");
        match parsed.payload {
            GWritePayload::Signed(event) => {
                assert_eq!(event.id, original.id);
                assert_eq!(event.sig, original.sig);
                assert_eq!(event.pubkey, original.pubkey);
                assert_eq!(event.content, original.content);
            }
            GWritePayload::Unsigned(_) => {
                panic!("a Signed FfiWritePayload must build a Signed GWritePayload")
            }
            GWritePayload::UnsignedReplaceableEdit { .. } => {
                panic!("the raw FFI write surface must not mint guarded replaceable edits")
            }
        }
    }

    /// #32: the sign stage is a structural no-op for `Signed` -- there is no
    /// `UnsignedEvent` anywhere in the `Signed` arm to hand a signer, so this
    /// is falsified at the type level as much as the runtime one; this test
    /// pins the runtime half (the exact bytes handed in are the exact bytes
    /// that would reach `Effect::RequestSign` if this were mistakenly routed
    /// there -- it never is, per `on_publish`).
    #[test]
    fn ffi_presigned_never_resigned() {
        let (original, intent) = signed_write_intent();
        let parsed = write_intent_from_ffi(intent).expect("a genuinely signed event must parse");
        let GWritePayload::Signed(event) = parsed.payload else {
            panic!("a Signed FfiWritePayload must build a Signed GWritePayload")
        };
        // A re-sign would mint a fresh id/sig; verbatim pass-through keeps
        // the caller's own id/sig, which only "same as original" can prove.
        assert_eq!(event.id, original.id);
        assert_eq!(event.sig, original.sig);
    }

    /// #52 Unit B: a signature that does not verify against the claimed
    /// id/pubkey NO LONGER fails at this boundary -- every field still
    /// parses (well-formed hex/signature shape), so `write_intent_from_ffi`
    /// succeeds. The verify that used to reject this here moved to
    /// `nmp-engine::core::EngineCore::on_publish`'s acceptance boundary
    /// (Unit A0/#56); `NmpEngine::publish`'s own test
    /// (`facade::tests::ffi_tampered_signed_publish_fails_closed_on_receipt_stream`)
    /// proves the rejection still happens, just downstream and
    /// asynchronously (`WriteStatus::Failed` on the receipt stream) rather
    /// than as a synchronous `FfiError` here.
    #[test]
    fn tampered_signed_event_still_parses_verify_moved_downstream() {
        let (_original, mut intent) = signed_write_intent();
        let FfiWritePayload::Signed { content, .. } = &mut intent.payload else {
            unreachable!("signed_write_intent always builds Signed")
        };
        // Tamper with the content after signing: id/sig no longer match it,
        // but every field is still well-formed hex/signature shape.
        *content = "tampered".to_string();

        write_intent_from_ffi(intent)
            .expect("marshaling never re-derives verify; that guarantee moved downstream");
    }

    /// A `sig` that isn't even valid hex is a distinct, earlier failure mode
    /// from a well-formed-but-non-verifying signature.
    #[test]
    fn ffi_rejects_signed_event_with_unparseable_signature() {
        let (_original, mut intent) = signed_write_intent();
        let FfiWritePayload::Signed { sig, .. } = &mut intent.payload else {
            unreachable!("signed_write_intent always builds Signed")
        };
        *sig = "not-hex".to_string();

        match write_intent_from_ffi(intent) {
            Err(FfiError::InvalidSignature { got }) => assert_eq!(got, "not-hex"),
            Err(other) => panic!("expected InvalidSignature, got a different FfiError: {other:?}"),
            Ok(_) => panic!("an unparseable sig must fail closed, not parse"),
        }
    }

    fn ffi_filter_kind1_author(author_hex: &str) -> FfiFilter {
        FfiFilter {
            kinds: Some(vec![1]),
            authors: Some(FfiBinding::Literal {
                values: vec![author_hex.to_string()],
            }),
            ..FfiFilter::default()
        }
    }

    /// #107: an `FfiDemand` declaring `Pinned` relays round-trips through
    /// `demand_from_ffi`/`demand_to_ffi` with the relay set canonicalized
    /// (parsed via `RelayUrl::parse`, sorted+deduped via `BTreeSet`) and
    /// every other field preserved -- including `cache: Strict`, which
    /// `Demand::new` itself never sets (it always starts `Agnostic`; this
    /// proves the FFI boundary applies it as a second, explicit step).
    #[test]
    fn demand_round_trips_pinned_source_and_strict_cache() {
        let demand = FfiDemand {
            selection: ffi_filter_kind1_author(&pk_hex()),
            source: FfiSourceAuthority::Pinned {
                relays: vec![
                    "wss://b.example.com".to_string(),
                    "wss://a.example.com".to_string(),
                ],
            },
            access: FfiAccessContext::Public,
            cache: FfiCacheMode::Strict,
            freshness: FfiFreshness::MaxAge { seconds: 14_400 },
        };

        let g = demand_from_ffi(demand).expect("nonempty pinned relay set is legal");
        assert_eq!(g.cache, GCacheMode::Strict);
        assert_eq!(g.freshness, GFreshness::MaxAge { seconds: 14_400 });
        match &g.source {
            GSourceAuthority::Pinned(relays) => {
                // BTreeSet<RelayUrl> is canonically sorted regardless of the
                // FFI caller's own insertion order.
                let urls: Vec<String> = relays.iter().map(|r| r.to_string()).collect();
                assert_eq!(urls, vec!["wss://a.example.com", "wss://b.example.com"]);
            }
            other => panic!("expected SourceAuthority::Pinned, got {other:?}"),
        }

        let back = demand_to_ffi(g);
        assert_eq!(back.cache, FfiCacheMode::Strict);
        assert_eq!(back.freshness, FfiFreshness::MaxAge { seconds: 14_400 });
        match back.source {
            FfiSourceAuthority::Pinned { relays } => {
                assert_eq!(relays, vec!["wss://a.example.com", "wss://b.example.com"]);
            }
            other => panic!("expected FfiSourceAuthority::Pinned, got {other:?}"),
        }
    }

    /// #107 Contract: an empty pinned relay set fails closed with a typed
    /// `FfiError`, never a panic -- mirroring `Demand::new`'s own
    /// `DemandError::PinnedRequiresNonemptyRelaySet` exactly.
    #[test]
    fn demand_from_ffi_rejects_an_empty_pinned_relay_set() {
        let demand = FfiDemand {
            selection: ffi_filter_kind1_author(&pk_hex()),
            source: FfiSourceAuthority::Pinned { relays: vec![] },
            access: FfiAccessContext::Public,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        };

        match demand_from_ffi(demand) {
            Err(FfiError::EmptyPinnedRelaySet) => {}
            Err(other) => {
                panic!("expected EmptyPinnedRelaySet, got a different FfiError: {other:?}")
            }
            Ok(_) => panic!("an empty pinned relay set must fail closed, not construct"),
        }
    }

    /// An unparseable relay URL inside `FfiSourceAuthority::Pinned` is a
    /// distinct, earlier failure mode from the empty-set case -- same typed
    /// error every other relay-URL boundary in this file uses.
    #[test]
    fn demand_from_ffi_rejects_an_unparseable_pinned_relay_url() {
        let demand = FfiDemand {
            selection: ffi_filter_kind1_author(&pk_hex()),
            source: FfiSourceAuthority::Pinned {
                relays: vec!["not-a-url".to_string()],
            },
            access: FfiAccessContext::Public,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        };

        match demand_from_ffi(demand) {
            Err(FfiError::InvalidRelayUrl { got }) => assert_eq!(got, "not-a-url"),
            Err(other) => panic!("expected InvalidRelayUrl, got a different FfiError: {other:?}"),
            Ok(_) => panic!("an unparseable relay url must fail closed, not construct"),
        }
    }

    /// #107: `SourceAuthority::AuthorOutboxes` declared over an unbound-
    /// author selection is the OTHER unconstructible `Demand` combination
    /// (#106) -- must also fail closed through the FFI boundary, not just
    /// the Pinned one.
    #[test]
    fn demand_from_ffi_rejects_author_outboxes_over_an_unbound_selection() {
        let demand = FfiDemand {
            selection: FfiFilter {
                kinds: Some(vec![1]),
                ..FfiFilter::default()
            },
            source: FfiSourceAuthority::AuthorOutboxes,
            access: FfiAccessContext::Public,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        };

        match demand_from_ffi(demand) {
            Err(FfiError::AuthorOutboxesRequiresBoundAuthors) => {}
            Err(other) => panic!(
                "expected AuthorOutboxesRequiresBoundAuthors, got a different FfiError: {other:?}"
            ),
            Ok(_) => panic!("must fail closed, not construct"),
        }
    }

    #[test]
    fn demand_freshness_round_trips_all_whole_second_variants() {
        for freshness in [
            FfiFreshness::Live,
            FfiFreshness::MaxAge { seconds: 14_400 },
            FfiFreshness::CacheOnly,
        ] {
            let demand = FfiDemand {
                selection: ffi_filter_kind1_author(&pk_hex()),
                source: FfiSourceAuthority::AuthorOutboxes,
                access: FfiAccessContext::Public,
                cache: FfiCacheMode::Agnostic,
                freshness,
            };
            let back = demand_to_ffi(demand_from_ffi(demand).unwrap());
            assert_eq!(back.freshness, freshness);
        }
    }
}
#[test]
fn engine_thread_refusal_preserves_component_and_reason_across_ffi() {
    let error = FfiError::from(nmp::EngineError::ThreadUnavailable {
        component: "signature verifier".to_string(),
        reason: "Resource temporarily unavailable".to_string(),
    });
    assert_eq!(
        error,
        FfiError::ThreadUnavailable {
            component: "signature verifier".to_string(),
            reason: "Resource temporarily unavailable".to_string(),
        }
    );
}
