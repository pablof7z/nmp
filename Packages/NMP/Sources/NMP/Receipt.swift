// The write noun's receipt stream, mirroring the `NMPQuery` bridge pattern
// (M4 plan §4c) but for `ReceiptObserver`/`WriteStatus` instead of
// `RowObserver`/`RowDelta`.

import NMPFFI

/// The stream of every state a single `publish` call's write reaches
/// (ledger #9 -- enqueue is not converged). `status` finishes when the
/// engine has nothing further to report for this intent (e.g. an
/// `Ephemeral` intent may finish immediately after `.sent`, a `Durable` one
/// only after every relay has reached a terminal state or given up).
public struct Receipt: Sendable {
    public let id: UInt64
    public let status: AsyncStream<WriteStatus>
}

public enum ReceiptReattachment: Sendable {
    case attached(Receipt)
    case notFound
    case retainedButUnreadable
}

/// Drains a publish's status updates into an `AsyncStream`. Not exposed
/// publicly -- an implementation detail of `NMPEngine.publish`.
private final class ReceiptBridge: ReceiptObserver, @unchecked Sendable {
    private let continuation: AsyncStream<WriteStatus>.Continuation

    init(continuation: AsyncStream<WriteStatus>.Continuation) {
        self.continuation = continuation
    }

    func onStatus(status: FfiWriteStatus) {
        continuation.yield(WriteStatus(status))
    }

    func onClosed() {
        // The receipt `Sender` was dropped (intent resolved / engine shut
        // down) -- finish the stream so a consumer awaiting it is never left
        // hanging, mirroring `RowBridge`/`DiagnosticsBridge`.
        continuation.finish()
    }
}

func mapReceiptReattachment(
    _ result: FfiReceiptReattachment,
    id: UInt64,
    status: AsyncStream<WriteStatus>
) -> ReceiptReattachment {
    switch result {
    case .attached:
        return .attached(Receipt(id: id, status: status))
    case .notFound:
        return .notFound
    case .retainedButUnreadable:
        return .retainedButUnreadable
    }
}

extension NMPEngine {
    /// Enqueue a write. Returns as soon as the intent is accepted into the
    /// outbox; `Receipt.status` streams everything that happens to it after
    /// that (M4 plan §9 -- `publish` is a one-shot enqueue call, the
    /// STREAM is where convergence is observed).
    public func publish(_ intent: WriteIntent) async throws -> Receipt {
        var continuation: AsyncStream<WriteStatus>.Continuation!
        let stream = AsyncStream<WriteStatus> { continuation = $0 }
        let bridge = ReceiptBridge(continuation: continuation)
        let id = try nmpRethrowing {
            try ffi.publish(intent: intent.toFfi(), observer: bridge)
        }
        return Receipt(id: id, status: stream)
    }

    /// Publish a `GroupSendIntent` from `groupMessageIntent` (#156). Take-once:
    /// `intent` is consumed by this call -- a second `publishComposed` on
    /// the SAME `GroupSendIntent` throws `NMPError.intentAlreadyConsumed`
    /// rather than silently re-publishing a stale template (recompose via
    /// `groupMessageIntent` again for a retry). Otherwise identical to
    /// `publish(_:)`'s receipt-stream bridge.
    public func publishComposed(_ intent: GroupSendIntent) async throws -> Receipt {
        var continuation: AsyncStream<WriteStatus>.Continuation!
        let stream = AsyncStream<WriteStatus> { continuation = $0 }
        let bridge = ReceiptBridge(continuation: continuation)
        let id = try nmpRethrowing {
            try ffi.publishComposed(intent: intent.ffi, observer: bridge)
        }
        return Receipt(id: id, status: stream)
    }

    /// Attach a new observer to retained receipt facts. Corrupt durable
    /// evidence is reported distinctly and never treated as absence.
    public func reattachReceipt(id: UInt64) throws -> ReceiptReattachment {
        var continuation: AsyncStream<WriteStatus>.Continuation!
        let stream = AsyncStream<WriteStatus> { continuation = $0 }
        let bridge = ReceiptBridge(continuation: continuation)
        let result = try nmpRethrowing {
            try ffi.reattachReceipt(receiptId: id, observer: bridge)
        }
        if result != .attached {
            continuation.finish()
        }
        return mapReceiptReattachment(result, id: id, status: stream)
    }
}
