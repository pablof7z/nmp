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
/// `WritePayload.signed` event that fails `nostr::Event::verify` is no
/// longer rejected synchronously here (#52 Unit B: the guarantee moved to
/// `nmp-engine`'s acceptance boundary so it holds for every entry point, not
/// only this one). It surfaces on `Receipt.status` instead, as
/// `WriteStatus.failed`, the first and only status delivered.
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
    case receiptCorrelationIdExhausted
    case storeOpenFailed(String)
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
    /// #156: `groupMessageIntent` has no active account from which NMP can
    /// derive the unsigned event author.
    case noActiveAccount
    /// #115: `publishComposed` was called a second time on the same
    /// `GroupSendIntent` -- it is take-once by design (call
    /// `groupMessageIntent` again for a retry, since NMP-owned time and
    /// couriered evidence should refresh anyway).
    case intentAlreadyConsumed
    /// No last-good NIP-11 document exists and acquisition failed.
    case relayInformationUnavailable(RelayInformationErrorKind)
    /// #591: `WriteIntent.correlation`/`reattachReceipt(correlation:)` was
    /// given a token that failed `CorrelationToken`'s bounded/non-empty
    /// validation.
    case invalidCorrelationToken(got: String, reason: String)
    /// #572: an `Nip73Target` failed its constructor validation (an empty
    /// `I`/`K` cell).
    case invalidNip73Target(reason: String)

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
        case .ReceiptCorrelationIdExhausted: self = .receiptCorrelationIdExhausted
        case .StoreOpenFailed(let reason): self = .storeOpenFailed(reason)
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
        case .NoActiveAccount: self = .noActiveAccount
        case .IntentAlreadyConsumed: self = .intentAlreadyConsumed
        case .RelayInformationUnavailable(let kind):
            self = .relayInformationUnavailable(RelayInformationErrorKind(kind))
        case .InvalidCorrelationToken(let got, let reason):
            self = .invalidCorrelationToken(got: got, reason: reason)
        case .InvalidNip73Target(let reason):
            self = .invalidNip73Target(reason: reason)
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
        case .receiptCorrelationIdExhausted:
            "Receipt correlation ID namespace exhausted"
        case .storeOpenFailed(let reason):
            "Could not open store: \(reason)"
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
        case .noActiveAccount:
            "Group messages require an active account"
        case .intentAlreadyConsumed:
            "This composed write intent was already published once"
        case .relayInformationUnavailable(let kind):
            "Relay information unavailable: \(kind)"
        case .invalidCorrelationToken(let got, let reason):
            "Invalid correlation token \(got.debugDescription): \(reason)"
        case .invalidNip73Target(let reason):
            "Invalid NIP-73 target: \(reason)"
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
