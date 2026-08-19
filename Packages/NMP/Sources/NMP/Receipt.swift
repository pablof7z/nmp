// The write noun's receipt stream, pulled from `NmpReceiptStream` (#680).
// Receipts are durable FIFO facts (not disposable snapshots): live delivery
// has a finite FIFO and typed lag, while persisted outbox/redb evidence is
// replayed in deterministic pages. There is no coalescing here.

import NMPFFI

/// The ordered `WriteFact` facts a single `publish` call's write reaches
/// (guarantee #9 -- enqueue is not converged), pulled from its durable receipt
/// handle (#680). Live delivery is finite: a paused consumer that falls
/// behind receives `NMPError.factStreamLagged` and can reattach the named
/// receipt to replay the canonical persisted history. It finishes (`nil`) when
/// the engine has nothing
/// further to report for this intent (e.g. an `Ephemeral` intent may finish
/// immediately after `.sent`, a `Durable` one only after every relay has
/// reached a terminal state or given up). Iterate with `for try await`; the
/// handle is single-consumer, so a second concurrent iterator surfaces
/// `NMPError.concurrentNext` rather than hanging.
public struct ReceiptStatus: AsyncSequence, Sendable {
    public typealias Element = WriteFact

    private let handle: NmpReceiptStream
    private let iteratorGate = NMPPullIteratorGate()

    init(handle: NmpReceiptStream) {
        self.handle = handle
    }

    public func makeAsyncIterator() -> Iterator {
        let core = NMPPullIteratorCore(handle: handle, iteratorGate: iteratorGate) { status in
            WriteFact(status)
        }
        return Iterator(core: core)
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpReceiptStream, WriteFact>

        public mutating func next() async throws -> WriteFact? {
            try await core.next()
        }
    }

    /// Stop delivering live status frames to this stream. The durable receipt
    /// is untouched (use `NMPEngine.cancel(receiptId:)` to cancel the write);
    /// a later `reattachReceipt` traverses the durable history. Idempotent.
    public func cancel() {
        handle.cancel()
    }
}

/// One accepted write and its live status stream. `id` is the stable
/// store-issued receipt id, usable for `reattachReceipt`/`cancel` even after
/// `status` is dropped.
public struct Receipt: Sendable {
    public let id: UInt64
    public let status: ReceiptStatus
    private let handle: NmpReceiptStream

    init(handle: NmpReceiptStream) {
        self.id = handle.id()
        self.status = ReceiptStatus(handle: handle)
        self.handle = handle
    }

    /// Await the one terminal publication answer. NMP owns receipt reduction
    /// and durable replay; callers do not fold `status` themselves.
    public func result() async throws -> ReceiptResult {
        let result = try await nmpRethrowingAsync {
            try await handle.result()
        }
        return ReceiptResult(result)
    }
}

public struct ReceiptRelayResult: Sendable, Hashable {
    public let relay: String
    public let state: RelayState

    init(_ ffi: FfiReceiptRelayResult) {
        self.relay = ffi.relay
        self.state = RelayState(ffi.state)
    }
}

/// NMP's terminal answer for one accepted write. Every known destination's
/// final state remains visible, including mixed publish/reject outcomes.
public struct ReceiptResult: Sendable, Hashable {
    public let outcome: WriteOutcome
    public let relays: [ReceiptRelayResult]

    init(_ ffi: FfiReceiptResult) {
        self.outcome = WriteOutcome(ffi.outcome)
        self.relays = ffi.relays.map(ReceiptRelayResult.init)
    }
}

public enum ReceiptReattachment: Sendable {
    case attached(Receipt)
    case notFound
    case retainedButUnreadable
}

/// Typed refusals from explicit pre-signature write cancellation.
public enum WriteCancellationOutcome: Sendable, Equatable {
    case cancelled
}

public enum NMPWriteCancellationError: Error, Sendable, Equatable {
    case unknownReceipt(receiptId: UInt64)
    case alreadySigned(receiptId: UInt64, eventId: String)
    case alreadyCompensated(receiptId: UInt64)
    case alreadySuperseded(receiptId: UInt64)
    /// The write was refused at acceptance and is already a permanently
    /// failed queue entry. There is nothing to cancel; remove it instead.
    case alreadyRefused(receiptId: UInt64)
    case persistenceFailed(receiptId: UInt64, reason: String)
    case engineClosed

    init(_ ffi: FfiCancelWriteError) {
        switch ffi {
        case .UnknownReceipt(let receiptId):
            self = .unknownReceipt(receiptId: receiptId)
        case .AlreadySigned(let receiptId, let eventId):
            self = .alreadySigned(receiptId: receiptId, eventId: eventId)
        case .AlreadyCompensated(let receiptId):
            self = .alreadyCompensated(receiptId: receiptId)
        case .AlreadySuperseded(let receiptId):
            self = .alreadySuperseded(receiptId: receiptId)
        case .AlreadyRefused(let receiptId):
            self = .alreadyRefused(receiptId: receiptId)
        case .PersistenceFailed(let receiptId, let reason):
            self = .persistenceFailed(receiptId: receiptId, reason: reason)
        case .EngineClosed:
            self = .engineClosed
        }
    }
}

/// Typed refusals from the queue-entry removal door.
public enum NMPQueueEntryRemovalError: Error, Sendable, Equatable {
    case unknownReceipt(receiptId: UInt64)
    /// The write still owns open delivery work. Cancel it first; removal is
    /// for entries nothing is going to move.
    case stillActive(receiptId: UInt64)
    case persistenceFailed(receiptId: UInt64, reason: String)
    case engineClosed

    init(_ ffi: FfiRemoveQueueEntryError) {
        switch ffi {
        case .UnknownReceipt(let receiptId):
            self = .unknownReceipt(receiptId: receiptId)
        case .StillActive(let receiptId):
            self = .stillActive(receiptId: receiptId)
        case .PersistenceFailed(let receiptId, let reason):
            self = .persistenceFailed(receiptId: receiptId, reason: reason)
        case .EngineClosed:
            self = .engineClosed
        }
    }
}

/// Typed failures from bounded publish-queue inspection.
public enum NMPPublishQueueError: Error, Sendable, Equatable {
    case invalidEventID(reason: String)
    case persistenceFailed(reason: String)
    case engineClosed

    init(_ ffi: FfiPublishQueueError) {
        switch ffi {
        case .InvalidEventId(let reason): self = .invalidEventID(reason: reason)
        case .PersistenceFailed(let reason): self = .persistenceFailed(reason: reason)
        case .EngineClosed: self = .engineClosed
        }
    }
}

extension NMPEngine {
    /// Read your own publish queue back.
    ///
    /// Answers "what have I got outstanding, and what went wrong with it"
    /// without having held a receipt stream open since acceptance. This is
    /// INSPECTION: it never blocks and never waits for settlement.
    public func publishQueue(
        afterReceiptID: UInt64? = nil,
        limit: UInt8
    ) throws -> [PublishQueueEntry] {
        do {
            return try ffi.publishQueue(
                afterReceiptId: afterReceiptID,
                limit: limit
            ).map(PublishQueueEntry.init)
        } catch let error as FfiPublishQueueError {
            throw NMPPublishQueueError(error)
        }
    }

    /// Read one bounded page of currently open obligations for a query row's
    /// exact event id. Reattach each returned receipt id to observe progress.
    public func publishQueue(
        forEventID eventID: String,
        afterReceiptID: UInt64? = nil,
        limit: UInt8
    ) throws -> [PublishQueueEntry] {
        do {
            return try ffi.publishQueueForEvent(
                eventId: eventID,
                afterReceiptId: afterReceiptID,
                limit: limit
            ).map(PublishQueueEntry.init)
        } catch let error as FfiPublishQueueError {
            throw NMPPublishQueueError(error)
        }
    }

    /// Forget one queue entry.
    ///
    /// A real TERMINATION path, not housekeeping: a write parked forever on a
    /// account whose signing provider never became available, and a permanently-failed refused entry,
    /// end no other way. A write that still owns open delivery work is
    /// refused -- cancel that one instead.
    public func removePublishQueueEntry(receiptId: UInt64) throws {
        do {
            try ffi.removePublishQueueEntry(receiptId: receiptId)
        } catch let error as FfiRemoveQueueEntryError {
            throw NMPQueueEntryRemovalError(error)
        }
    }

    /// Cancel an accepted unsigned write. Returns the durable terminal fact;
    /// repeated cancellation returns `.cancelled` idempotently.
    public func cancel(receiptId: UInt64) throws -> WriteCancellationOutcome {
        do {
            switch try ffi.cancel(receiptId: receiptId) {
            case .cancelled: return .cancelled
            }
        } catch let error as FfiCancelWriteError {
            throw NMPWriteCancellationError(error)
        }
    }

    /// Enqueue a write. Returns as soon as the intent is accepted into the
    /// outbox; `Receipt.status` streams everything that happens to it after
    /// that (M4 plan §9 -- `publish` is a one-shot enqueue call, the
    /// STREAM is where convergence is observed).
    public func publish(_ intent: WriteIntent) async throws -> Receipt {
        let handle = try nmpRethrowing {
            try ffi.publish(intent: intent.toFfi())
        }
        return Receipt(handle: handle)
    }

    /// Attach a fresh pull stream to retained receipt facts (#680): the
    /// `.attached` result carries a new `NmpReceiptStream` that transparently
    /// traverses the durable `WriteFact` history in finite pages and then
    /// streams onward. Corrupt or disappearing durable evidence is reported
    /// distinctly and never treated as absence.
    public func reattachReceipt(id: UInt64) throws -> ReceiptReattachment {
        let result = try nmpRethrowing {
            try ffi.reattachReceipt(receiptId: id)
        }
        switch result {
        case .attached(let stream):
            return .attached(Receipt(handle: stream))
        case .notFound:
            return .notFound
        case .retainedButUnreadable:
            return .retainedButUnreadable
        }
    }

}
