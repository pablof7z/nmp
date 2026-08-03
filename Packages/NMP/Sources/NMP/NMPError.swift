// The ergonomic error surface: `NMP`'s public API never leaks the
// `Ffi*`-prefixed generated types (M4 plan §2/§9 -- "hide the Ffi* types
// behind ergonomic Swift enums/structs"). `FfiError` already conforms to
// `Swift.Error`, but re-wrapping it keeps the public surface entirely free
// of the `Ffi` prefix, matching every other value type in this package.

import Foundation
import NMPFFI

/// Every way a call into the engine can fail -- typed states, never a crash
/// (mirrors `nmp-ffi`'s own `FfiError`; see that type's doc for the Rust
/// side of each case).
///
/// NOTE: there is deliberately no `.invalidSignedEvent` case anymore -- a
/// `WritePayload.signed` event that fails `nostr::Event::verify` is rejected
/// at `nmp-engine`'s acceptance boundary rather than here, so the guarantee
/// holds for every entry point rather than only this one -- and because that
/// instruction can never resolve, it surfaces as `.publishRefused` from
/// `publish` itself rather than as a fact on a receipt stream nothing will
/// ever add to.
/// Receipt-correlation exhaustion is synchronous because no truthful
/// `Receipt` or status stream can be created without an identity.
///
public enum NMPError: Error, Sendable, Equatable {
    case nonIndexableFilterTag(String)
    case invalidPublicKey(String)
    case invalidEventId(String)
    case invalidRelayUrl(String)
    case invalidTag([String])
    case invalidSecretKey
    case invalidSigner(String)
    case authCapabilityRegistryFull(limit: UInt64)
    case authCapabilityInstanceExhausted
    case noActiveSigner
    case invalidSignRequest(String)
    case signerUnavailable(String)
    case signerRejected(String)
    case invalidSignerOutput(String)
    /// `publish` refused the call outright: either NMP could not write
    /// anything down, or the instruction could not resolve (no active
    /// account, a signature that does not verify, an explicit identity
    /// contradicting a signed payload's author, a reserved kind, an empty
    /// explicit route). Nothing durable exists and there is no queue entry to
    /// inspect.
    ///
    /// Everything else takes CUSTODY and fails in the queue where you can see
    /// it -- including a stale replaceable base, which succeeds here and
    /// arrives as `WriteOutcome.refused`.
    case publishRefused(String)
    case storeOpenFailed(String)
    /// #489: `NmpEngineConfig.storePath` names a persistent store already
    /// owned by this or another process. No second database owner and no
    /// partial engine were created.
    case storeAlreadyOpen(String)
    case storeResetFailed(String)
    case storeStillOpen(String)
    /// The engine could not be constructed (`NmpEngine.init`): a genuine
    /// engine-start infrastructure failure. Never raised by an ordinary
    /// operation (#704).
    case engineStartFailed(component: String, reason: String)
    /// A windowed `observe` could not open its canonical history projection
    /// because the store degraded during setup. This is the case's sole
    /// production meaning; relay connection/worker failure remains ordinary
    /// acquisition evidence in the observation stream (#704).
    case observationUnavailable(reason: String)
    /// #680: a second `next()` was awaited on an observation stream (or a
    /// `signed()` on a sign handle) while a previous one was still in flight.
    /// Every observation handle is single-consumer -- surface the misuse as a
    /// typed error, never a hang. No frame is lost or duplicated.
    case concurrentNext
    /// A durable FIFO fact stream crossed its finite live-delivery bound
    /// while the app was paused. No fact is claimed delivered and memory stays
    /// bounded; when present, reattach `receiptId` to replay persisted facts.
    case factStreamLagged(receiptId: UInt64?)
    case receiptReplayUnavailable(receiptId: UInt64)
    /// #680: `NmpSignEventHandle.signed()` was awaited a second time -- the
    /// one-shot result was already delivered to the first await.
    case signEventAlreadyConsumed
    case invalidSignature(String)
    case engineClosed
    /// `decodeNostrEntity`'s input was not valid bech32, had an
    /// unrecognized HRP prefix, or had a malformed inner TLV payload
    /// (#116).
    case invalidNostrEntity(String)
    /// `decodeNostrEntity`'s input decoded to `nsec`/`ncryptsec` -- refused
    /// rather than decoded (#116).
    case nostrEntitySecretKeyRejected
    /// An `NMPDemand` declared `.authorOutboxes` over a selection whose
    /// `authors` field is unbound (#107).
    case authorOutboxesRequiresBoundAuthors
    /// An `NMPDemand` declared `.pinned([])` -- an empty relay set (#107
    /// Contract: "the pinned relay set must be nonempty").
    case emptyPinnedRelaySet
    /// A windowed `observe` declared a zero `initial` or `max` row count
    /// (#485) -- an empty window can neither deliver nor grow.
    case windowZeroRows
    /// A windowed `observe` declared `initial > max` (#485).
    case windowInitialExceedsMax(initial: UInt64, max: UInt64)
    /// A windowed `observe` was given a selection that already carries its
    /// own NIP-01 `limit` (#485) -- the window IS the bound; carrying a
    /// second, competing bound on the wire filter is refused rather than
    /// silently reconciled.
    case windowSelectionHasLimit
    /// A windowed `observe` was given a live query that already declares an
    /// aggregate result limit (#1108) -- the window and the aggregate bound
    /// would be two competing owners of the merged row count.
    case windowAggregateResultLimit
    /// A live query was declared with no demand branches at all (#1108).
    case emptyQueryUnion
    /// A live query declared an aggregate result limit of zero (#1108): a
    /// query that may never contain a row is not a bound.
    case aggregateResultLimitZero
    /// A nested live-query branch carried its own aggregate result limit
    /// (#1108). Branches flatten into one canonical set, so an inner bound
    /// has no surviving scope and accepting it would silently discard it.
    case nestedAggregateResultLimit
    /// A live query declared more branches than the supported hard ceiling
    /// (#1108). The whole declaration is refused; no subset is installed.
    case tooManyQueryBranches(requested: UInt64, maximum: UInt64)
    /// No last-good NIP-11 document exists and acquisition failed.
    case relayInformationUnavailable(RelayInformationErrorKind)
    /// #591: `WriteIntent.correlation`/`reattachReceipt(correlation:)` was
    /// given a token that failed `CorrelationToken`'s bounded/non-empty
    /// validation.
    case invalidCorrelationToken(got: String, reason: String)
    /// #572: an `Nip73Target` failed its constructor validation (an empty
    /// `I`/`K` cell).
    case invalidNip73Target(reason: String)
    /// #973: a composer returned a compare-and-swap replaceable edit, which
    /// has no wire form on purpose -- a replaceable precondition crosses
    /// this boundary only inside the semantic method that owns it
    /// (`follow`/`unfollow`), never as a payload a native caller could
    /// reassemble without the guard.
    case replaceableEditHasNoWireForm
    /// #1033: `NMPRelayScope.on`/`FfiRelayScope.on` was given an empty
    /// relay set -- a group must be hosted somewhere.
    case emptyRelayScope
    /// #1033: an event builder handed to `NMPGroup.publish` already carried
    /// its own `h` tag. The retained group id is the sole semantic source of
    /// that tag; a caller-supplied one is refused before any write reaches
    /// the door.
    case groupCallerSuppliedContext
    /// #1033: a read selection handed to `NMPGroup.read` already constrained
    /// `#h`. The retained group id is the sole semantic source of that row.
    case groupCallerSuppliedContextConstraint
    /// #1033: a read selection handed to `NMPGroup.read` already declared a
    /// `since`/`until`/`limit` timeline bound the group door itself owns.
    case groupCallerSuppliedTimeline
    /// #1033: `NMPGroup.validateContext`/`publishSigned` was given an event
    /// carrying no `h` tag naming any group at all.
    case groupContextMissing(expected: String)
    /// #1033: an already-signed event's `h` tag names a different group
    /// than the one it was handed to.
    case groupContextMismatched(found: String, expected: String)
    /// #1033: an already-signed event carried more than one distinct `h`
    /// tag, so which group it belongs to is ambiguous.
    case groupContextAmbiguous(expected: String)

    init(_ ffi: FfiError) {
        switch ffi {
        case .NonIndexableFilterTag(let got): self = .nonIndexableFilterTag(got)
        case .InvalidPublicKey(let got): self = .invalidPublicKey(got)
        case .InvalidEventId(let got): self = .invalidEventId(got)
        case .InvalidRelayUrl(let got): self = .invalidRelayUrl(got)
        case .InvalidTag(let got): self = .invalidTag(got)
        case .InvalidSecretKey: self = .invalidSecretKey
        case .InvalidSigner(let reason): self = .invalidSigner(reason)
        case .AuthCapabilityRegistryFull(let limit):
            self = .authCapabilityRegistryFull(limit: limit)
        case .AuthCapabilityInstanceExhausted:
            self = .authCapabilityInstanceExhausted
        case .NoActiveSigner: self = .noActiveSigner
        case .InvalidSignRequest(let reason): self = .invalidSignRequest(reason)
        case .PublishRefused(let reason): self = .publishRefused(reason)
        case .StoreOpenFailed(let reason): self = .storeOpenFailed(reason)
        case .StoreAlreadyOpen(let path): self = .storeAlreadyOpen(path)
        case .StoreResetFailed(let reason): self = .storeResetFailed(reason)
        case .StoreStillOpen(let path): self = .storeStillOpen(path)
        case .EngineStartFailed(let component, let reason):
            self = .engineStartFailed(component: component, reason: reason)
        case .ObservationUnavailable(let reason):
            self = .observationUnavailable(reason: reason)
        case .ConcurrentNext: self = .concurrentNext
        case .FactStreamLagged(let receiptId):
            self = .factStreamLagged(receiptId: receiptId)
        case .ReceiptReplayUnavailable(let receiptId):
            self = .receiptReplayUnavailable(receiptId: receiptId)
        case .InvalidSignature(let got): self = .invalidSignature(got)
        case .EngineClosed: self = .engineClosed
        case .InvalidNostrEntity(let reason): self = .invalidNostrEntity(reason)
        case .NostrEntitySecretKeyRejected: self = .nostrEntitySecretKeyRejected
        case .AuthorOutboxesRequiresBoundAuthors: self = .authorOutboxesRequiresBoundAuthors
        case .EmptyPinnedRelaySet: self = .emptyPinnedRelaySet
        case .WindowZeroRows: self = .windowZeroRows
        case .WindowInitialExceedsMax(let initial, let max):
            self = .windowInitialExceedsMax(initial: initial, max: max)
        case .WindowSelectionHasLimit: self = .windowSelectionHasLimit
        case .WindowAggregateResultLimit: self = .windowAggregateResultLimit
        case .EmptyQueryUnion: self = .emptyQueryUnion
        case .AggregateResultLimitZero: self = .aggregateResultLimitZero
        case .NestedAggregateResultLimit: self = .nestedAggregateResultLimit
        case .TooManyQueryBranches(let requested, let maximum):
            self = .tooManyQueryBranches(requested: requested, maximum: maximum)
        case .RelayInformationUnavailable(let kind):
            self = .relayInformationUnavailable(RelayInformationErrorKind(kind))
        case .InvalidCorrelationToken(let got, let reason):
            self = .invalidCorrelationToken(got: got, reason: reason)
        case .InvalidNip73Target(let reason):
            self = .invalidNip73Target(reason: reason)
        case .ReplaceableEditHasNoWireForm: self = .replaceableEditHasNoWireForm
        case .EmptyRelayScope: self = .emptyRelayScope
        case .GroupCallerSuppliedContext: self = .groupCallerSuppliedContext
        case .GroupCallerSuppliedContextConstraint:
            self = .groupCallerSuppliedContextConstraint
        case .GroupCallerSuppliedTimeline: self = .groupCallerSuppliedTimeline
        case .GroupContextMissing(let expected): self = .groupContextMissing(expected: expected)
        case .GroupContextMismatched(let found, let expected):
            self = .groupContextMismatched(found: found, expected: expected)
        case .GroupContextAmbiguous(let expected):
            self = .groupContextAmbiguous(expected: expected)
        }
    }
}

extension NMPError: LocalizedError {
    /// Stable native presentation for every typed failure. Keep this switch
    /// exhaustive: adding a new error case must also decide what ordinary
    /// Swift clients show without discarding its evidence.
    public var errorDescription: String? {
        switch self {
        case .nonIndexableFilterTag(let got):
            "Not indexable as a filter key: \(got.debugDescription)"
        case .invalidPublicKey(let got):
            "Invalid public key hex: \(got.debugDescription)"
        case .invalidEventId(let got):
            "Invalid event ID hex: \(got.debugDescription)"
        case .invalidRelayUrl(let got):
            "Invalid relay URL: \(got.debugDescription)"
        case .invalidTag(let got):
            "Invalid tag: \(String(reflecting: got))"
        case .invalidSecretKey:
            "Invalid secret key"
        case .invalidSigner(let reason):
            "Invalid signer: \(reason)"
        case .authCapabilityRegistryFull(let limit):
            "AUTH capability registry is full at \(limit) entries"
        case .authCapabilityInstanceExhausted:
            "AUTH capability instance space exhausted"
        case .noActiveSigner:
            "The active account has no registered signer"
        case .invalidSignRequest(let reason):
            "Invalid sign request: \(reason)"
        case .signerUnavailable(let reason):
            "Signer unavailable: \(reason)"
        case .signerRejected(let reason):
            "Signer rejected the request: \(reason)"
        case .invalidSignerOutput(let reason):
            "Invalid signer output: \(reason)"
        case .publishRefused(let reason):
            reason
        case .storeOpenFailed(let reason):
            "Could not open store: \(reason)"
        case .storeAlreadyOpen(let path):
            "Persistent store is already open: \(path)"
        case .storeResetFailed(let reason):
            "Could not reset store: \(reason)"
        case .storeStillOpen(let path):
            "Persistent store is still open: \(path)"
        case .engineStartFailed(let component, let reason):
            "Engine could not start (\(component)): \(reason)"
        case .observationUnavailable(let reason):
            "Observation could not be established: \(reason)"
        case .concurrentNext:
            "A next()/signed() call was awaited while a previous one was still in flight; observation streams are single-consumer"
        case .factStreamLagged(let receiptId?):
            "The finite live fact stream fell behind; reattach receipt \(receiptId) to replay"
        case .factStreamLagged(receiptId: nil):
            "The finite live fact stream fell behind before a receipt was observable"
        case .receiptReplayUnavailable(let receiptId):
            "Retained evidence for receipt \(receiptId) became unavailable during replay"
        case .signEventAlreadyConsumed:
            "This sign-event result was already consumed"
        case .invalidSignature(let got):
            "Invalid signature hex: \(got.debugDescription)"
        case .engineClosed:
            "Engine already shut down"
        case .invalidNostrEntity(let reason):
            "Invalid Nostr entity: \(reason)"
        case .nostrEntitySecretKeyRejected:
            "Refusing to decode a secret-key entity"
        case .authorOutboxesRequiresBoundAuthors:
            "SourceAuthority.authorOutboxes requires a selection whose authors field is bound"
        case .emptyPinnedRelaySet:
            "SourceAuthority.pinned requires a nonempty relay set"
        case .windowZeroRows:
            "Window initial/max must be representable nonzero row counts"
        case .windowInitialExceedsMax(let initial, let max):
            "Window initial \(initial) exceeds max \(max)"
        case .windowSelectionHasLimit:
            "A windowed selection must not also declare a limit"
        case .windowAggregateResultLimit:
            "A windowed observation must not also declare an aggregate result limit"
        case .emptyQueryUnion:
            "A live query must declare at least one demand branch"
        case .aggregateResultLimitZero:
            "An aggregate result limit of zero can never contain a row"
        case .nestedAggregateResultLimit:
            "A nested live-query branch must not declare its own aggregate result limit"
        case .tooManyQueryBranches(let requested, let maximum):
            "A live query supports at most \(maximum) demand branches; \(requested) were declared"
        case .relayInformationUnavailable(let kind):
            "Relay information unavailable: \(kind)"
        case .invalidCorrelationToken(let got, let reason):
            "Invalid correlation token \(got.debugDescription): \(reason)"
        case .invalidNip73Target(let reason):
            "Invalid NIP-73 target: \(reason)"
        case .replaceableEditHasNoWireForm:
            "A replaceable edit crosses this boundary only inside the semantic method that owns its precondition, never as a payload"
        case .emptyRelayScope:
            "RelayScope.on requires a nonempty relay set -- a group must be hosted somewhere"
        case .groupCallerSuppliedContext:
            "A group write must not carry its own h tag; the group's retained id is the sole source of that tag"
        case .groupCallerSuppliedContextConstraint:
            "A group read selection must not already constrain #h; the group's retained id is the sole source of that row"
        case .groupCallerSuppliedTimeline:
            "A group read selection must not already declare since/until/limit; the group door owns that bound"
        case .groupContextMissing(let expected):
            "Event carries no h tag; expected group \(expected.debugDescription)"
        case .groupContextMismatched(let found, let expected):
            "Event's h tag \(found.debugDescription) does not match expected group \(expected.debugDescription)"
        case .groupContextAmbiguous(let expected):
            "Event carries more than one distinct h tag; expected exactly group \(expected.debugDescription)"
        }
    }
}

/// Runs `body`, translating any thrown `FfiError` into the ergonomic
/// `NMPError` -- the one seam every call into `NMPFFI` passes through.
func nmpRethrowing<T>(_ body: () throws -> T) throws -> T {
    do {
        return try body()
    } catch let error as FfiError {
        throw NMPError(error)
    }
}

/// Async counterpart used by generated UniFFI operations. The suspension
/// remains visible to callers while preserving the ergonomic error surface.
func nmpRethrowingAsync<T>(_ body: () async throws -> T) async throws -> T {
    do {
        return try await body()
    } catch let error as FfiError {
        throw NMPError(error)
    }
}
