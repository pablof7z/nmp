// The Rust-channel -> Swift-`AsyncSequence` bridge for the diagnostic
// surface (M5 plan §1.3), mirroring `Query.swift`'s `NMPQuery`/`RowBridge`
// exactly. This is the ONLY place `NMP` holds a `DiagnosticsObserver`
// foreign-trait conformance.

import Foundation
import NMPFFI

/// A live diagnostics stream (`nmp_engine`'s read-only diagnostic
/// projection). Iterate directly with `for await`; there is no container or
/// provider object required around it, same discipline as `NMPQuery`.
///
/// Each element is the CURRENT engine-global `DiagnosticsSnapshot` -- never a
/// delta (there is nothing to accumulate here: every snapshot is already the
/// full current picture).
///
/// Demand teardown is DEINIT-TIED, same as `NMPQuery`: once the last strong
/// reference to the underlying observer handle is released, the Rust side's
/// `Drop` withdraws the observer automatically. No `cancel()` call is
/// required of the app.
public struct NMPDiagnostics: AsyncSequence, Sendable {
    public typealias Element = DiagnosticsSnapshot

    private let handle: NmpDiagnosticsHandle
    private let stream: AsyncStream<DiagnosticsSnapshot>

    init(engine: NmpEngineProtocol) {
        var continuation: AsyncStream<DiagnosticsSnapshot>.Continuation!
        let stream = AsyncStream<DiagnosticsSnapshot> { continuation = $0 }
        let bridge = DiagnosticsBridge(continuation: continuation)
        self.handle = engine.observeDiagnostics(observer: bridge)
        self.stream = stream
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(handle: handle, base: stream.makeAsyncIterator())
    }

    public struct Iterator: AsyncIteratorProtocol {
        // Held for the iterator's lifetime so the observer survives at least
        // as long as anything is actually consuming it; released (and
        // therefore withdrawn) when the iterator itself is discarded.
        private let handle: NmpDiagnosticsHandle
        private var base: AsyncStream<DiagnosticsSnapshot>.AsyncIterator

        init(handle: NmpDiagnosticsHandle, base: AsyncStream<DiagnosticsSnapshot>.AsyncIterator) {
            self.handle = handle
            self.base = base
        }

        public mutating func next() async -> DiagnosticsSnapshot? {
            await base.next()
        }
    }

    /// Withdraw the diagnostics observer now rather than waiting for the
    /// last reference to be released. Safe to call more than once; safe to
    /// never call at all.
    public func cancel() {
        handle.cancel()
    }
}

/// Drains a live diagnostics stream into an `AsyncStream` (M5 plan §1.3).
/// Not exposed publicly -- an implementation detail of `NMPDiagnostics`.
private final class DiagnosticsBridge: DiagnosticsObserver, @unchecked Sendable {
    private let continuation: AsyncStream<DiagnosticsSnapshot>.Continuation

    init(continuation: AsyncStream<DiagnosticsSnapshot>.Continuation) {
        self.continuation = continuation
    }

    func onSnapshot(snapshot: FfiDiagnosticsSnapshot) {
        continuation.yield(DiagnosticsSnapshot(snapshot))
    }

    func onClosed() {
        continuation.finish()
    }
}
