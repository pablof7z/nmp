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
    AccessContext as GAccessContext, AcquisitionEvidence, AuthDenialSource as GAuthDenialSource,
    AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, AuthPhase, Binding as GBinding,
    CacheMode as GCacheMode, CancelWriteError, CancelWriteOutcome, CorrelationToken,
    CoverageInterval, Demand as GDemand, DemandError as GDemandError, Derived as GDerived,
    DiagnosticsSnapshot, EventBuilder as GEventBuilder, Filter as GFilter, FilterCoverageEntry,
    Frame, Freshness as GFreshness, Identity as GIdentity, IdentityField as GIdentityField,
    IndexedTagName, Lane, NotSentReason as GNotSentReason, PublishQueueEntry as GPublishQueueEntry,
    RefuseReason as GRefuseReason, RelayDiagnosticsSnapshot, RelayState as GRelayState,
    RelayWaiting as GRelayWaiting, RemoveQueueEntryError as GRemoveQueueEntryError,
    RequestRowsError, RetryCause as GRetryCause, Row, RowDelta, Selector as GSelector,
    SetAlgebra as GSetAlgebra, SetOp as GSetOp, ShortfallFact, SigningState as GSigningState,
    SourceAuthority as GSourceAuthority, SourceEvidence, SourceStatus, StalledWrite,
    StalledWriteStage, StalledWriteTotals, Window, WindowLoad, WriteFact as GWriteStatus,
    WriteIntent as GWriteIntent, WriteOutcome as GWriteOutcome, WritePayload as GWritePayload,
    WriteRouting as GWriteRouting,
};
use nostr::secp256k1::schnorr::Signature;
use nostr::{Event as SignedEvent, EventId, JsonUtil, PublicKey, RelayUrl, Tag, Timestamp};

use crate::types::{
    FfiAccessContext, FfiAcquisitionEvidence, FfiAuthDenialSource, FfiAuthDiagnostics,
    FfiAuthPhase, FfiBinding, FfiCacheMode, FfiCancelWriteError, FfiCancelWriteOutcome,
    FfiCoverageInterval, FfiDemand, FfiDerived, FfiDiagnosticsSnapshot, FfiEventBuilder, FfiFilter,
    FfiFilterCoverage, FfiFrame, FfiFreshness, FfiIdentity, FfiIdentityField, FfiKindCount,
    FfiLaneCount, FfiLiveQuery, FfiNotSentReason, FfiPublishQueueEntry, FfiQueueRelayState,
    FfiRefuseReason, FfiRelayDiagnostics, FfiRelayInformationErrorKind, FfiRelayState,
    FfiRelayWaiting, FfiRemoveQueueEntryError, FfiRetryCause, FfiRow, FfiRowDelta, FfiSelector,
    FfiSetAlgebra, FfiSetOp, FfiShortfallFact, FfiSignEventFailure, FfiSignEventRequest,
    FfiSignedEvent, FfiSigningState, FfiSourceAuthority, FfiSourceEvidence, FfiSourceStatus,
    FfiStalledWrite, FfiStalledWriteStage, FfiStalledWriteTotals, FfiWindow, FfiWindowContents,
    FfiWindowLoad, FfiWriteFact, FfiWriteIntent, FfiWriteOutcome, FfiWritePayload, FfiWriteRouting,
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
    /// `publish` refused the call outright: either NMP could not write
    /// anything down, or the instruction could not resolve (no active
    /// account, a signature that does not verify, an explicit identity
    /// contradicting a signed payload's author, a reserved kind, an empty
    /// explicit route). Nothing durable exists and there is no queue entry
    /// to inspect.
    ///
    /// Everything else takes CUSTODY and fails in the queue where the app
    /// can see it — including a stale replaceable base, which succeeds here
    /// and arrives as `FfiWriteOutcome::Refused`.
    PublishRefused {
        reason: String,
    },
    /// `NmpEngine::new`'s `store_path` pointed at a file `RedbStore::open`
    /// could not open.
    StoreOpenFailed {
        reason: String,
    },
    /// `NmpEngine::new`'s `store_path` named a persistent store already owned
    /// by this or another process. No second database owner and no partial
    /// engine were created (#489).
    StoreAlreadyOpen {
        path: String,
    },
    /// The requested unowned persistent store could not be removed.
    StoreResetFailed {
        reason: String,
    },
    /// Destructive reset was refused because a live engine in this or any
    /// other process owns the same canonical persistent-store path.
    StoreStillOpen {
        path: String,
    },
    /// The engine could not be constructed (`NmpEngine::new`). A genuine
    /// engine-start infrastructure failure -- the OS refused an engine-owned
    /// thread, or the configured relay budget was unrepresentable. The
    /// component and safe OS reason survive Swift/Kotlin translation unchanged.
    /// Never raised by an ordinary operation (#704).
    EngineStartFailed {
        component: String,
        reason: String,
    },
    /// A windowed `observe` could not open its canonical store projection (the
    /// store degraded while establishing the history session). This is the ONLY
    /// thing it means (#704 review): a concrete store-projection failure, NEVER
    /// a worker/thread/pool/runtime-busy, task-admission, spawn-failure, or
    /// queue-full condition. A relay whose connection cannot be opened is NOT
    /// this error -- it is ordinary acquisition evidence in the observation's
    /// stream, and the observation still succeeds on its other sources.
    ObservationUnavailable {
        reason: String,
    },
    /// A second `next()`/`signed()` was awaited on a stream/handle while a
    /// previous one was still in flight. Observation streams are
    /// single-consumer (#680): await the next pull only after the previous one
    /// has resolved. No frame is lost or duplicated — the offending call is
    /// rejected, not the stream.
    ConcurrentNext,
    /// A finite FIFO fact stream retained its bounded prefix but the producer
    /// advanced beyond that live-delivery window before the app drained it.
    /// The stream disconnects loudly instead of dropping facts or growing
    /// memory. `receipt_id` is present whenever the durable receipt is known;
    /// reattach that receipt to replay persisted facts.
    FactStreamLagged {
        receipt_id: Option<u64>,
    },
    /// A paged durable receipt replay could no longer reconstruct the next
    /// page from retained evidence. The receipt identity remains known and is
    /// not collapsed into absence.
    ReceiptReplayUnavailable {
        receipt_id: u64,
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
    /// it surfaces on the `WriteFact` receipt stream as `Failed` instead.
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
    /// A windowed observe was declared over a live query that already carries
    /// an aggregate result limit (#1108) -- the window and the aggregate bound
    /// would be two competing owners of the merged row count
    /// (`nmp::EngineError::WindowAggregateResultLimit` mirror).
    WindowAggregateResultLimit,
    /// A live query was declared with no demand branches at all (#1108)
    /// (`nmp::LiveQueryError::EmptyUnion` mirror).
    EmptyQueryUnion,
    /// A live query declared an aggregate result limit of zero (#1108): a
    /// query that may never contain a row is not a bound
    /// (`nmp::LiveQueryError::AggregateResultLimitZero` mirror).
    AggregateResultLimitZero,
    /// A nested live-query branch carried its own aggregate result limit
    /// (#1108). Branches flatten into ONE canonical set, so an inner bound has
    /// no surviving scope and accepting it would silently discard it
    /// (`nmp::LiveQueryError::NestedAggregateResultLimit` mirror).
    NestedAggregateResultLimit,
    /// A live query declared more branches than the supported hard ceiling
    /// (#1108). The whole declaration is refused; no subset is installed
    /// (`nmp::LiveQueryError::TooManyQueryBranches` mirror).
    TooManyQueryBranches {
        requested: u64,
        maximum: u64,
    },
    /// NIP-11 acquisition failed before any last-good document existed. Every
    /// `nmp::RelayInformationError` variant carries here as a typed
    /// `FfiRelayInformationErrorKind` instead of collapsing to a message
    /// string (#494). (#704 removed the waiter/thread admission discriminants
    /// that once had dedicated `FfiError` shapes -- the async NIP-11 fetch has
    /// no admission refusal.)
    RelayInformationUnavailable {
        kind: FfiRelayInformationErrorKind,
    },
    /// #591: `FfiWriteIntent.correlation` was `Some` but failed
    /// `nmp_grammar::CorrelationToken`'s `TryFrom<&str>` bounded/non-empty
    /// validation (empty, or over `CorrelationToken::MAX_LEN` bytes). Synchronous,
    /// before any engine call -- same discipline as `InvalidPublicKey`/
    /// `InvalidTag` above.
    /// A composer returned a CAS-guarded replaceable edit, which has no
    /// wire form on purpose: a replaceable precondition crosses this
    /// boundary only inside a fused semantic method that owns its policy
    /// (`NmpEngine::follow`/`unfollow`), never as a payload a native caller
    /// could reassemble without the guard
    /// (`docs/internals/writes/payload-and-replaceable-edits.md` §5). This
    /// is the payload axis of #951's bug class: a projection door refuses
    /// as a VALUE instead of panicking on an exported path.
    ReplaceableEditHasNoWireForm,
    InvalidCorrelationToken {
        got: String,
        reason: String,
    },
    /// #572/#1258: an `FfiNip73` failed `nmp_nip73::Nip73`'s constructor
    /// validation (an empty `I`/`K` cell, or a `Url` that is not an
    /// absolute URL and therefore cannot be normalised).
    InvalidNip73 {
        reason: String,
    },
    /// #155: an `FfiReaction::Emoji` failed `nmp::nip25::Reaction::emoji`'s
    /// validation. Two states reach here, both of which would otherwise
    /// publish a reaction whose content says something the caller did not:
    /// the empty string, which NIP-25 reads as `+` and therefore as a LIKE,
    /// and a NIP-30 `:shortcode:`, which needs a companion `emoji` row this
    /// door does not write and would reach every reader as literal colons.
    InvalidReaction {
        reason: String,
    },
    /// #1033 `FfiRelayScope::on` was called with no host at all
    /// (`nmp::nip29::RelayScopeError::EmptyRelaySet` mirror). A group must
    /// be hosted somewhere -- there is nothing to read from, nothing to
    /// write to, and no honest evidence to report.
    EmptyRelayScope,
    /// #1033 (`nmp::nip29::GroupContextError::CallerSuppliedContext`
    /// mirror). An unsigned draft handed to `FfiGroup::publish` already
    /// carried an `h` row -- the group id retained by the scope is the only
    /// source of that row, so a caller's own is refused whether it matches
    /// this group or not.
    GroupCallerSuppliedContext,
    /// #1033 (`nmp::nip29::GroupContextError::CallerSuppliedContextConstraint`
    /// mirror). A read selection handed to `FfiGroup::read` already
    /// constrained `#h` -- the retained group id is the sole semantic
    /// source of that row.
    GroupCallerSuppliedContextConstraint,
    /// #1033 (`nmp::nip29::GroupContextError::CallerSuppliedTimeline`
    /// mirror). An unsigned draft already carried a `previous` row, which
    /// the group never mints and never accepts from a caller.
    GroupCallerSuppliedTimeline,
    /// #1281 (`nmp::nip29::GroupContextError::NoGroupNamed` mirror).
    /// `FfiRelayScope::groups` was called with no group id at all. An event
    /// with no `h` row is not in a group, so there is nothing to
    /// contextualize and no honest route to mint -- the same refusal shape
    /// [`Self::EmptyRelayScope`] makes on the relay axis.
    EmptyGroupSet,
    /// #1033/#1281 (`nmp::nip29::GroupContextError::MissingContext` mirror).
    /// A signed event handed to `FfiGroup::validate_context` carries no `h`
    /// row at all. `expected` is
    /// the whole set the door was asked for, in canonical order -- one id
    /// for an `FfiGroup`, several for an `FfiGroups`.
    GroupContextMissing {
        expected: Vec<String>,
    },
    /// #1033/#1281 (`nmp::nip29::GroupContextError::MismatchedContext`
    /// mirror). A signed event names a different SET of groups than the
    /// one validating it -- too few, too many, or the wrong
    /// ones. An event carrying a second `h` row beside the right one reports
    /// both in `found`.
    GroupContextMismatched {
        found: Vec<String>,
        expected: Vec<String>,
    },
    /// #1281 (`nmp::nip29::GroupContextError::RepeatedContext` mirror). A
    /// pre-signed event names the right groups but repeats one of them in a
    /// second `h` row, which is not a row the door would ever mint.
    GroupContextRepeated {
        repeated: Vec<String>,
    },
    /// #1245 (`nmp::nip29::GroupContextError::RecordsAreNotContextScoped`
    /// mirror). A selection handed to `FfiGroup::read` named one of NIP-29's
    /// own relay-signed group records (39000/39001/39002). Those key
    /// themselves by `d`, never by `h`, so the read would match nothing
    /// forever and an app could not tell that apart from a group with no
    /// roster. Read them through `FfiGroup::observe_records` instead.
    GroupRecordsNotContextScoped {
        kinds: Vec<u16>,
    },
    /// #1233 (`nmp::nip29::GroupObserveError::NoRecordSelected` mirror). A
    /// records observation named none of the three records, which would
    /// deliver a permanently empty snapshot.
    GroupNoRecordSelected,
    /// #1252 (`nmp::nip29::GroupPredicateError::NoKindSelected` mirror). A
    /// selection handed to `groups_whose_record_matches` named no kind. It is
    /// evaluated with NIP-29's own pin, so it would match every event the
    /// group's host holds and key the listing on their `d` rows.
    GroupIdSelectionNamesNoKind,
    /// #1252 (`nmp::nip29::GroupPredicateError::NotAGroupRecordKind` mirror).
    /// A selection handed to `groups_whose_record_matches` named a kind that
    /// is not one of NIP-29's three relay-signed group records. That leaf is
    /// evaluated AT the group's host, which is not authoritative for anything
    /// else -- the read would silently under-resolve. Ids that come from the
    /// app's OWN data go through `any_of` as a derived binding carrying its
    /// own authority.
    GroupIdSelectionNotAGroupRecordKind {
        kind: u16,
    },
}

impl From<nmp::nip29::RelayScopeError> for FfiError {
    fn from(err: nmp::nip29::RelayScopeError) -> Self {
        match err {
            nmp::nip29::RelayScopeError::EmptyRelaySet => Self::EmptyRelayScope,
        }
    }
}

impl From<nmp::nip29::GroupContextError> for FfiError {
    fn from(err: nmp::nip29::GroupContextError) -> Self {
        match err {
            nmp::nip29::GroupContextError::CallerSuppliedContext => {
                Self::GroupCallerSuppliedContext
            }
            nmp::nip29::GroupContextError::CallerSuppliedContextConstraint => {
                Self::GroupCallerSuppliedContextConstraint
            }
            nmp::nip29::GroupContextError::CallerSuppliedTimeline => {
                Self::GroupCallerSuppliedTimeline
            }
            nmp::nip29::GroupContextError::NoGroupNamed => Self::EmptyGroupSet,
            nmp::nip29::GroupContextError::MissingContext { expected } => {
                Self::GroupContextMissing {
                    expected: expected.into_iter().collect(),
                }
            }
            nmp::nip29::GroupContextError::MismatchedContext { found, expected } => {
                Self::GroupContextMismatched {
                    found: found.into_iter().collect(),
                    expected: expected.into_iter().collect(),
                }
            }
            nmp::nip29::GroupContextError::RepeatedContext { repeated } => {
                Self::GroupContextRepeated {
                    repeated: repeated.into_iter().collect(),
                }
            }
            nmp::nip29::GroupContextError::RecordsAreNotContextScoped { kinds } => {
                Self::GroupRecordsNotContextScoped {
                    kinds: kinds.into_iter().collect(),
                }
            }
        }
    }
}

impl From<nmp::nip29::GroupPredicateError> for FfiError {
    fn from(err: nmp::nip29::GroupPredicateError) -> Self {
        match err {
            nmp::nip29::GroupPredicateError::NoKindSelected => Self::GroupIdSelectionNamesNoKind,
            nmp::nip29::GroupPredicateError::NotAGroupRecordKind { kind } => {
                Self::GroupIdSelectionNotAGroupRecordKind { kind }
            }
        }
    }
}

/// Same re-dispatch discipline as `GroupReadError`: the only variant that is
/// this door's OWN is the empty record selection.
impl From<nmp::nip29::GroupObserveError> for FfiError {
    fn from(err: nmp::nip29::GroupObserveError) -> Self {
        match err {
            nmp::nip29::GroupObserveError::NoRecordSelected => Self::GroupNoRecordSelected,
            nmp::nip29::GroupObserveError::Declaration(error) => Self::from(error),
            nmp::nip29::GroupObserveError::Engine(error) => Self::from(error),
        }
    }
}

/// `GroupReadError`'s two halves both already have an `FfiError` home --
/// `Context` folds through `GroupContextError`'s own mapping above,
/// `Declaration` through the existing `LiveQueryError` mapping (#1108) --
/// so this is a plain re-dispatch, never a second error taxonomy.
impl From<nmp::nip29::GroupReadError> for FfiError {
    fn from(err: nmp::nip29::GroupReadError) -> Self {
        match err {
            nmp::nip29::GroupReadError::Context(error) => Self::from(error),
            nmp::nip29::GroupReadError::Declaration(error) => Self::from(error),
        }
    }
}

/// Same re-dispatch discipline as `GroupReadError` above: `Context` through
/// `GroupContextError`, `Engine` through the existing `nmp::EngineError`
/// mapping.
impl From<nmp::nip29::GroupPublishError> for FfiError {
    fn from(err: nmp::nip29::GroupPublishError) -> Self {
        match err {
            nmp::nip29::GroupPublishError::Context(error) => Self::from(error),
            nmp::nip29::GroupPublishError::Engine(error) => Self::from(error),
        }
    }
}

/// Exact failure returned by `NmpRowStream::request_rows`
/// (`nmp::RequestRowsError` mirror). Growth is declarative -- there is no
/// token to misuse and no generation to go stale, so the only failures left
/// are the structural one (`Unwindowed`), engine teardown, and canonical-store
/// failure while staging an advance.
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
}

/// Exact lifecycle refusal for the private row-pull ticket used by the native
/// SDK bridges (#762).
///
/// The ticket exists before Kotlin enters UniFFI's cancellable async
/// READY/complete split. It is intentionally not an app-facing observation
/// noun: Swift and Kotlin create, receive, and settle it inside their existing
/// `AsyncSequence`/`Flow` adapters.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiRowPullError {
    /// Another ticket still owns this single-consumer stream.
    ConcurrentNext,
    /// `receive()` was already started on this ticket.
    ReceiveAlreadyStarted,
    /// `commit()` ran before `receive()` reached a retained value or terminal
    /// result. The ticket and stream state are unchanged.
    NotReady,
    /// This ticket was already committed or aborted. It can never affect a
    /// later ticket.
    Finished,
    /// The stream was cancelled or dropped. A retained delta was discarded
    /// and cannot be resurrected by a late ticket operation.
    Closed,
    /// `abort()` won while the Rust receive future was still resolving.
    Aborted,
}

impl From<nmp::EngineError> for FfiError {
    fn from(err: nmp::EngineError) -> Self {
        match err {
            nmp::EngineError::PublishRefused { reason } => Self::PublishRefused { reason },
            nmp::EngineError::InvalidRelayUrl { url } => Self::InvalidRelayUrl { got: url },
            nmp::EngineError::StoreOpenFailed { reason } => Self::StoreOpenFailed { reason },
            nmp::EngineError::StoreAlreadyOpen { path } => Self::StoreAlreadyOpen { path },
            nmp::EngineError::StoreResetFailed { reason } => Self::StoreResetFailed { reason },
            nmp::EngineError::StoreStillOpen { path } => Self::StoreStillOpen { path },
            nmp::EngineError::EngineStartFailed { component, reason } => {
                Self::EngineStartFailed { component, reason }
            }
            nmp::EngineError::ObservationUnavailable { reason } => {
                Self::ObservationUnavailable { reason }
            }
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
            nmp::EngineError::EngineClosed => Self::EngineClosed,
            nmp::EngineError::WindowInitialExceedsMax { initial, max } => {
                Self::WindowInitialExceedsMax {
                    initial: initial as u64,
                    max: max as u64,
                }
            }
            nmp::EngineError::WindowSelectionHasLimit => Self::WindowSelectionHasLimit,
            nmp::EngineError::WindowAggregateResultLimit => Self::WindowAggregateResultLimit,
        }
    }
}

impl From<nmp::LiveQueryError> for FfiError {
    fn from(err: nmp::LiveQueryError) -> Self {
        match err {
            nmp::LiveQueryError::EmptyUnion => Self::EmptyQueryUnion,
            nmp::LiveQueryError::AggregateResultLimitZero => Self::AggregateResultLimitZero,
            nmp::LiveQueryError::NestedAggregateResultLimit => Self::NestedAggregateResultLimit,
            nmp::LiveQueryError::TooManyQueryBranches { requested, maximum } => {
                Self::TooManyQueryBranches {
                    requested: requested as u64,
                    maximum: maximum as u64,
                }
            }
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
            Self::ReplaceableEditHasNoWireForm => write!(
                f,
                "a replaceable edit crosses this boundary only inside the semantic method that \
                 owns its precondition, never as a payload"
            ),
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
            Self::PublishRefused { reason } => write!(f, "{reason}"),
            Self::StoreOpenFailed { reason } => write!(f, "could not open store: {reason}"),
            Self::StoreAlreadyOpen { path } => {
                write!(f, "persistent store is already open: {path}")
            }
            Self::StoreResetFailed { reason } => write!(f, "could not reset store: {reason}"),
            Self::StoreStillOpen { path } => {
                write!(f, "persistent store is still open: {path}")
            }
            Self::EngineStartFailed { component, reason } => {
                write!(f, "engine could not start ({component}): {reason}")
            }
            Self::ObservationUnavailable { reason } => {
                write!(f, "observation could not be established: {reason}")
            }
            Self::ConcurrentNext => write!(
                f,
                "a next()/signed() was awaited while a previous one was still in flight; \
                 observation streams are single-consumer"
            ),
            Self::FactStreamLagged { receipt_id } => match receipt_id {
                Some(id) => write!(
                    f,
                    "the finite live fact stream fell behind; reattach receipt {id} to replay"
                ),
                None => write!(
                    f,
                    "the finite live fact stream fell behind before a receipt was observable"
                ),
            },
            Self::ReceiptReplayUnavailable { receipt_id } => write!(
                f,
                "retained evidence for receipt {receipt_id} became unavailable during replay"
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
            Self::WindowAggregateResultLimit => write!(
                f,
                "a windowed observation must not also declare an aggregate result limit"
            ),
            Self::EmptyQueryUnion => {
                write!(f, "a live query must declare at least one demand branch")
            }
            Self::AggregateResultLimitZero => {
                write!(f, "an aggregate result limit of zero can never contain a row")
            }
            Self::NestedAggregateResultLimit => write!(
                f,
                "a nested live-query branch must not declare its own aggregate result limit"
            ),
            Self::TooManyQueryBranches { requested, maximum } => write!(
                f,
                "a live query supports at most {maximum} demand branches; {requested} were declared"
            ),
            Self::RelayInformationUnavailable { kind } => {
                write!(f, "relay information unavailable: {kind:?}")
            }
            Self::InvalidCorrelationToken { got, reason } => {
                write!(f, "invalid correlation token {got:?}: {reason}")
            }
            Self::InvalidNip73 { reason } => write!(f, "invalid NIP-73 external content id: {reason}"),
            Self::InvalidReaction { reason } => write!(f, "invalid reaction: {reason}"),
            Self::EmptyRelayScope => {
                write!(f, "a NIP-29 relay scope must name at least one host relay")
            }
            Self::GroupCallerSuppliedContext => write!(
                f,
                "the 'h' tag belongs to the group, not to the caller"
            ),
            Self::GroupCallerSuppliedContextConstraint => write!(
                f,
                "the '#h' constraint belongs to the group, not to the caller's selection"
            ),
            Self::GroupCallerSuppliedTimeline => write!(
                f,
                "the 'previous' tag belongs to the group, not to the caller, and the group \
                 never mints one"
            ),
            Self::EmptyGroupSet => write!(
                f,
                "a group write must name at least one group: an event with no 'h' row is not \
                 in a group at all"
            ),
            Self::GroupContextMissing { expected } => {
                write!(f, "pre-signed event carries no 'h' row (expected {expected:?})")
            }
            Self::GroupContextMismatched { found, expected } => write!(
                f,
                "pre-signed event names groups {found:?}, expected {expected:?}"
            ),
            Self::GroupContextRepeated { repeated } => write!(
                f,
                "pre-signed event names groups {repeated:?} in more than one 'h' row"
            ),
            Self::GroupRecordsNotContextScoped { kinds } => write!(
                f,
                "kinds {kinds:?} are NIP-29's own relay-signed group records: they key \
                 themselves by 'd', never by 'h', so no such event could ever match a \
                 group-content read -- read them through the group's records door"
            ),
            Self::GroupNoRecordSelected => f.write_str(
                "a group-records observation must select at least one of the three relay-signed \
                 records",
            ),
            Self::GroupIdSelectionNamesNoKind => f.write_str(
                "a group-record selection must name at least one of NIP-29's three relay-signed \
                 group record kinds",
            ),
            Self::GroupIdSelectionNotAGroupRecordKind { kind } => write!(
                f,
                "kind:{kind} is not one of NIP-29's three relay-signed group records; a group \
                 host is not authoritative for it"
            ),
        }
    }
}

impl From<nmp::RelayInformationRequestError> for FfiError {
    fn from(error: nmp::RelayInformationRequestError) -> Self {
        // #704: `WaiterSaturated`/`ThreadUnavailable` were deleted from the
        // NIP-11 error — the async runtime has no waiter/thread admission
        // refusal. Real acquisition failures keep their domain kinds.
        match error {
            nmp::RelayInformationRequestError::Engine(error) => error.into(),
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

impl std::fmt::Display for FfiRowPullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConcurrentNext => write!(f, "another row pull ticket is still active"),
            Self::ReceiveAlreadyStarted => {
                write!(f, "receive was already started on this row pull ticket")
            }
            Self::NotReady => write!(f, "the row pull ticket has no result to commit"),
            Self::Finished => write!(f, "the row pull ticket is already settled"),
            Self::Closed => write!(f, "the row stream is closed"),
            Self::Aborted => write!(f, "the row pull ticket was aborted"),
        }
    }
}

impl std::error::Error for FfiRowPullError {}

impl From<RequestRowsError> for FfiRequestRowsError {
    fn from(error: RequestRowsError) -> Self {
        match error {
            RequestRowsError::Unwindowed => Self::Unwindowed,
            RequestRowsError::EngineClosed => Self::EngineClosed,
            RequestRowsError::StoreUnavailable => Self::StoreUnavailable,
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

#[cfg(test)]
mod engine_error_tests {
    use super::*;

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
    fn second_store_open_refusal_remains_a_typed_ffi_error() {
        let error = FfiError::from(nmp::EngineError::StoreAlreadyOpen {
            path: "/canonical/nmp.redb".to_string(),
        });
        assert_eq!(
            error,
            FfiError::StoreAlreadyOpen {
                path: "/canonical/nmp.redb".to_string(),
            }
        );
        assert_eq!(
            error.to_string(),
            "persistent store is already open: /canonical/nmp.redb"
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
            evidence: vec![AcquisitionEvidence {
                sources: vec![],
                shortfall: vec![],
            }],
            execution: vec![],
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
            evidence: vec![AcquisitionEvidence {
                sources: vec![],
                shortfall: vec![],
            }],
            execution: vec![],
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
            // #714: nested policy is app-visible and lossless. Never
            // reconstruct it through `Demand::from_filter`: the inner query
            // owns source/access/cache/freshness independently from its
            // outer demand.
            inner: demand_from_ffi(derived.inner.clone())?,
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

/// #1033: a NIP-29 discovery predicate's `subjects` binding names PUBKEYS
/// (`member_list_includes_at`/`admin_list_includes_at`'s `#p` row) -- the
/// same hex-pubkey invariant `FfiFilter.authors` carries, not the unchecked
/// rule an arbitrary `#<letter>` tag binding gets. A caller-supplied
/// `Literal` that is not 32-byte hex is a typed [`FfiError::InvalidPublicKey`],
/// never a panic two crates downstream.
pub(crate) fn subjects_binding_from_ffi(b: FfiBinding) -> Result<GBinding, FfiError> {
    binding_from_ffi(b, LiteralField::Authors)
}

/// A binding whose literals are NIP-29 group ids -- `d` row VALUES, which
/// have no hex invariant and are validated exactly as any other tag binding
/// is. Validating them as pubkeys would reject every real group id
/// ("photographers" is not 32-byte hex), which is why this is a distinct
/// entry point rather than a reuse of the subjects one.
pub(crate) fn group_ids_binding_from_ffi(b: FfiBinding) -> Result<GBinding, FfiError> {
    binding_from_ffi(b, LiteralField::Tag)
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
                inner: demand_to_ffi(d.inner),
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
/// per-query surface every `FfiFrame` from `NmpRowStream::next` carries --
/// ratified codex-nova names, see `types.rs`'s own doc). Replaces the
/// deleted query-level collapse: every source's facts map faithfully, never
/// rolled up into a verdict.
/// `FfiLiveQuery -> nmp::LiveQuery` (#1108). Every construction refusal is a
/// typed [`FfiError`] with the same case and fields Rust produces: an empty
/// union, a zero aggregate bound, a nested aggregate bound, or more branches
/// than the supported ceiling. No partial observation is ever installed.
pub fn live_query_from_ffi(query: FfiLiveQuery) -> Result<nmp::LiveQuery, FfiError> {
    let branches = query
        .branches
        .into_iter()
        .map(|branch| demand_from_ffi(branch).map(nmp::LiveQuery::single))
        .collect::<Result<Vec<_>, _>>()?;
    let aggregate_result_limit = query.aggregate_result_limit.map(|limit| limit as usize);
    nmp::LiveQuery::union(branches, aggregate_result_limit).map_err(FfiError::from)
}

/// `nmp::LiveQuery -> FfiLiveQuery` (#1033, the reverse of
/// `live_query_from_ffi`). Canonicalization (branch sort/dedup, nested-limit
/// refusal) already happened at construction on the Rust side, so this is a
/// plain field-for-field projection with no failure mode of its own -- every
/// NIP-29 read this crate mints reaches the app through this door.
pub fn live_query_to_ffi(query: nmp::LiveQuery) -> FfiLiveQuery {
    FfiLiveQuery {
        branches: query
            .branches()
            .iter()
            .cloned()
            .map(demand_to_ffi)
            .collect(),
        aggregate_result_limit: query.aggregate_result_limit().map(|limit| limit as u32),
    }
}

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
            evidence: frame.evidence.into_iter().map(evidence_to_ffi).collect(),
        },
        None => FfiFrame {
            deltas: frame.deltas.iter().map(row_delta_to_ffi).collect(),
            window: None,
            evidence: frame.evidence.into_iter().map(evidence_to_ffi).collect(),
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

fn signing_state_to_ffi(state: &GSigningState) -> FfiSigningState {
    match state {
        GSigningState::AwaitingSigner { pubkey } => FfiSigningState::AwaitingSigner {
            pubkey: pubkey.to_hex(),
        },
        GSigningState::InFlight { pubkey } => FfiSigningState::InFlight {
            pubkey: pubkey.to_hex(),
        },
        GSigningState::Signed { event_id } => FfiSigningState::Signed {
            event_id: event_id.to_hex(),
        },
        GSigningState::Refused { reason } => FfiSigningState::Refused {
            reason: reason.clone(),
        },
    }
}

fn auth_denial_source_to_ffi(source: GAuthDenialSource) -> FfiAuthDenialSource {
    match source {
        GAuthDenialSource::Policy => FfiAuthDenialSource::Policy,
        GAuthDenialSource::Signer => FfiAuthDenialSource::Signer,
        GAuthDenialSource::Relay => FfiAuthDenialSource::Relay,
    }
}

fn retry_cause_to_ffi(cause: GRetryCause) -> FfiRetryCause {
    match cause {
        GRetryCause::Interrupted => FfiRetryCause::Interrupted,
        GRetryCause::AckTimeout => FfiRetryCause::AckTimeout,
        GRetryCause::ConnectionLost => FfiRetryCause::ConnectionLost,
        GRetryCause::RelayRateLimited => FfiRetryCause::RelayRateLimited,
        GRetryCause::RelayError => FfiRetryCause::RelayError,
    }
}

pub fn relay_state_to_ffi(state: &GRelayState) -> FfiRelayState {
    match state {
        GRelayState::Waiting(waiting) => FfiRelayState::Waiting {
            waiting: match waiting {
                GRelayWaiting::NotConnected => FfiRelayWaiting::NotConnected,
                GRelayWaiting::NeedsAuth => FfiRelayWaiting::NeedsAuth,
                GRelayWaiting::BackingOff {
                    attempt,
                    eligible_at,
                    cause,
                    detail,
                } => FfiRelayWaiting::BackingOff {
                    attempt: *attempt,
                    eligible_at: eligible_at.as_secs(),
                    cause: retry_cause_to_ffi(*cause),
                    detail: detail.clone(),
                },
                GRelayWaiting::PersistenceStalled { detail } => {
                    FfiRelayWaiting::PersistenceStalled {
                        detail: detail.clone(),
                    }
                }
            },
        },
        GRelayState::Sent {
            attempt,
            written_at,
        } => FfiRelayState::Sent {
            attempt: *attempt,
            written_at: written_at.as_secs(),
        },
        GRelayState::Published => FfiRelayState::Published,
        GRelayState::Rejected { reason } => FfiRelayState::Rejected {
            reason: reason.clone(),
        },
        GRelayState::AuthFailed {
            pubkey,
            source,
            reason,
        } => FfiRelayState::AuthFailed {
            pubkey: pubkey.to_hex(),
            source: auth_denial_source_to_ffi(*source),
            reason: reason.clone(),
        },
        GRelayState::GaveUp => FfiRelayState::GaveUp,
    }
}

fn refuse_reason_to_ffi(reason: GRefuseReason) -> FfiRefuseReason {
    match reason {
        GRefuseReason::AlreadyExpired => FfiRefuseReason::AlreadyExpired,
        GRefuseReason::Tombstoned => FfiRefuseReason::Tombstoned,
        GRefuseReason::ReplaceableBaseOnRegularEvent => {
            FfiRefuseReason::ReplaceableBaseOnRegularEvent
        }
        GRefuseReason::ReplaceableBaseChanged { expected, actual } => {
            FfiRefuseReason::ReplaceableBaseChanged {
                expected: expected.map(|id| id.to_hex()),
                actual: actual.map(|id| id.to_hex()),
            }
        }
    }
}

fn write_outcome_to_ffi(outcome: &GWriteOutcome) -> FfiWriteOutcome {
    match outcome {
        GWriteOutcome::Settled => FfiWriteOutcome::Settled,
        GWriteOutcome::NoDestination => FfiWriteOutcome::NoDestination,
        GWriteOutcome::NotSent(reason) => FfiWriteOutcome::NotSent {
            reason: match reason {
                GNotSentReason::Cancelled => FfiNotSentReason::Cancelled,
                GNotSentReason::Superseded => FfiNotSentReason::Superseded,
            },
        },
        GWriteOutcome::Refused(reason) => FfiWriteOutcome::Refused {
            reason: refuse_reason_to_ffi(*reason),
        },
    }
}

pub fn write_status_to_ffi(s: WriteStatusRef<'_>) -> FfiWriteFact {
    match s.0 {
        GWriteStatus::Signing(state) => FfiWriteFact::Signing {
            state: signing_state_to_ffi(state),
        },
        GWriteStatus::Relay { relay, state } => FfiWriteFact::Relay {
            relay: relay.to_string(),
            state: relay_state_to_ffi(state),
        },
        GWriteStatus::Destinations {
            relays,
            complete,
            awaiting_author_routes,
        } => FfiWriteFact::Destinations {
            relays: relays.iter().map(RelayUrl::to_string).collect(),
            complete: *complete,
            // Hex, never bech32: an `npub` exists to be shown to a person or
            // pasted by one, and this crosses to an app that will decide what
            // to do with the key (`conventions/bech32-boundary.md`). The
            // BTreeSet's key order survives into the vector, so two reads of
            // the same park compare equal.
            awaiting_author_routes: awaiting_author_routes
                .iter()
                .map(PublicKey::to_hex)
                .collect(),
        },
        GWriteStatus::Outcome(outcome) => FfiWriteFact::Outcome {
            outcome: write_outcome_to_ffi(outcome),
        },
    }
}

pub fn publish_queue_entry_to_ffi(entry: &GPublishQueueEntry) -> FfiPublishQueueEntry {
    FfiPublishQueueEntry {
        receipt_id: entry.receipt_id.0,
        event_id: entry.event_id.to_hex(),
        pubkey: entry.pubkey.to_hex(),
        accepted_at: entry.accepted_at.as_secs(),
        signing: signing_state_to_ffi(&entry.signing),
        relays: entry.relays.iter().map(RelayUrl::to_string).collect(),
        route_complete: entry.route_complete,
        relay_states: entry
            .relay_states
            .iter()
            .map(|(relay, state)| FfiQueueRelayState {
                relay: relay.to_string(),
                state: relay_state_to_ffi(state),
            })
            .collect(),
        outcome: entry.outcome.as_ref().map(write_outcome_to_ffi),
        persistence_fault: entry.persistence_fault.clone(),
    }
}

pub fn remove_queue_entry_error_to_ffi(error: GRemoveQueueEntryError) -> FfiRemoveQueueEntryError {
    match error {
        GRemoveQueueEntryError::UnknownReceipt { receipt_id } => {
            FfiRemoveQueueEntryError::UnknownReceipt {
                receipt_id: receipt_id.0,
            }
        }
        GRemoveQueueEntryError::StillActive { receipt_id } => {
            FfiRemoveQueueEntryError::StillActive {
                receipt_id: receipt_id.0,
            }
        }
        GRemoveQueueEntryError::PersistenceFailed { receipt_id, reason } => {
            FfiRemoveQueueEntryError::PersistenceFailed {
                receipt_id: receipt_id.0,
                reason,
            }
        }
        GRemoveQueueEntryError::EngineClosed => FfiRemoveQueueEntryError::EngineClosed,
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
        CancelWriteError::AlreadySuperseded { receipt_id } => {
            FfiCancelWriteError::AlreadySuperseded {
                receipt_id: receipt_id.0,
            }
        }
        CancelWriteError::AlreadyRefused { receipt_id } => FfiCancelWriteError::AlreadyRefused {
            receipt_id: receipt_id.0,
        },
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
        Lane::AuthorOutbound => "author_outbound",
        Lane::Hint => "hint",
        Lane::Provenance => "provenance",
        Lane::OperatorApp => "operator_app",
        Lane::OperatorFallback => "operator_fallback",
        Lane::Exact => "exact",
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

fn stalled_write_stage_to_ffi(stage: StalledWriteStage) -> FfiStalledWriteStage {
    match stage {
        StalledWriteStage::Unroutable => FfiStalledWriteStage::Unroutable,
        StalledWriteStage::Unsignable => FfiStalledWriteStage::Unsignable,
        StalledWriteStage::Undeliverable => FfiStalledWriteStage::Undeliverable,
    }
}

fn stalled_write_to_ffi(write: StalledWrite) -> FfiStalledWrite {
    FfiStalledWrite {
        id: write.id,
        stage: stalled_write_stage_to_ffi(write.stage),
        detail: write.detail,
        stalled_since: write.stalled_since.as_secs(),
    }
}

fn stalled_write_totals_to_ffi(totals: StalledWriteTotals) -> FfiStalledWriteTotals {
    FfiStalledWriteTotals {
        unroutable: totals.unroutable,
        unsignable: totals.unsignable,
        undeliverable: totals.undeliverable,
        omitted_details: totals.omitted_details,
        detail_limit: totals.detail_limit,
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
        stalled_writes: s
            .stalled_writes
            .into_iter()
            .map(stalled_write_to_ffi)
            .collect(),
        stalled_write_totals: stalled_write_totals_to_ffi(s.stalled_write_totals),
    }
}

/// Newtype wrapper so `write_status_to_ffi` can take `&WriteFact` without
/// this crate needing a `From<&WriteFact>` orphan impl.
pub struct WriteStatusRef<'a>(pub &'a GWriteStatus);

#[cfg(test)]
mod write_fact_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every arm of the write vocabulary crosses the boundary as ITSELF.
    ///
    /// The falsifier this pins is the one #1237 was filed about: an
    /// `AuthFailed` folded into `Rejected` would tell a user a relay refused
    /// them when their own client declined to ask, and a `Settled` collapsed
    /// into silence would leave an app unable to tell a finished write from
    /// a dropped subscription.
    #[test]
    fn every_write_fact_maps_without_terminal_rollup() {
        let relay = RelayUrl::parse("wss://status.example").unwrap();
        let event_id = EventId::from_hex(&"00".repeat(32)).unwrap();
        let pubkey = nostr::Keys::generate().public_key();
        let awaited = nostr::Keys::generate().public_key();
        let cases = vec![
            (
                GWriteStatus::Signing(GSigningState::AwaitingSigner { pubkey }),
                FfiWriteFact::Signing {
                    state: FfiSigningState::AwaitingSigner {
                        pubkey: pubkey.to_hex(),
                    },
                },
            ),
            (
                // #1261: a signature in flight must not arrive on the far
                // side of the boundary as a write parked on a key nobody
                // has a signer for.
                GWriteStatus::Signing(GSigningState::InFlight { pubkey }),
                FfiWriteFact::Signing {
                    state: FfiSigningState::InFlight {
                        pubkey: pubkey.to_hex(),
                    },
                },
            ),
            (
                GWriteStatus::Signing(GSigningState::Signed { event_id }),
                FfiWriteFact::Signing {
                    state: FfiSigningState::Signed {
                        event_id: event_id.to_hex(),
                    },
                },
            ),
            (
                GWriteStatus::Signing(GSigningState::Refused {
                    reason: "signer said no".into(),
                }),
                FfiWriteFact::Signing {
                    state: FfiSigningState::Refused {
                        reason: "signer said no".into(),
                    },
                },
            ),
            (
                GWriteStatus::Destinations {
                    relays: BTreeSet::from([relay.clone()]),
                    complete: false,
                    awaiting_author_routes: BTreeSet::from([awaited]),
                },
                FfiWriteFact::Destinations {
                    relays: vec![relay.to_string()],
                    complete: false,
                    awaiting_author_routes: vec![awaited.to_hex()],
                },
            ),
            (
                GWriteStatus::Destinations {
                    relays: BTreeSet::new(),
                    complete: true,
                    awaiting_author_routes: BTreeSet::new(),
                },
                FfiWriteFact::Destinations {
                    relays: Vec::new(),
                    complete: true,
                    awaiting_author_routes: Vec::new(),
                },
            ),
            (
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::Waiting(GRelayWaiting::NotConnected),
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::Waiting {
                        waiting: FfiRelayWaiting::NotConnected,
                    },
                },
            ),
            (
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::Waiting(GRelayWaiting::NeedsAuth),
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::Waiting {
                        waiting: FfiRelayWaiting::NeedsAuth,
                    },
                },
            ),
            (
                // #1032's second half: a backoff that cannot say WHY is a
                // silently reverted fix.
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::Waiting(GRelayWaiting::BackingOff {
                        attempt: 4,
                        eligible_at: Timestamp::from(99u64),
                        cause: GRetryCause::RelayRateLimited,
                        detail: Some("slow down".into()),
                    }),
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::Waiting {
                        waiting: FfiRelayWaiting::BackingOff {
                            attempt: 4,
                            eligible_at: 99,
                            cause: FfiRetryCause::RelayRateLimited,
                            detail: Some("slow down".into()),
                        },
                    },
                },
            ),
            (
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::Waiting(GRelayWaiting::PersistenceStalled {
                        detail: "disk".into(),
                    }),
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::Waiting {
                        waiting: FfiRelayWaiting::PersistenceStalled {
                            detail: "disk".into(),
                        },
                    },
                },
            ),
            (
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::Sent {
                        attempt: 40,
                        written_at: Timestamp::from(7u64),
                    },
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::Sent {
                        attempt: 40,
                        written_at: 7,
                    },
                },
            ),
            (
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::Published,
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::Published,
                },
            ),
            (
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::Rejected {
                        reason: "no".into(),
                    },
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::Rejected {
                        reason: "no".into(),
                    },
                },
            ),
            (
                // NOT `Rejected`. Folding an app's own decision not to
                // authenticate into the relay's rejection tells the user a
                // relay refused them when their own client declined to ask.
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::AuthFailed {
                        pubkey,
                        source: GAuthDenialSource::Policy,
                        reason: "policy declined".into(),
                    },
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::AuthFailed {
                        pubkey: pubkey.to_hex(),
                        source: FfiAuthDenialSource::Policy,
                        reason: "policy declined".into(),
                    },
                },
            ),
            (
                GWriteStatus::Relay {
                    relay: relay.clone(),
                    state: GRelayState::GaveUp,
                },
                FfiWriteFact::Relay {
                    relay: relay.to_string(),
                    state: FfiRelayState::GaveUp,
                },
            ),
            (
                GWriteStatus::Outcome(GWriteOutcome::Settled),
                FfiWriteFact::Outcome {
                    outcome: FfiWriteOutcome::Settled,
                },
            ),
            (
                GWriteStatus::Outcome(GWriteOutcome::NoDestination),
                FfiWriteFact::Outcome {
                    outcome: FfiWriteOutcome::NoDestination,
                },
            ),
            (
                GWriteStatus::Outcome(GWriteOutcome::NotSent(GNotSentReason::Cancelled)),
                FfiWriteFact::Outcome {
                    outcome: FfiWriteOutcome::NotSent {
                        reason: FfiNotSentReason::Cancelled,
                    },
                },
            ),
            (
                GWriteStatus::Outcome(GWriteOutcome::NotSent(GNotSentReason::Superseded)),
                FfiWriteFact::Outcome {
                    outcome: FfiWriteOutcome::NotSent {
                        reason: FfiNotSentReason::Superseded,
                    },
                },
            ),
            (
                // Both ids survive the crossing. Reduced to a string, an app
                // could only tell the user to redo the edit by hand.
                GWriteStatus::Outcome(GWriteOutcome::Refused(
                    GRefuseReason::ReplaceableBaseChanged {
                        expected: Some(event_id),
                        actual: None,
                    },
                )),
                FfiWriteFact::Outcome {
                    outcome: FfiWriteOutcome::Refused {
                        reason: FfiRefuseReason::ReplaceableBaseChanged {
                            expected: Some(event_id.to_hex()),
                            actual: None,
                        },
                    },
                },
            ),
            (
                GWriteStatus::Outcome(GWriteOutcome::Refused(GRefuseReason::Tombstoned)),
                FfiWriteFact::Outcome {
                    outcome: FfiWriteOutcome::Refused {
                        reason: FfiRefuseReason::Tombstoned,
                    },
                },
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(write_status_to_ffi(WriteStatusRef(&source)), expected);
        }
    }
}

pub fn parse_pubkey(hex: &str) -> Result<PublicKey, FfiError> {
    PublicKey::from_hex(hex).map_err(|_| FfiError::InvalidPublicKey {
        got: hex.to_string(),
    })
}

/// #1033: the module-wide event-id parse rule (`FfiGroup::delete_event`'s
/// `event_id` and any other exact 32-byte-hex `EventId` input) -- same
/// typed-refusal discipline as [`parse_pubkey`], never a panic.
pub fn parse_event_id(hex: &str) -> Result<EventId, FfiError> {
    EventId::from_hex(hex).map_err(|_| FfiError::InvalidEventId {
        got: hex.to_string(),
    })
}

/// `FfiIdentity -> nmp::Identity`. `Explicit`'s pubkey goes through the
/// module-wide [`parse_pubkey`] rule verbatim and nothing else: a bech32
/// `npub` is REFUSED here, however well-formed, because "which encodings
/// does this field take" must have one answer for every pubkey-shaped input
/// rather than one answer per field
/// (`docs/internals/conventions/bech32-boundary.md`). An app holding a
/// display form decodes it with `decode_nostr_entity` at the boundary where
/// the user pasted it. A malformed string is a typed
/// [`FfiError::InvalidPublicKey`] naming the offending input,
/// synchronously, BEFORE any engine call.
///
/// The RESTATEMENT check (`Explicit` != a signed payload's author)
/// deliberately does NOT live here: like `Signed`'s verify (see
/// `signed_event_from_ffi`'s doc), it runs at the engine's acceptance
/// boundary so the guarantee holds for every entry point, refusing the
/// `publish` CALL itself as [`FfiError::PublishRefused`] — nothing is taken
/// into custody, so no receipt and no queue entry exist for it.
fn identity_from_ffi(identity: FfiIdentity) -> Result<GIdentity, FfiError> {
    Ok(match identity {
        FfiIdentity::Active => GIdentity::Active,
        FfiIdentity::Explicit { pubkey } => GIdentity::Explicit(parse_pubkey(&pubkey)?),
    })
}

/// Project an identity a protocol module chose back out to the FFI
/// boundary -- total in both directions, so a protocol crate that changes
/// which identity it names projects that change faithfully instead of
/// tripping a closed-contract assertion on an exported path.
pub(crate) fn identity_to_ffi(identity: GIdentity) -> FfiIdentity {
    match identity {
        GIdentity::Active => FfiIdentity::Active,
        GIdentity::Explicit(pk) => FfiIdentity::Explicit {
            pubkey: pk.to_hex(),
        },
    }
}

/// #591: `FfiWriteIntent.correlation`'s dedicated parse (also used by the
/// engine-free NIP-22 composer, hence `pub(crate)`). Delegates entirely
/// to `nmp::CorrelationToken`'s `TryFrom<&str>` bounded/non-empty
/// validation; a rejection becomes a typed, synchronous
/// [`FfiError::InvalidCorrelationToken`] naming both the offending input and
/// the reason, BEFORE any engine call.
pub(crate) fn parse_correlation_token(input: &str) -> Result<CorrelationToken, FfiError> {
    CorrelationToken::try_from(input).map_err(|err| FfiError::InvalidCorrelationToken {
        got: input.to_string(),
        reason: err.to_string(),
    })
}

pub fn parse_relay_url(url: &str) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(url).map_err(|_| FfiError::InvalidRelayUrl {
        got: url.to_string(),
    })
}

/// Project a routing STRATEGY a protocol module chose back out to the FFI
/// boundary. Total in both directions, which is the point: a protocol crate
/// that changes which route it mints projects that change faithfully instead
/// of tripping a closed-contract assertion on an exported path (#951's bug
/// class, on the routing axis).
pub(crate) fn write_routing_to_ffi(routing: nmp::WriteRouting) -> FfiWriteRouting {
    match routing {
        nmp::WriteRouting::Auto => FfiWriteRouting::Auto,
        nmp::WriteRouting::Explicit(relays) => FfiWriteRouting::Explicit {
            relays: relays.iter().map(|relay| relay.to_string()).collect(),
        },
    }
}

/// Project a payload a protocol module composed back out to the FFI
/// boundary. Total over every shape that HAS a wire form, so a protocol
/// crate that changes which payload it mints projects that change
/// faithfully instead of tripping a closed-contract assertion on an
/// exported path (#951's bug class, on the payload axis). The one shape
/// with no wire form refuses as a typed value -- see
/// [`FfiError::ReplaceableEditHasNoWireForm`].
pub(crate) fn write_payload_to_ffi(payload: GWritePayload) -> Result<FfiWritePayload, FfiError> {
    match payload {
        GWritePayload::Event(builder) => Ok(FfiWritePayload::Event {
            builder: event_builder_to_ffi(builder),
        }),
        GWritePayload::Signed(event) => Ok(FfiWritePayload::Signed {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: event.content.clone(),
            sig: event.sig.to_string(),
        }),
        GWritePayload::ReplaceableEdit { .. } => Err(FfiError::ReplaceableEditHasNoWireForm),
    }
}

/// `nmp::EventBuilder -> FfiEventBuilder`. Infallible in this direction:
/// every field already is what the record carries.
pub(crate) fn event_builder_to_ffi(builder: GEventBuilder) -> FfiEventBuilder {
    FfiEventBuilder {
        kind: builder.kind.as_u16(),
        tags: builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect(),
        content: builder.content,
        created_at: builder.created_at.map(|ts| ts.as_secs()),
    }
}

/// The durability a protocol module chose, projected totally for the same
/// reason routing and payload are.
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

/// `FfiEventBuilder -> nmp::EventBuilder`. The only thing that can fail is
/// a tag row that is not a tag; there is deliberately no author to parse,
/// no kind whitelist to check, and no timestamp to invent.
pub(crate) fn event_builder_from_ffi(builder: FfiEventBuilder) -> Result<GEventBuilder, FfiError> {
    Ok(GEventBuilder {
        kind: nostr::Kind::from(builder.kind),
        tags: tags_from_ffi(builder.tags)?,
        content: builder.content,
        created_at: builder.created_at.map(Timestamp::from),
    })
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
/// refusing the `publish` call as [`FfiError::PublishRefused`].
pub(crate) fn signed_event_from_ffi(
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

/// `FfiWriteIntent -> nmp::WriteIntent`. `Event` builds the `EventBuilder`
/// the engine stamps, freezes and signs internally; `Signed` (#32)
/// parses the caller-supplied event's fields and passes it through
/// verbatim -- see `signed_event_from_ffi`'s doc for where the verify now
/// happens. `identity` parses first, so a malformed pubkey is a typed
/// synchronous refusal before anything else is even looked at -- see
/// `identity_from_ffi`'s doc for the parse/restatement boundary split.
pub fn write_intent_from_ffi(intent: FfiWriteIntent) -> Result<GWriteIntent, FfiError> {
    let identity = identity_from_ffi(intent.identity)?;
    let correlation = intent
        .correlation
        .as_deref()
        .map(parse_correlation_token)
        .transpose()?;

    let payload = match intent.payload {
        FfiWritePayload::Event { builder } => {
            GWritePayload::Event(event_builder_from_ffi(builder)?)
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

    // Both routing words project, because both are app vocabulary: an app
    // saying "publish this event to relay: [user input]" is the same
    // primitive a wiki, DM, or group crate uses, and there is no third word
    // for either of them to reach for. A malformed URL is a typed
    // synchronous refusal here, before any engine call; an EMPTY relay list
    // is refused at the engine's acceptance door (it is a routing rule, not
    // a parsing rule, so it lives in one place for every surface).
    let routing = match intent.routing {
        FfiWriteRouting::Auto => GWriteRouting::Auto,
        FfiWriteRouting::Explicit { relays } => GWriteRouting::Explicit(
            relays
                .into_iter()
                .map(|url| parse_relay_url(&url))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };

    Ok(GWriteIntent {
        payload,
        routing,
        identity,
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
                subscription_budget: Some(20),
                subscriptions_refused: 0,
                subid_length_limit: None,
                subid_length_rejects_our_ids: false,
                authors_served: 1,
                by_lane: vec![(Lane::OperatorApp, 2)],
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
            sessions_refused_by_subscription_budget: 0,
            store_degraded: None,
            transport_degraded: Some("signature verification worker unavailable".to_string()),
            stalled_writes: vec![
                StalledWrite {
                    id: "unroutable-descriptor".to_string(),
                    stage: StalledWriteStage::Unroutable,
                    detail: "no route is known yet".to_string(),
                    stalled_since: Timestamp::from(1_700_000_001u64),
                },
                StalledWrite {
                    id: "unsignable-descriptor".to_string(),
                    stage: StalledWriteStage::Unsignable,
                    detail: "no signer is registered".to_string(),
                    stalled_since: Timestamp::from(1_700_000_002u64),
                },
                StalledWrite {
                    id: "undeliverable-descriptor".to_string(),
                    stage: StalledWriteStage::Undeliverable,
                    detail: "no destination is reachable: wss://nowhere.example".to_string(),
                    stalled_since: Timestamp::from(u64::from(u32::MAX) + 1),
                },
            ],
            stalled_write_totals: StalledWriteTotals {
                unroutable: 1,
                unsignable: 2,
                undeliverable: 3,
                omitted_details: 4,
                detail_limit: u64::MAX,
            },
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
        // Every stalled-write field crosses whole, in order, with the exact
        // stage each row was built with and a `u64` instant that survives the
        // 32-bit boundary a narrower carrier would have silently truncated.
        assert_eq!(
            ffi.stalled_writes
                .iter()
                .map(|w| (w.id.as_str(), w.stage, w.detail.as_str(), w.stalled_since))
                .collect::<Vec<_>>(),
            vec![
                (
                    "unroutable-descriptor",
                    FfiStalledWriteStage::Unroutable,
                    "no route is known yet",
                    1_700_000_001u64
                ),
                (
                    "unsignable-descriptor",
                    FfiStalledWriteStage::Unsignable,
                    "no signer is registered",
                    1_700_000_002u64
                ),
                (
                    "undeliverable-descriptor",
                    FfiStalledWriteStage::Undeliverable,
                    "no destination is reachable: wss://nowhere.example",
                    u64::from(u32::MAX) + 1
                ),
            ]
        );
        assert_eq!(
            ffi.stalled_write_totals,
            FfiStalledWriteTotals {
                unroutable: 1,
                unsignable: 2,
                undeliverable: 3,
                omitted_details: 4,
                detail_limit: u64::MAX,
            }
        );
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
                inner: FfiDemand {
                    selection: FfiFilter {
                        kinds: Some(vec![3]),
                        authors: Some(FfiBinding::Reactive {
                            field: FfiIdentityField::ActivePubkey,
                        }),
                        ..FfiFilter::default()
                    },
                    source: FfiSourceAuthority::AuthorOutboxes,
                    access: FfiAccessContext::Public,
                    cache: FfiCacheMode::Agnostic,
                    freshness: FfiFreshness::Live,
                },
                project: FfiSelector::Tag {
                    name: "p".to_string(),
                },
            }),
        };
        let mutes = FfiBinding::Derived {
            derived: std::sync::Arc::new(FfiDerived {
                inner: FfiDemand {
                    selection: FfiFilter {
                        kinds: Some(vec![10_000]),
                        authors: Some(FfiBinding::Reactive {
                            field: FfiIdentityField::ActivePubkey,
                        }),
                        ..FfiFilter::default()
                    },
                    source: FfiSourceAuthority::AuthorOutboxes,
                    access: FfiAccessContext::Public,
                    cache: FfiCacheMode::Agnostic,
                    freshness: FfiFreshness::Live,
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

    /// #714: a nested `Derived.inner` is a complete `Demand`, not a
    /// filter-shaped value that silently regains defaults at the boundary.
    /// Deliberately make every inner policy differ from the outer demand so
    /// dropping any one of them fails this exact round trip.
    #[test]
    fn derived_inner_full_demand_round_trips_every_policy_independently() {
        let inner = FfiDemand {
            selection: FfiFilter {
                kinds: Some(vec![3]),
                authors: Some(FfiBinding::Reactive {
                    field: FfiIdentityField::ActivePubkey,
                }),
                ..FfiFilter::default()
            },
            source: FfiSourceAuthority::Pinned {
                relays: vec!["wss://inner.example.com".to_string()],
            },
            access: FfiAccessContext::Nip42 {
                public_key: pk_hex(),
            },
            cache: FfiCacheMode::Strict,
            freshness: FfiFreshness::MaxAge { seconds: 600 },
        };
        let mut public_inner = inner.clone();
        public_inner.access = FfiAccessContext::Public;
        assert_ne!(
            inner, public_inner,
            "identical nested selections under different access contexts are distinct descriptors"
        );
        let outer = FfiDemand {
            selection: FfiFilter {
                kinds: Some(vec![1]),
                authors: Some(FfiBinding::Derived {
                    derived: std::sync::Arc::new(FfiDerived {
                        inner: inner.clone(),
                        project: FfiSelector::Tag {
                            name: "p".to_string(),
                        },
                    }),
                }),
                ..FfiFilter::default()
            },
            source: FfiSourceAuthority::Public,
            access: FfiAccessContext::Public,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        };

        let grammar = demand_from_ffi(outer.clone()).expect("every nested policy is valid");
        let binding = grammar
            .selection
            .authors
            .as_ref()
            .expect("outer authors binding");
        let GBinding::Derived(derived) = binding else {
            panic!("expected derived authors binding");
        };
        assert!(matches!(derived.inner.source, GSourceAuthority::Pinned(_)));
        assert!(matches!(derived.inner.access, GAccessContext::Nip42(_)));
        assert_eq!(derived.inner.cache, GCacheMode::Strict);
        assert_eq!(derived.inner.freshness, GFreshness::MaxAge { seconds: 600 });

        assert_eq!(demand_to_ffi(grammar), outer);

        let public_outer = FfiDemand {
            selection: FfiFilter {
                kinds: Some(vec![1]),
                authors: Some(FfiBinding::Derived {
                    derived: std::sync::Arc::new(FfiDerived {
                        inner: public_inner,
                        project: FfiSelector::Tag {
                            name: "p".to_string(),
                        },
                    }),
                }),
                ..FfiFilter::default()
            },
            source: FfiSourceAuthority::Public,
            access: FfiAccessContext::Public,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        };
        assert_ne!(outer, public_outer);
        assert_eq!(
            demand_to_ffi(demand_from_ffi(public_outer.clone()).unwrap()),
            public_outer
        );
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
            payload: FfiWritePayload::Event {
                builder: FfiEventBuilder {
                    kind: 1,
                    tags: vec![vec!["e".to_string(), "e".repeat(64)]],
                    content: "hello".to_string(),
                    created_at: Some(100),
                },
            },
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
            correlation: None,
        }
    }

    /// #1105: the routing vocabulary crossing this boundary is exactly two
    /// words, and each word maps to its own twin in BOTH directions.
    ///
    /// Written as a match over the pair rather than as two equality
    /// assertions, deliberately: every variant of both enums is named here,
    /// so a third `WriteRouting` or `FfiWriteRouting` variant makes this
    /// test stop COMPILING instead of quietly leaving one word unexercised.
    /// A runtime assertion could never say that. The same cardinality is
    /// enforced on the Swift and Kotlin surfaces, which no Rust test can
    /// see, by `scripts/check-routing-vocabulary.sh`.
    #[test]
    fn the_routing_vocabulary_is_two_words_in_both_directions() {
        let relay = "wss://chosen.example".to_string();

        for outbound in [
            GWriteRouting::Auto,
            GWriteRouting::Explicit(vec![parse_relay_url(&relay).expect("a well-formed relay")]),
        ] {
            match (&outbound, write_routing_to_ffi(outbound.clone())) {
                (GWriteRouting::Auto, FfiWriteRouting::Auto) => {}
                (
                    GWriteRouting::Explicit(relays),
                    FfiWriteRouting::Explicit { relays: projected },
                ) => {
                    assert_eq!(
                        relays.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        projected
                    );
                }
                (GWriteRouting::Auto, projected) | (GWriteRouting::Explicit(_), projected) => {
                    panic!("a routing word must project as itself, not as {projected:?}")
                }
            }
        }

        for inbound in [
            FfiWriteRouting::Auto,
            FfiWriteRouting::Explicit {
                relays: vec![relay.clone()],
            },
        ] {
            let intent = FfiWriteIntent {
                routing: inbound.clone(),
                ..valid_write_intent()
            };
            let parsed = write_intent_from_ffi(intent).expect("both words parse");
            match (&inbound, parsed.routing) {
                (FfiWriteRouting::Auto, GWriteRouting::Auto) => {}
                (FfiWriteRouting::Explicit { relays }, GWriteRouting::Explicit(parsed)) => {
                    assert_eq!(
                        relays,
                        &parsed.iter().map(ToString::to_string).collect::<Vec<_>>()
                    );
                }
                (FfiWriteRouting::Auto, _) | (FfiWriteRouting::Explicit { .. }, _) => {
                    panic!("a routing word must arrive as itself")
                }
            }
        }
    }

    /// #972: an app naming exact relays crosses the boundary verbatim --
    /// same relays, same order, nothing added by the boundary itself.
    #[test]
    fn explicit_routing_crosses_the_boundary_verbatim() {
        let intent = FfiWriteIntent {
            routing: FfiWriteRouting::Explicit {
                relays: vec![
                    "wss://user-typed-relay.example".to_string(),
                    "wss://second.example".to_string(),
                ],
            },
            ..valid_write_intent()
        };
        let parsed = write_intent_from_ffi(intent).expect("explicit routing must parse");
        let GWriteRouting::Explicit(relays) = parsed.routing else {
            panic!("explicit routing must project as explicit routing")
        };
        assert_eq!(
            relays.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                "wss://user-typed-relay.example".to_string(),
                "wss://second.example".to_string()
            ]
        );
        assert_eq!(
            write_routing_to_ffi(GWriteRouting::Explicit(relays)),
            FfiWriteRouting::Explicit {
                relays: vec![
                    "wss://user-typed-relay.example".to_string(),
                    "wss://second.example".to_string()
                ]
            },
            "the projection back out is the exact inverse"
        );
    }

    /// A malformed relay URL is a typed synchronous refusal at this
    /// boundary, before any engine call -- never a silently dropped relay,
    /// which would publish to a narrower set than the caller asked for.
    #[test]
    fn a_malformed_explicit_relay_refuses_the_whole_intent() {
        let intent = FfiWriteIntent {
            routing: FfiWriteRouting::Explicit {
                relays: vec![
                    "wss://fine.example".to_string(),
                    "not-a-relay-url".to_string(),
                ],
            },
            ..valid_write_intent()
        };
        match write_intent_from_ffi(intent).err() {
            Some(FfiError::InvalidRelayUrl { got }) => assert_eq!(got, "not-a-relay-url"),
            other => panic!("expected InvalidRelayUrl, got {other:?}"),
        }
    }

    /// Emptiness is NOT rejected here: it is a routing rule, enforced once
    /// at the engine's acceptance door so every surface gets the identical
    /// refusal rather than each boundary inventing its own.
    #[test]
    fn an_empty_explicit_route_parses_and_is_refused_downstream_instead() {
        let intent = FfiWriteIntent {
            routing: FfiWriteRouting::Explicit { relays: vec![] },
            ..valid_write_intent()
        };
        let parsed = write_intent_from_ffi(intent).expect("parsing is not where empty is caught");
        assert!(matches!(parsed.routing, GWriteRouting::Explicit(ref r) if r.is_empty()));
    }

    #[test]
    fn well_formed_write_intent_parses_ok() {
        let intent = valid_write_intent();
        let parsed = write_intent_from_ffi(intent).expect("well-formed intent must parse");
        match parsed.payload {
            GWritePayload::Event(builder) => assert_eq!(builder.tags.len(), 1),
            GWritePayload::ReplaceableEdit { .. } => {
                panic!("the raw FFI write surface must not mint guarded replaceable edits")
            }
            GWritePayload::Signed(_) => {
                panic!("an Event FfiWritePayload must build an Event GWritePayload")
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
        let FfiWritePayload::Event { builder } = &mut intent.payload else {
            unreachable!("valid_write_intent always builds an Event payload")
        };
        let tags = &mut builder.tags;
        *tags = vec![
            vec!["-".to_string()],
            vec!["poop".to_string(), "value".to_string()],
            vec!["alt".to_string(), "a human-readable summary".to_string()],
        ];
        let expected = tags.clone();

        let parsed = write_intent_from_ffi(intent)
            .expect("multi-character/punctuation event-tag names must not be rejected");
        let GWritePayload::Event(builder) = parsed.payload else {
            unreachable!("valid_write_intent always builds an Event payload")
        };
        let round_tripped: Vec<Vec<String>> =
            builder.tags.iter().map(|t| t.clone().to_vec()).collect();
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
        let FfiWritePayload::Event { builder } = &mut intent.payload else {
            unreachable!("valid_write_intent always builds an Event payload")
        };
        builder.tags.push(Vec::new()); // empty tag array: Tag::parse rejects this
        match write_intent_from_ffi(intent) {
            Err(err) => assert_eq!(err, FfiError::InvalidTag { got: Vec::new() }),
            Ok(_) => panic!("a malformed tag must fail closed, not silently drop"),
        }
    }

    /// Round-trip, hex form: a well-formed hex key lands in
    /// `nmp::WriteIntent::identity` as `Identity::Explicit` carrying the
    /// parsed `PublicKey`, never dropped or rewritten. On a builder payload
    /// that key is the only source of the author, so there is nothing
    /// beside it to agree with.
    #[test]
    fn an_explicit_identity_round_trips_as_the_parsed_pubkey() {
        let mut intent = valid_write_intent();
        intent.identity = FfiIdentity::Explicit { pubkey: pk_hex() };
        let parsed = write_intent_from_ffi(intent).expect("a hex identity must parse");
        assert_eq!(
            parsed.identity,
            GIdentity::Explicit(PublicKey::from_hex(&pk_hex()).expect("fixture hex is valid")),
            "an explicit identity must cross the boundary as the exact parsed key"
        );
    }

    /// The bech32 boundary rule, on the one field that used to break it: a
    /// perfectly well-formed `npub` for a real identity (fixture pair
    /// borrowed from `entity::tests`) is REFUSED here, with the same typed
    /// error every other malformed pubkey input gets. Not a parsing
    /// accident -- "which encodings does this field take" has one answer,
    /// and an app holding a display form decodes it at its own boundary
    /// with `decode_nostr_entity` before it ever reaches the write plane.
    #[test]
    fn a_bech32_npub_identity_is_refused_however_well_formed() {
        let npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
        let mut intent = valid_write_intent();
        intent.identity = FfiIdentity::Explicit {
            pubkey: npub.to_string(),
        };
        match write_intent_from_ffi(intent) {
            Err(FfiError::InvalidPublicKey { got }) => assert_eq!(got, npub),
            Err(other) => panic!("expected InvalidPublicKey, got: {other:?}"),
            Ok(_) => panic!("bech32 must not cross the write plane's identity input"),
        }
    }

    /// `Active` is a positive instruction, not an absence: it crosses as
    /// `Identity::Active` and means "whoever is the active account at
    /// acceptance", which is a resolution the engine performs rather than a
    /// field it skipped.
    #[test]
    fn active_crosses_as_active_not_as_an_absence() {
        let parsed = write_intent_from_ffi(valid_write_intent())
            .expect("a well-formed intent naming no key must parse");
        assert_eq!(
            parsed.identity,
            GIdentity::Active,
            "the default identity must mean the active account, nothing else"
        );
    }

    /// Fail-closed parse: a string that is not 64-char hex rejects the WHOLE
    /// intent with the same typed error every other malformed pubkey input
    /// gets, naming the offending string -- synchronously, before any engine
    /// call, so no receipt stream ever exists for it (the
    /// well-formed-but-CONTRADICTORY case on a signed payload is the
    /// acceptance boundary's `FfiError::PublishRefused`, not this error).
    #[test]
    fn a_malformed_identity_is_a_typed_error_not_a_panic() {
        for garbage in ["not-a-pubkey", "npub1notvalidbech32"] {
            let mut intent = valid_write_intent();
            intent.identity = FfiIdentity::Explicit {
                pubkey: garbage.to_string(),
            };
            match write_intent_from_ffi(intent) {
                Err(FfiError::InvalidPublicKey { got }) => assert_eq!(got, garbage),
                Err(other) => {
                    panic!("expected InvalidPublicKey, got a different FfiError: {other:?}")
                }
                Ok(_) => panic!("a malformed identity must fail closed, not parse"),
            }
        }
    }

    /// A real signed event (`EventBuilder::sign_with_keys`), rendered field-
    /// for-field into a `FfiWritePayload::Signed` the same way an app would
    /// after receiving one from an external signer provider.
    /// Reachability Gate for [`FfiError::ReplaceableEditHasNoWireForm`],
    /// and the falsifier for #951's payload axis: the projection door is
    /// TOTAL, so the one payload shape with no wire form comes back as a
    /// value instead of panicking on an exported path. A CAS-guarded
    /// replacement crosses this boundary only inside the semantic method
    /// that owns its precondition.
    #[test]
    fn a_replaceable_edit_refuses_as_a_value_rather_than_panicking() {
        let edit = GWritePayload::ReplaceableEdit {
            builder: GEventBuilder::new(nostr::Kind::ContactList).content("guarded"),
            expected_base: None,
        };
        assert_eq!(
            write_payload_to_ffi(edit),
            Err(FfiError::ReplaceableEditHasNoWireForm)
        );
    }

    /// The other side of that totality: every payload that DOES have a wire
    /// form projects faithfully, so a composer changing which one it mints
    /// crosses intact rather than tripping a closed-contract assertion.
    #[test]
    fn every_payload_with_a_wire_form_projects_faithfully() {
        let builder = GEventBuilder::new(nostr::Kind::TextNote)
            .content("hello")
            .created_at(Timestamp::from(42u64));
        assert_eq!(
            write_payload_to_ffi(GWritePayload::Event(builder)),
            Ok(FfiWritePayload::Event {
                builder: FfiEventBuilder {
                    kind: 1,
                    tags: Vec::new(),
                    content: "hello".to_string(),
                    created_at: Some(42),
                },
            })
        );

        let (event, _) = signed_write_intent();
        let projected = write_payload_to_ffi(GWritePayload::Signed(event.clone()))
            .expect("a signed event has a wire form");
        let FfiWritePayload::Signed { id, sig, .. } = projected else {
            panic!("a signed payload must project as Signed")
        };
        assert_eq!(id, event.id.to_hex());
        assert_eq!(sig, event.sig.to_string());
    }

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
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
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
            GWritePayload::Event(_) => {
                panic!("a Signed FfiWritePayload must build a Signed GWritePayload")
            }
            GWritePayload::ReplaceableEdit { .. } => {
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
    /// (`facade::tests::ffi_tampered_signed_publish_is_refused_by_publish_itself`)
    /// proves the rejection still happens, just downstream — as
    /// `FfiError::PublishRefused` from the engine's acceptance boundary
    /// rather than from this parse.
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
fn engine_start_failure_preserves_component_and_reason_across_ffi() {
    let error = FfiError::from(nmp::EngineError::EngineStartFailed {
        component: "signature verifier".to_string(),
        reason: "Resource temporarily unavailable".to_string(),
    });
    assert_eq!(
        error,
        FfiError::EngineStartFailed {
            component: "signature verifier".to_string(),
            reason: "Resource temporarily unavailable".to_string(),
        }
    );
}

#[test]
fn observation_unavailable_maps_to_domain_error_across_ffi() {
    // #704: an `observe` relay-worker/projection open failure crosses FFI as a
    // domain outcome carrying no worker/pool/thread concept.
    let error = FfiError::from(nmp::EngineError::ObservationUnavailable {
        reason: "relay worker: Resource temporarily unavailable".to_string(),
    });
    assert_eq!(
        error,
        FfiError::ObservationUnavailable {
            reason: "relay worker: Resource temporarily unavailable".to_string(),
        }
    );
}
