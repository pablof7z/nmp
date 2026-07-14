import Foundation
import NMPFFI

/// Mechanical state of the most recent history advance. This is deliberately
/// separate from acquisition evidence and never claims the network is
/// complete or that no older event exists.
public enum HistoryLoadFact: Sendable, Equatable {
    case idle
    case requesting
    case returned(added: UInt64)
    case atBound(maxRows: UInt64)

    init(_ ffi: FfiHistoryLoadFact) {
        switch ffi {
        case .idle: self = .idle
        case .requesting: self = .requesting
        case .returned(let added): self = .returned(added: added)
        case .atBound(let maxRows): self = .atBound(maxRows: maxRows)
        }
    }
}

/// Opaque process-local continuation for one exact history session and
/// generation. It has no public initializer or fields; callers can only
/// return a continuation NMP delivered.
public struct NMPHistoryContinuation: @unchecked Sendable {
    fileprivate let ffi: NmpHistoryContinuation

    init(_ ffi: NmpHistoryContinuation) {
        self.ffi = ffi
    }
}

/// One self-contained bounded history state. `rows` is the authoritative
/// canonical selection order (`createdAt DESC, id ASC`). Apps may reverse or
/// otherwise present it, but never reconstruct acquisition order or cursors.
public struct HistoryBatch: Sendable {
    public let rows: [Row]
    public let continuation: NMPHistoryContinuation?
    public let evidence: AcquisitionEvidence
    public let load: HistoryLoadFact
}

/// Typed failures from `NMPHistoryQuery.loadOlder(using:)`. Continuation
/// misuse, local-store failure, and acquisition failure remain distinct.
public enum NMPHistoryLoadError: Error, Sendable, Equatable {
    case wrongVersion
    case wrongEngine
    case wrongSession
    case wrongDescriptor
    case staleGeneration
    case loadInProgress
    case atBound(maxRows: UInt64)
    case noBoundary
    case storeUnavailable
    case transportUnavailable(reason: String)

    init(_ ffi: FfiHistoryLoadError) {
        switch ffi {
        case .WrongVersion: self = .wrongVersion
        case .WrongEngine: self = .wrongEngine
        case .WrongSession: self = .wrongSession
        case .WrongDescriptor: self = .wrongDescriptor
        case .StaleGeneration: self = .staleGeneration
        case .LoadInProgress: self = .loadInProgress
        case .AtBound(let maxRows): self = .atBound(maxRows: maxRows)
        case .NoBoundary: self = .noBoundary
        case .StoreUnavailable: self = .storeUnavailable
        case .TransportUnavailable(let reason):
            self = .transportUnavailable(reason: reason)
        }
    }
}

/// One coordinated bounded-history session. Iterate it directly with
/// `for await`; the sequence always buffers at most the newest full state.
/// Dropping or cancelling the query withdraws every engine-owned acquisition
/// handle opened for the session.
public struct NMPHistoryQuery: AsyncSequence, Sendable {
    public typealias Element = HistoryBatch

    private let handle: NmpHistoryHandle
    private let stream: AsyncStream<HistoryBatch>

    init(
        engine: NmpEngineProtocol,
        demand: FfiDemand,
        pageSize: UInt64,
        maxRows: UInt64
    ) throws {
        var continuation: AsyncStream<HistoryBatch>.Continuation!
        let stream = AsyncStream<HistoryBatch>(bufferingPolicy: .bufferingNewest(1)) {
            continuation = $0
        }
        let bridge = HistoryBridge(continuation: continuation, maxRows: maxRows)
        self.handle = try nmpRethrowing {
            try engine.observeHistory(
                query: FfiHistoryQuery(
                    demand: demand,
                    pageSize: pageSize,
                    maxRows: maxRows
                ),
                observer: bridge
            )
        }
        self.stream = stream
    }

    public func makeAsyncIterator() -> Iterator {
        Iterator(handle: handle, base: stream.makeAsyncIterator())
    }

    public struct Iterator: AsyncIteratorProtocol {
        private let handle: NmpHistoryHandle
        private var base: AsyncStream<HistoryBatch>.AsyncIterator

        init(handle: NmpHistoryHandle, base: AsyncStream<HistoryBatch>.AsyncIterator) {
            self.handle = handle
            self.base = base
        }

        public mutating func next() async -> HistoryBatch? {
            await base.next()
        }
    }

    /// Advance this exact session using only the latest continuation it
    /// issued. Wrong-session/engine/generation use fails with a typed error.
    public func loadOlder(using continuation: NMPHistoryContinuation) throws {
        do {
            try handle.loadOlder(continuation: continuation.ffi)
        } catch let error as FfiHistoryLoadError {
            throw NMPHistoryLoadError(error)
        }
    }

    /// Withdraw the entire session now. Idempotent and optional; ARC teardown
    /// invokes the same engine-side cancellation guard.
    public func cancel() {
        handle.cancel()
    }
}

/// Folds every receiver-relative delta synchronously, validates it against
/// the same frame's authoritative rows, then retains only that bounded full
/// state. Delivery is latest-wins twice: `FrameCoalescer` drops intermediate
/// render frames and `AsyncStream.bufferingNewest(1)` bounds a slow consumer.
final class HistoryBridge: HistoryObserver, @unchecked Sendable {
    private let continuation: AsyncStream<HistoryBatch>.Continuation
    private let maxRows: UInt64
    private let lock = NSLock()
    private var priorByID: [String: Row] = [:]
    private lazy var coalescer = FrameCoalescer<HistoryBatch> {
        [continuation = self.continuation] batch in
        continuation.yield(batch)
    }

    init(continuation: AsyncStream<HistoryBatch>.Continuation, maxRows: UInt64) {
        self.continuation = continuation
        self.maxRows = maxRows
    }

    func onBatch(batch: FfiHistoryBatch) {
        let authoritative = batch.rows.map(Row.init)
        precondition(
            UInt64(authoritative.count) <= maxRows,
            "Rust history frame exceeded its declared maxRows bound"
        )

        lock.lock()
        var reduced = priorByID
        for delta in batch.deltas {
            switch delta {
            case .added(let ffiRow):
                let row = Row(ffiRow)
                reduced[row.id] = row
            case .sourcesGrew(let id, let sources):
                if let existing = reduced[id] {
                    reduced[id] = existing.withSources(sources)
                }
            case .removed(let id):
                reduced.removeValue(forKey: id)
            }
        }
        let authoritativeByID = Dictionary(
            uniqueKeysWithValues: authoritative.map { ($0.id, $0) }
        )
        assert(
            reduced == authoritativeByID,
            "history deltas must describe the authoritative full frame"
        )
        priorByID = authoritativeByID
        lock.unlock()

        coalescer.push(
            HistoryBatch(
                rows: authoritative,
                continuation: batch.continuation.map(NMPHistoryContinuation.init),
                evidence: AcquisitionEvidence(batch.evidence),
                load: HistoryLoadFact(batch.load)
            )
        )
    }

    func onClosed() {
        coalescer.flushNow()
        continuation.finish()
    }
}
