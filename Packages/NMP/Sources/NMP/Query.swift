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
/// Each element is the full current snapshot (`RowBatch`), never a bare
/// delta. How that snapshot is produced derives from the observation's
/// boundedness (#485): an UNBOUNDED query is delivered as a lossless delta
/// stream that the bridge folds into its accumulated state (redelivering
/// the full set per change is the known O(rows²) class); a WINDOWED query
/// is delivered as authoritative bounded snapshots that replace the state
/// wholesale, each carrying the window's `WindowLoad` growth fact.
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

    init(engine: NmpEngineProtocol, filter: FfiFilter, window: FfiWindow?) throws {
        try self.init(engine: engine) { engine, observer in
            try engine.observe(query: filter, window: window, observer: observer)
        }
    }

    /// #107: the explicit-`FfiDemand` entry point -- same bridge/coalescing
    /// shape as the `FfiFilter` initializer above, just a different
    /// `NmpEngineProtocol` verb underneath.
    init(engine: NmpEngineProtocol, demand: FfiDemand, window: FfiWindow?) throws {
        try self.init(engine: engine) { engine, observer in
            try engine.observeDemand(query: demand, window: window, observer: observer)
        }
    }

    /// Shared bridge/coalescing setup: `subscribe` is the ONE difference
    /// between the `FfiFilter` and `FfiDemand` entry points (which
    /// `NmpEngineProtocol` verb actually opens the subscription).
    private init(
        engine: NmpEngineProtocol,
        subscribe: (NmpEngineProtocol, RowObserver) throws -> NmpQueryHandle
    ) throws {
        var continuation: AsyncStream<RowBatch>.Continuation!
        // `.bufferingNewest(1)`: belt-and-suspenders alongside `RowBridge`'s
        // own frame coalescing below -- if a consumer ever falls behind the
        // ~60Hz delivery cadence, only the latest coalesced snapshot sits in
        // the stream's buffer, so the backlog can never grow (#17).
        let stream = AsyncStream<RowBatch>(bufferingPolicy: .bufferingNewest(1)) {
            continuation = $0
        }
        let bridge = RowBridge(continuation: continuation)
        self.handle = try nmpRethrowing { try subscribe(engine, bridge) }
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

    /// Windowed observations only: monotonically raise this query's window
    /// row target to at least `atLeast`, clamped to the window's declared
    /// `max`. Growth is DECLARATIVE by design -- no continuation token to
    /// round-trip, so there is nothing to go stale and nothing to misuse;
    /// the call is idempotent, and a value at or below the current target
    /// is simply a no-op. Outcomes arrive in-band as `WindowLoad` facts on
    /// delivered batches (`RowBatch.load`) -- including `.atBound(max:)`,
    /// which is a delivered fact, never a thrown error.
    ///
    /// Throws only the synchronous refusals: `NMPRequestRowsError`
    /// (`.unwindowed` on a query opened without a window, `.engineClosed`,
    /// `.storeUnavailable`, `.transportUnavailable(reason:)`).
    public func requestRows(atLeast: UInt64) throws {
        do {
            try handle.requestRows(atLeast: atLeast)
        } catch let error as FfiRequestRowsError {
            throw NMPRequestRowsError(error)
        }
    }

    /// Withdraw the subscription now rather than waiting for the last
    /// reference to be released. Safe to call more than once; safe to never
    /// call at all.
    public func cancel() {
        handle.cancel()
    }
}

/// Drains a live subscription's frames into an `AsyncStream` (M4 plan §4c).
/// Not part of the module's PUBLIC API -- an implementation detail of
/// `NMPQuery`. `internal` (not `private`) only so `@testable import NMP`
/// can drive `onFrame` directly for the accumulation/replacement falsifiers
/// (#105's `SourcesGrew` replace-in-place proof; #485's windowed-snapshot
/// replacement proof); no other consumer outside this package can ever see
/// it.
///
/// ONE observer, two frame shapes, chosen by the engine from the
/// observation's boundedness (#485):
///
/// - `frame.window == nil` (unbounded): `frame.deltas` is the exact
///   lossless transition; it is folded synchronously into the accumulated
///   state on every `onFrame` call, so no delta is ever missed.
/// - `frame.window != nil` (windowed): `frame.window!.rows` is the complete
///   authoritative bounded set and REPLACES the state wholesale -- windowed
///   frames conflate to latest-state on the Rust side, so no per-frame
///   delta stream exists to fold, and the wire deliberately ships each row
///   once (`frame.deltas` is always empty here).
///
/// DELIVERY into the stream is coalesced through `FrameCoalescer`
/// (#17/docs/known-gaps.md): during historical replay `onFrame` can fire
/// far faster than any consumer can re-render, so only the latest snapshot
/// is actually yielded, at most once per ~60Hz tick -- the retained state
/// itself is always fully caught up, only the *delivery* of intermediate
/// states is dropped.
final class RowBridge: RowObserver, @unchecked Sendable {
    private let continuation: AsyncStream<RowBatch>.Continuation
    private let lock = NSLock()
    // Insertion-ordered accumulation for the unbounded mode: `order` tracks
    // arrival order, `byId` the current value for each still-live row. For
    // the windowed mode both are simply replaced from each authoritative
    // frame (canonical newest-first order). NMP does mechanics only
    // (retain what the engine says is live) -- ordering/rendering policy
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

    func onFrame(frame: FfiFrame) {
        lock.lock()
        if let window = frame.window {
            // #485: an authoritative bounded snapshot -- replace, never
            // fold. `frame.deltas` is empty by contract for windowed
            // frames (rows never cross the FFI twice).
            let rows = window.rows.map(Row.init)
            order = rows.map(\.id)
            byId = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0) })
            lock.unlock()
            coalescer.push(
                RowBatch(
                    rows: rows,
                    evidence: AcquisitionEvidence(frame.evidence),
                    load: WindowLoad(window.load)
                )
            )
            return
        }
        for delta in frame.deltas {
            switch delta {
            case .added(let ffiRow):
                let row = Row(ffiRow)
                if byId[row.id] == nil {
                    order.append(row.id)
                }
                byId[row.id] = row
            case .sourcesGrew(let id, let sources):
                // #105: the SAME row already matched; only its
                // relay-provenance set grew. Replace it in place -- `order`
                // is untouched, this is never an insertion.
                if let existing = byId[id] {
                    byId[id] = existing.withSources(sources)
                }
            case .removed(let id):
                if byId.removeValue(forKey: id) != nil {
                    order.removeAll { $0 == id }
                }
            }
        }
        let snapshot = order.compactMap { byId[$0] }
        lock.unlock()
        coalescer.push(
            RowBatch(
                rows: snapshot,
                evidence: AcquisitionEvidence(frame.evidence),
                load: nil
            )
        )
    }

    func onClosed() {
        // Deliver whatever accumulated state is still pending before
        // finishing, so a burst that lands right as the subscription closes
        // is never silently dropped.
        coalescer.flushNow()
        continuation.finish()
    }
}
