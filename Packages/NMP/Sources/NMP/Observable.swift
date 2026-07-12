// SwiftUI convenience sugar ON TOP of `NMPQuery` -- NOT the primary API
// (M4 plan §9). `NMPQuery` itself (the `AsyncSequence`) is what a view's own
// `.task { for await ... }` should iterate directly (the §7 canary); this
// class exists purely for call sites that would rather bind a view straight
// to an `@Observable` object instead of managing their own `@State` array.

import Observation

@available(iOS 17.0, macOS 14.0, *)
@MainActor
@Observable
public final class NMPQuerySnapshot {
    public private(set) var rows: [Row] = []
    /// `nil` until the first real query batch arrives. An empty evidence
    /// value is not manufactured here: the engine always reports an explicit
    /// shortfall when no source/demand fact exists.
    public private(set) var evidence: AcquisitionEvidence?

    // Not observable UI state (hence `@ObservationIgnored`), and
    // `nonisolated(unsafe)` so the (nonisolated) `deinit` can cancel it: the
    // write (in `init`, on the main actor) and the read (in `deinit`, which
    // only runs once the last reference is gone) can never overlap, and
    // `Task.cancel()` is itself thread-safe -- so the opt-out is sound.
    @ObservationIgnored private nonisolated(unsafe) var consumeTask: Task<Void, Never>?

    public init(_ query: NMPQuery) {
        consumeTask = Task { [weak self] in
            for await batch in query {
                guard !Task.isCancelled else { return }
                self?.rows = batch.rows
                self?.evidence = batch.evidence
            }
        }
    }

    deinit {
        consumeTask?.cancel()
    }
}

/// `@Observable` sugar ON TOP of `NMPDiagnostics`, mirroring
/// `NMPQuerySnapshot` exactly (M5 plan §1.3) -- for a DiagnosticsView that
/// would rather bind straight to an `@Observable` object than manage its own
/// `@State`.
@available(iOS 17.0, macOS 14.0, *)
@MainActor
@Observable
public final class NMPDiagnosticsSnapshotObserver {
    public private(set) var snapshot: DiagnosticsSnapshot = DiagnosticsSnapshot()

    // See `NMPQuerySnapshot.consumeTask` for the `@ObservationIgnored` +
    // `nonisolated(unsafe)` rationale.
    @ObservationIgnored private nonisolated(unsafe) var consumeTask: Task<Void, Never>?

    public init(_ diagnostics: NMPDiagnostics) {
        consumeTask = Task { [weak self] in
            for await snapshot in diagnostics {
                guard !Task.isCancelled else { return }
                self?.snapshot = snapshot
            }
        }
    }

    deinit {
        consumeTask?.cancel()
    }
}
