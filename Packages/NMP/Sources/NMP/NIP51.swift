import NMPFFI

public enum NMPRelayListActionFailure: Sendable, Hashable {
    case invalidRelay(String)
    case signedOut
    case accountChanged
    case acquisitionTimedOut
    case cachedOnly
    case sourceUnavailable
    case baseHasWrongAuthor
    case baseHasWrongKind
    case timestampExhausted
    case invalidGeneratedTag
    case engineClosed
    case receiptUnavailable
    case threadUnavailable(component: String, reason: String)
    case executorSaturated(component: String, capacity: UInt64)

    init(_ ffi: FfiRelayListActionFailure) {
        switch ffi {
        case .invalidRelay(let got): self = .invalidRelay(got)
        case .signedOut: self = .signedOut
        case .accountChanged: self = .accountChanged
        case .acquisitionTimedOut: self = .acquisitionTimedOut
        case .cachedOnly: self = .cachedOnly
        case .sourceUnavailable: self = .sourceUnavailable
        case .baseHasWrongAuthor: self = .baseHasWrongAuthor
        case .baseHasWrongKind: self = .baseHasWrongKind
        case .timestampExhausted: self = .timestampExhausted
        case .invalidGeneratedTag: self = .invalidGeneratedTag
        case .engineClosed: self = .engineClosed
        case .receiptUnavailable: self = .receiptUnavailable
        case .threadUnavailable(let component, let reason):
            self = .threadUnavailable(component: component, reason: reason)
        case .executorSaturated(let component, let capacity):
            self = .executorSaturated(component: component, capacity: capacity)
        }
    }
}

public enum NMPRelayListActionStatus: Sendable, Hashable {
    case acquiring
    case noChange(present: Bool)
    case receipt(id: UInt64, status: WriteStatus)
    case failed(NMPRelayListActionFailure)

    init(_ ffi: FfiRelayListActionStatus) {
        switch ffi {
        case .acquiring:
            self = .acquiring
        case .noChange(let present):
            self = .noChange(present: present)
        case .receipt(let receiptID, let status):
            self = .receipt(id: receiptID, status: WriteStatus(status))
        case .failed(let failure):
            self = .failed(NMPRelayListActionFailure(failure))
        }
    }
}

public struct NMPRelayListAction: Sendable {
    public let status: AsyncStream<NMPRelayListActionStatus>
}

private final class RelayListActionBridge: RelayListActionObserver, @unchecked Sendable {
    private let continuation: AsyncStream<NMPRelayListActionStatus>.Continuation

    init(continuation: AsyncStream<NMPRelayListActionStatus>.Continuation) {
        self.continuation = continuation
    }

    func onStatus(status: FfiRelayListActionStatus) {
        continuation.yield(NMPRelayListActionStatus(status))
    }

    func onClosed() {
        continuation.finish()
    }
}

extension NMPEngine {
    /// Add one public relay to the active account's NIP-51 kind:10009 list.
    /// NMP owns acquisition, exact-base preservation, signing, routing, and
    /// receipt state. Invalid URLs and operational failures arrive in-stream.
    public func addSimpleGroupRelay(_ relay: String) -> NMPRelayListAction {
        relayListAction(relay: relay, adding: true)
    }

    /// Remove matching public relay tags without removing remembered group
    /// entries or altering private content and unrelated tags.
    public func removeSimpleGroupRelay(_ relay: String) -> NMPRelayListAction {
        relayListAction(relay: relay, adding: false)
    }

    private func relayListAction(relay: String, adding: Bool) -> NMPRelayListAction {
        var continuation: AsyncStream<NMPRelayListActionStatus>.Continuation!
        let stream = AsyncStream<NMPRelayListActionStatus> { continuation = $0 }
        let bridge = RelayListActionBridge(continuation: continuation)
        if adding {
            ffi.addSimpleGroupRelay(relay: relay, observer: bridge)
        } else {
            ffi.removeSimpleGroupRelay(relay: relay, observer: bridge)
        }
        return NMPRelayListAction(status: stream)
    }
}
