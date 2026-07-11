// The Rust-channel -> Swift-`AsyncSequence` bridge (M4 plan §4c). This is
// the heart of the ergonomic layer: the ONLY place `NMP` holds a foreign-
// trait conformance, and the ONLY place a callback thread's mutation
// touches shared state.

import Foundation
import NMPFFI

/// A live, detachable query (`nmp_engine`'s read noun). `NMPQuery` is the
/// PRIMARY read handle -- iterate it directly with `for await`; there is no
/// container or provider object required around it (M4 plan §7's canary).
///
/// Each element is the full accumulated snapshot (`RowBatch`), never a bare
/// delta: the bridge accumulates `Added`/`Removed` deltas internally so a
/// consumer never has to.
///
/// Demand teardown is DEINIT-TIED: once the last strong reference to the
/// underlying subscription handle is released (the query goes out of scope,
/// or its iteration `Task` is cancelled and drops its iterator), the Rust
/// side's `Drop` unsubscribes automatically. No `cancel()` call is required
/// of the app -- `NmpQueryHandle.cancel()` exists only for an app that wants
/// to tear down explicitly before Swift's own ARC would get to it.
public struct NMPQuery: AsyncSequence, Sendable {
    public typealias Element = RowBatch

    private let handle: NmpQueryHandle
    private let stream: AsyncStream<RowBatch>

    init(engine: NmpEngineProtocol, filter: FfiFilter) throws {
        var continuation: AsyncStream<RowBatch>.Continuation!
        let stream = AsyncStream<RowBatch> { continuation = $0 }
        let bridge = RowBridge(continuation: continuation)
        self.handle = try nmpRethrowing { try engine.observe(query: filter, observer: bridge) }
        self.stream = stream
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(handle: handle, base: stream.makeAsyncIterator())
    }

    public struct Iterator: AsyncIteratorProtocol {
        // Held for the iterator's lifetime so the subscription survives at
        // least as long as anything is actually consuming it; released (and
        // therefore unsubscribed) when the iterator itself is discarded.
        private let handle: NmpQueryHandle
        private var base: AsyncStream<RowBatch>.AsyncIterator

        init(handle: NmpQueryHandle, base: AsyncStream<RowBatch>.AsyncIterator) {
            self.handle = handle
            self.base = base
        }

        public mutating func next() async -> RowBatch? {
            await base.next()
        }
    }

    /// Withdraw the subscription now rather than waiting for the last
    /// reference to be released. Safe to call more than once; safe to never
    /// call at all.
    public func cancel() {
        handle.cancel()
    }
}

/// Drains a live subscription's row-delta batches into an `AsyncStream`
/// (M4 plan §4c). Not exposed publicly -- an implementation detail of
/// `NMPQuery`.
private final class RowBridge: RowObserver, @unchecked Sendable {
    private let continuation: AsyncStream<RowBatch>.Continuation
    private let lock = NSLock()
    // Insertion-ordered accumulation: `order` tracks arrival order, `byId`
    // the current value for each still-live row. NMP does mechanics only
    // (accumulate what the engine says is live) -- ordering/rendering policy
    // is an app concern (feed doctrine), not this bridge's.
    private var order: [String] = []
    private var byId: [String: Row] = [:]

    init(continuation: AsyncStream<RowBatch>.Continuation) {
        self.continuation = continuation
    }

    func onBatch(deltas: [FfiRowDelta], coverage: FfiCoverage) {
        lock.lock()
        for delta in deltas {
            switch delta {
            case .added(let ffiRow):
                let row = Row(ffiRow)
                if byId[row.id] == nil {
                    order.append(row.id)
                }
                byId[row.id] = row
            case .removed(let id):
                if byId.removeValue(forKey: id) != nil {
                    order.removeAll { $0 == id }
                }
            }
        }
        let snapshot = order.compactMap { byId[$0] }
        lock.unlock()
        continuation.yield(RowBatch(rows: snapshot, coverage: Coverage(coverage)))
    }

    func onClosed() {
        continuation.finish()
    }
}
