// The ergonomic error surface: `NMP`'s public API never leaks the
// `Ffi*`-prefixed generated types (M4 plan §2/§9 -- "hide the Ffi* types
// behind ergonomic Swift enums/structs"). `FfiError` already conforms to
// `Swift.Error`, but re-wrapping it keeps the public surface entirely free
// of the `Ffi` prefix, matching every other value type in this package.

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
    case noActiveSigner
    case invalidSignRequest(String)
    case signerUnavailable(String)
    case signerRejected(String)
    case invalidSignerOutput(String)
    case receiptCorrelationIdExhausted
    case storeOpenFailed(String)
    case storeResetFailed(String)
    case threadUnavailable(component: String, reason: String)
    case executorSaturated(component: String, capacity: UInt64)
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
    case historyPageSizeOutOfRange(UInt64)
    case historyMaxRowsOutOfRange(UInt64)
    case historyZeroPageSize
    case historyZeroMaxRows
    case historyPageExceedsMaxRows(pageSize: UInt64, maxRows: UInt64)
    case historySelectionHasLimit
    /// #156: `groupMessageIntent` has no active account from which NMP can
    /// derive the unsigned event author.
    case noActiveAccount
    /// #115: `publishComposed` was called a second time on the same
    /// `GroupSendIntent` -- it is take-once by design (call
    /// `groupMessageIntent` again for a retry, since NMP-owned time and
    /// couriered evidence should refresh anyway).
    case intentAlreadyConsumed
    /// No last-good NIP-11 document exists and acquisition failed.
    case relayInformationUnavailable(String)
    /// One relay's in-flight NIP-11 waiter set is at its finite bound.
    case relayInformationWaitersSaturated(capacity: UInt64)

    init(_ ffi: FfiError) {
        switch ffi {
        case .NonIndexableFilterTag(let got): self = .nonIndexableFilterTag(got)
        case .InvalidPublicKey(let got): self = .invalidPublicKey(got)
        case .InvalidEventId(let got): self = .invalidEventId(got)
        case .InvalidRelayUrl(let got): self = .invalidRelayUrl(got)
        case .InvalidTag(let got): self = .invalidTag(got)
        case .InvalidSecretKey: self = .invalidSecretKey
        case .InvalidSigner(let reason): self = .invalidSigner(reason)
        case .NoActiveSigner: self = .noActiveSigner
        case .InvalidSignRequest(let reason): self = .invalidSignRequest(reason)
        case .ReceiptCorrelationIdExhausted: self = .receiptCorrelationIdExhausted
        case .StoreOpenFailed(let reason): self = .storeOpenFailed(reason)
        case .StoreResetFailed(let reason): self = .storeResetFailed(reason)
        case .ThreadUnavailable(let component, let reason):
            self = .threadUnavailable(component: component, reason: reason)
        case .ExecutorSaturated(let component, let capacity):
            self = .executorSaturated(component: component, capacity: capacity)
        case .InvalidSignature(let got): self = .invalidSignature(got)
        case .EngineClosed: self = .engineClosed
        case .InvalidNostrEntity(let reason): self = .invalidNostrEntity(reason)
        case .NostrEntitySecretKeyRejected: self = .nostrEntitySecretKeyRejected
        case .AuthorOutboxesRequiresBoundAuthors: self = .authorOutboxesRequiresBoundAuthors
        case .EmptyPinnedRelaySet: self = .emptyPinnedRelaySet
        case .HistoryPageSizeOutOfRange(let value): self = .historyPageSizeOutOfRange(value)
        case .HistoryMaxRowsOutOfRange(let value): self = .historyMaxRowsOutOfRange(value)
        case .HistoryZeroPageSize: self = .historyZeroPageSize
        case .HistoryZeroMaxRows: self = .historyZeroMaxRows
        case .HistoryPageExceedsMaxRows(let pageSize, let maxRows):
            self = .historyPageExceedsMaxRows(pageSize: pageSize, maxRows: maxRows)
        case .HistorySelectionHasLimit: self = .historySelectionHasLimit
        case .NoActiveAccount: self = .noActiveAccount
        case .IntentAlreadyConsumed: self = .intentAlreadyConsumed
        case .RelayInformationUnavailable(let reason): self = .relayInformationUnavailable(reason)
        case .RelayInformationWaitersSaturated(let capacity):
            self = .relayInformationWaitersSaturated(capacity: capacity)
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
