import NMPFFI

/// One retained receipt identity in a bounded receipt-set observation.
public enum ReceiptSetIdentity: Sendable, Hashable {
    case id(UInt64)
    case correlation(String)

    func toFfi() -> FfiReceiptIdentity {
        switch self {
        case .id(let id): .id(receiptId: id)
        case .correlation(let token): .correlation(token: token)
        }
    }
}

/// Tagged receipt facts and exact per-identity recovery outcomes.
public enum ReceiptSetEvent: Sendable {
    case fact(identity: ReceiptSetIdentity, receiptId: UInt64, status: WriteStatus)
    case notFound(identity: ReceiptSetIdentity)
    case retainedButUnreadable(identity: ReceiptSetIdentity, receiptId: UInt64?)
    case replayAfterLag(identity: ReceiptSetIdentity, receiptId: UInt64)
    case replayUnavailable(identity: ReceiptSetIdentity, receiptId: UInt64)
    /// Emitted only after this receipt's complete replay/live stream closes.
    case closed(identity: ReceiptSetIdentity, receiptId: UInt64)

    init(_ event: FfiReceiptSetEvent) {
        switch event {
        case .fact(let identity, let receiptId, let status):
            self = .fact(
                identity: ReceiptSetIdentity(identity),
                receiptId: receiptId,
                status: WriteStatus(status)
            )
        case .notFound(let identity):
            self = .notFound(identity: ReceiptSetIdentity(identity))
        case .retainedButUnreadable(let identity, let receiptId):
            self = .retainedButUnreadable(
                identity: ReceiptSetIdentity(identity),
                receiptId: receiptId
            )
        case .replayAfterLag(let identity, let receiptId):
            self = .replayAfterLag(
                identity: ReceiptSetIdentity(identity),
                receiptId: receiptId
            )
        case .replayUnavailable(let identity, let receiptId):
            self = .replayUnavailable(
                identity: ReceiptSetIdentity(identity),
                receiptId: receiptId
            )
        case .closed(let identity, let receiptId):
            self = .closed(identity: ReceiptSetIdentity(identity), receiptId: receiptId)
        }
    }
}

private extension ReceiptSetIdentity {
    init(_ identity: FfiReceiptIdentity) {
        switch identity {
        case .id(let receiptId): self = .id(receiptId)
        case .correlation(let token): self = .correlation(token)
        }
    }
}

public enum NMPReceiptSetError: Error, Sendable, Equatable {
    case capacityExceeded(capacity: UInt64, requested: UInt64)
    case duplicateIdentity(String)
    case engineClosed

    init(_ error: FfiReceiptSetError) {
        switch error {
        case .CapacityExceeded(let capacity, let requested):
            self = .capacityExceeded(capacity: capacity, requested: requested)
        case .DuplicateIdentity(let identity):
            self = .duplicateIdentity(identity)
        case .EngineClosed:
            self = .engineClosed
        }
    }
}

/// One fair pull sequence over a finite set of retained receipts.
public struct ReceiptSetStatus: AsyncSequence, Sendable {
    public typealias Element = ReceiptSetEvent

    private let handle: NmpReceiptSetStream
    private let iteratorGate = NMPPullIteratorGate()

    init(handle: NmpReceiptSetStream) {
        self.handle = handle
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(
            core: NMPPullIteratorCore(
                handle: handle,
                iteratorGate: iteratorGate,
                map: { @Sendable event in ReceiptSetEvent(event) }
            )
        )
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpReceiptSetStream, ReceiptSetEvent>

        public mutating func next() async throws -> ReceiptSetEvent? {
            try await core.next()
        }
    }

    public func cancel() {
        handle.cancel()
    }
}

extension NMPEngine {
    /// The exact NMP-owned admission bound for one receipt-set observation.
    public var receiptSetCapacity: UInt64 {
        ffi.receiptSetCapacity()
    }

    /// Observe every retained identity fairly through one cancellation scope.
    public func observeReceipts(
        _ identities: [ReceiptSetIdentity]
    ) throws -> ReceiptSetStatus {
        do {
            return ReceiptSetStatus(
                handle: try ffi.observeReceipts(identities: identities.map { $0.toFfi() })
            )
        } catch let error as FfiReceiptSetError {
            throw NMPReceiptSetError(error)
        }
    }
}
