// The pull-based diagnostics `AsyncSequence` (#680), mirroring `NMPQuery`.
// Each `next()` awaits `NmpDiagnosticsStream.next()`; there is no observer
// conformance and no drain thread anymore.

import Foundation
import NMPFFI

/// A live diagnostics stream (`nmp_engine`'s read-only diagnostic
/// projection). Iterate directly with `for try await`; there is no container
/// or provider object required around it, same discipline as `NMPQuery`.
///
/// Each element is the CURRENT engine-global `DiagnosticsSnapshot` -- never a
/// delta (every snapshot is already the full current picture). Delivery is
/// demand-driven and cadence-limited (#17), same as `NMPQuery`: a slow
/// consumer adds no Swift queue and the native mailbox keeps only the newest
/// complete snapshot.
///
/// The sequence is THROWING (ends `nil` on withdrawal; surfaces
/// `NMPError.concurrentNext` if two iterators pull one handle). The
/// reference-owned iterator cancels the Rust handle on normal loop exit, and
/// explicitly forwards Swift task cancellation to Rust (#680).
public struct NMPDiagnostics: AsyncSequence, Sendable {
    public typealias Element = DiagnosticsSnapshot

    private let handle: NmpDiagnosticsStream
    private let iteratorGate = NMPPullIteratorGate()

    init(engine: NmpEngineProtocol) throws {
        self.handle = try nmpRethrowing { try engine.observeDiagnostics() }
    }

    public func makeAsyncIterator() -> Iterator {
        let core = NMPPullIteratorCore(
            handle: handle,
            iteratorGate: iteratorGate,
            throttle: true
        ) { snapshot in DiagnosticsSnapshot(snapshot) }
        return Iterator(core: core)
    }

    public struct Iterator: AsyncIteratorProtocol {
        let core: NMPPullIteratorCore<NmpDiagnosticsStream, DiagnosticsSnapshot>

        public mutating func next() async throws -> DiagnosticsSnapshot? {
            try await core.next()
        }
    }

    /// Withdraw the diagnostics observer now rather than waiting for the
    /// last reference to be released. Safe to call more than once; safe to
    /// never call at all.
    public func cancel() {
        handle.cancel()
    }
}
