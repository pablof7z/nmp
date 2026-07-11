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
        // `.bufferingNewest(1)`: belt-and-suspenders alongside `RowBridge`'s
        // own frame coalescing below -- if a consumer ever falls behind the
        // ~60Hz delivery cadence, only the latest coalesced snapshot sits in
        // the stream's buffer, so the backlog can never grow (#17).
        let stream = AsyncStream<RowBatch>(bufferingPolicy: .bufferingNewest(1)) {
            continuation = $0
        }
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
///
/// Accumulation (deltas -> the current live snapshot) happens synchronously
/// on every `onBatch` call, so no delta is ever missed. DELIVERY into the
/// stream is coalesced through `FrameCoalescer` (#17/docs/known-gaps.md):
/// during historical replay `onBatch` can fire far faster than any consumer
/// can re-render, so only the latest accumulated snapshot is actually
/// yielded, at most once per ~60Hz tick -- the accumulated state itself is
/// always fully caught up, only the *delivery* of intermediate states is
/// dropped.
private final class RowBridge: RowObserver, @unchecked Sendable {
    private let continuation: AsyncStream<RowBatch>.Continuation
    private let lock = NSLock()
    // Insertion-ordered accumulation: `order` tracks arrival order, `byId`
    // the current value for each still-live row. NMP does mechanics only
    // (accumulate what the engine says is live) -- ordering/rendering policy
    // is an app concern (feed doctrine), not this bridge's.
    private var order: [String] = []
    private var byId: [String: Row] = [:]
    // Captures `continuation` only (not `self`) -- avoids a retain cycle
    // between this bridge and its own coalescer.
    private lazy var coalescer = FrameCoalescer<RowBatch> { [continuation = self.continuation] batch in
        continuation.yield(batch)
    }

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
        coalescer.push(RowBatch(rows: snapshot, coverage: Coverage(coverage)))
    }

    func onClosed() {
        // Deliver whatever accumulated state is still pending before
        // finishing, so a burst that lands right as the subscription closes
        // is never silently dropped.
        coalescer.flushNow()
        continuation.finish()
    }
}
