// The pull-based Rust-handle -> Swift-`AsyncSequence` adapter (#680). This is
// the ONLY place `NMP` folds observation frames into delivered state.

import Foundation
import NMPFFI

/// A live, detachable query (`nmp_engine`'s read noun). `NMPQuery` is the
/// PRIMARY read handle -- iterate it directly with `for try await`; there is
/// no container or provider object required around it (M4 plan §7's canary).
///
/// Each element is the full current snapshot (`RowBatch`), never a bare
/// delta. How that snapshot is produced derives from the observation's
/// boundedness (#485): an UNBOUNDED query is delivered as exact rebased
/// deltas that the iterator folds into its accumulated state (the engine
/// mailbox conflates intermediate reducer emits for a slow consumer); a
/// WINDOWED query is delivered as authoritative bounded snapshots that
/// replace the state wholesale, each carrying the window's `WindowLoad` fact.
///
/// The sequence is THROWING: it ends normally (`nil`) when the engine tears
/// the subscription down, and surfaces `NMPError.concurrentNext` if two
/// iterators pull the same handle at once (the handle is single-consumer,
/// #680). Delivery is throttled through `FrameCoalescer` (#17) so a tight
/// `for try await` loop during historical replay cannot peg the main thread.
///
/// Demand teardown is TERMINATION-TIED: dropping the iterating task (cancel /
/// scope exit) drops the iterator, whose stream termination calls the handle's
/// `cancel()` -- required because Swift task cancellation never reaches Rust
/// (#680). `NmpRowStream.cancel()` also runs on the handle's own ARC `deinit`.
public struct NMPQuery: AsyncSequence, Sendable {
    public typealias Element = RowBatch

    private let handle: NmpRowStream

    init(engine: NmpEngineProtocol, filter: FfiFilter, window: FfiWindow?) throws {
        self.handle = try nmpRethrowing {
            try engine.observe(query: filter, window: window)
        }
    }

    /// #107: the explicit-`FfiDemand` entry point -- same handle/coalescing
    /// shape as the `FfiFilter` initializer above, just a different
    /// `NmpEngineProtocol` verb underneath.
    init(engine: NmpEngineProtocol, demand: FfiDemand, window: FfiWindow?) throws {
        self.handle = try nmpRethrowing {
            try engine.observeDemand(query: demand, window: window)
        }
    }

    public func makeAsyncIterator() -> Iterator {
        let accumulator = RowAccumulator()
        let stream = nmpPullStream(
            handle: handle,
            bufferingPolicy: .bufferingNewest(1),
            throttle: true
        ) { frame in accumulator.fold(frame) }
        return Iterator(base: stream.makeAsyncIterator())
    }

    public struct Iterator: AsyncIteratorProtocol {
        var base: AsyncThrowingStream<RowBatch, Error>.AsyncIterator

        public mutating func next() async throws -> RowBatch? {
            try await base.next()
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

/// The unbounded/windowed delta-fold accumulator, moved out of the deleted
/// `RowObserver` bridge and into the iterator's per-frame mapping (#680).
/// `internal` (not `private`) so `@testable import NMP` can drive `fold`
/// directly for the accumulation/replacement falsifiers (#105's `SourcesGrew`
/// replace-in-place proof; #485's windowed-snapshot replacement proof).
///
/// ONE fold, two frame shapes, chosen by the engine from the observation's
/// boundedness (#485):
///
/// - `frame.window == nil` (unbounded): `frame.deltas` is the exact
///   transition rebased onto the last delivered Rust frame. Folding every
///   delivered transition keeps the accumulated state exact.
/// - `frame.window != nil` (windowed): `frame.window!.rows` is the complete
///   authoritative bounded set and REPLACES the state wholesale -- windowed
///   frames conflate to latest-state on the Rust side, so `frame.deltas` is
///   always empty here.
///
/// Only ever touched from the single pump task that owns its stream, so no
/// lock is needed (the old bridge locked because a callback thread raced the
/// consumer; there is no callback thread anymore).
final class RowAccumulator: @unchecked Sendable {
    // Insertion-ordered accumulation for the unbounded mode: `order` tracks
    // arrival order, `byId` the current value for each still-live row. For
    // the windowed mode both are replaced from each authoritative frame
    // (canonical newest-first order). NMP does mechanics only (retain what the
    // engine says is live) -- ordering/rendering policy is an app concern.
    private var order: [String] = []
    private var byId: [String: Row] = [:]

    func fold(_ frame: FfiFrame) -> RowBatch {
        if let window = frame.window {
            // #485: an authoritative bounded snapshot -- replace, never fold.
            // `frame.deltas` is empty by contract for windowed frames (rows
            // never cross the FFI twice).
            let rows = window.rows.map(Row.init)
            order = rows.map(\.id)
            byId = Dictionary(uniqueKeysWithValues: rows.map { ($0.id, $0) })
            return RowBatch(
                rows: rows,
                evidence: AcquisitionEvidence(frame.evidence),
                load: WindowLoad(window.load)
            )
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
                // #105: the SAME row already matched; only its relay-provenance
                // set grew. Replace it in place -- `order` is untouched, this is
                // never an insertion.
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
        return RowBatch(
            rows: snapshot,
            evidence: AcquisitionEvidence(frame.evidence),
            load: nil
        )
    }
}
