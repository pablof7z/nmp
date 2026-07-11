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
    public let status: AsyncStream<WriteStatus>
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
        try nmpRethrowing {
            try ffi.publish(intent: intent.toFfi(), observer: bridge)
        }
        return Receipt(status: stream)
    }
}
