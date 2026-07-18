// The write noun's receipt stream, pulled from `NmpReceiptStream` (#680).
// Receipts are durable FIFO facts (not disposable snapshots): the persisted
// outbox/redb store is the source of truth, so there is no coalescing here.

import NMPFFI

/// The stream of every `WriteStatus` a single `publish` call's write reaches
/// (ledger #9 -- enqueue is not converged), pulled in order from the durable
/// receipt handle (#680). It finishes (`nil`) when the engine has nothing
/// further to report for this intent (e.g. an `Ephemeral` intent may finish
/// immediately after `.sent`, a `Durable` one only after every relay has
/// reached a terminal state or given up). Iterate with `for try await`; the
/// handle is single-consumer, so a second concurrent iterator surfaces
/// `NMPError.concurrentNext` rather than hanging.
public struct ReceiptStatus: AsyncSequence, Sendable {
    public typealias Element = WriteStatus

    private let handle: NmpReceiptStream

    init(handle: NmpReceiptStream) {
        self.handle = handle
    }

    public func makeAsyncIterator() -> Iterator {
        let stream = nmpPullStream(handle: handle) { status in WriteStatus(status) }
        return Iterator(base: stream.makeAsyncIterator())
    }

    public struct Iterator: AsyncIteratorProtocol {
        var base: AsyncThrowingStream<WriteStatus, Error>.AsyncIterator

        public mutating func next() async throws -> WriteStatus? {
            try await base.next()
        }
    }

    /// Stop delivering live status frames to this stream. The durable receipt
    /// is untouched (use `NMPEngine.cancel(receiptId:)` to cancel the write);
    /// a later `reattachReceipt` replays the durable prefix. Idempotent.
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

    init(handle: NmpReceiptStream) {
        self.id = handle.id()
        self.status = ReceiptStatus(handle: handle)
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
    case alreadyAbandoned(receiptId: UInt64)
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
        case .AlreadyAbandoned(let receiptId):
            self = .alreadyAbandoned(receiptId: receiptId)
        case .PersistenceFailed(let receiptId, let reason):
            self = .persistenceFailed(receiptId: receiptId, reason: reason)
        case .EngineClosed:
            self = .engineClosed
        }
    }
}

extension NMPEngine {
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

    /// Publish a `GroupSendIntent` from `groupMessageIntent` (#156). Take-once:
    /// `intent` is consumed by this call -- a second `publishComposed` on
    /// the SAME `GroupSendIntent` throws `NMPError.intentAlreadyConsumed`
    /// rather than silently re-publishing a stale template (recompose via
    /// `groupMessageIntent` again for a retry). Otherwise identical to
    /// `publish(_:)`'s receipt-stream bridge.
    public func publishComposed(_ intent: GroupSendIntent) async throws -> Receipt {
        let handle = try nmpRethrowing {
            try ffi.publishComposed(intent: intent.ffi)
        }
        return Receipt(handle: handle)
    }

    /// Publish a `CommentIntent` from `commentIntent` (#572). Take-once --
    /// see `publishComposed(_ intent: GroupSendIntent)`'s own doc; identical
    /// contract, just for the NIP-22 composed intent, delivered pull-based
    /// over `Receipt.status` (#680).
    public func publishComposed(_ intent: CommentIntent) async throws -> Receipt {
        let handle = try nmpRethrowing {
            try ffi.publishComposed(intent: intent.ffi)
        }
        return Receipt(handle: handle)
    }

    /// Attach a fresh pull stream to retained receipt facts (#680): the
    /// `.attached` result carries a new `NmpReceiptStream` that replays the
    /// durable `WriteStatus` prefix from the store and streams onward. Corrupt
    /// durable evidence is reported distinctly and never treated as absence.
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

    /// #591: recover a receipt after a crash that happened BEFORE the app
    /// could durably persist the receipt id `publish`/`publishComposed`
    /// returned -- looked up by the caller's own crash-safe correlation
    /// token instead. Otherwise identical to `reattachReceipt(id:)`.
    public func reattachReceipt(correlation: String) throws -> ReceiptReattachment {
        let result = try nmpRethrowing {
            try ffi.reattachByCorrelation(correlation: correlation)
        }
        // The resolved receipt id (#591) rides along on the attached stream
        // handle itself (`Receipt.id`); a token-only caller learns it there.
        switch result.outcome {
        case .attached(let stream):
            return .attached(Receipt(handle: stream))
        case .notFound:
            return .notFound
        case .retainedButUnreadable:
            return .retainedButUnreadable
        }
    }
}
