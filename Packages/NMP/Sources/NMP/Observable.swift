// SwiftUI convenience sugar ON TOP of `NMPQuery` -- NOT the primary API
// (M4 plan §9). `NMPQuery` itself (the `AsyncSequence`) is what a view's own
// `.task { for await ... }` should iterate directly (the §7 canary); this
// class exists purely for call sites that would rather bind a view straight
// to an `@Observable` object instead of managing their own `@State` array.

import Observation

@available(iOS 17.0, macOS 14.0, *)
@Observable
public final class NMPQuerySnapshot {
    public private(set) var rows: [Row] = []
    public private(set) var coverage: Coverage = .unknown

    private var consumeTask: Task<Void, Never>?

    public init(_ query: NMPQuery) {
        consumeTask = Task { [weak self] in
            for await batch in query {
                guard !Task.isCancelled else { return }
                self?.rows = batch.rows
                self?.coverage = batch.coverage
            }
        }
    }

    deinit {
        consumeTask?.cancel()
    }
}
