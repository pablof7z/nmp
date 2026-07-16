import Combine
import Foundation
import NMP
import NMPContent
import SwiftUI

/// One independently owned observation handle. Cancellation consumes the
/// closure exactly once; deinitialization is the final safety net.
public final class NMPReferenceObservationHandle: @unchecked Sendable {
    private let lock = NSLock()
    private var cancellation: (@Sendable () -> Void)?

    public init(cancellation: @escaping @Sendable () -> Void) {
        self.cancellation = cancellation
    }

    public func cancel() {
        lock.lock()
        let action = cancellation
        cancellation = nil
        lock.unlock()
        action?()
    }

    deinit {
        cancel()
    }
}

/// Injectable construction seam for an ordinary NMP observation. It owns no
/// shared state or cache: every call must return a fresh, independent handle.
public struct NMPReferenceObservationFactory: @unchecked Sendable {
    public typealias Receive = @MainActor @Sendable (RowBatch) -> Void
    public typealias Open = @MainActor @Sendable (
        NMPDemand,
        @escaping Receive
    ) throws -> NMPReferenceObservationHandle

    private let open: Open

    public init(open: @escaping Open) {
        self.open = open
    }

    /// Open one ordinary observation because the calling component explicitly
    /// chose resolution. Parsing, document walking, and visibility never call
    /// this method on their own.
    @MainActor
    public func observe(
        _ demand: NMPDemand,
        receive: @escaping Receive
    ) throws -> NMPReferenceObservationHandle {
        try open(demand, receive)
    }

    /// Production factory over the app's existing engine. The iteration task
    /// retains exactly one `NMPQuery`; cancelling the returned handle releases
    /// that query without affecting any equal handle another component owns.
    public static func live(engine: NMPEngine) -> NMPReferenceObservationFactory {
        NMPReferenceObservationFactory { demand, receive in
            let query = try engine.observe(demand)
            let task = Task { @MainActor in
                for await batch in query {
                    guard !Task.isCancelled else { break }
                    receive(batch)
                }
            }
            return NMPReferenceObservationHandle(cancellation: {
                task.cancel()
                query.cancel()
            })
        }
    }
}

/// Per-component observation state used by `observeWhileVisible`.
///
/// The latest canonical/helper batches survive hidden periods. The lifecycle
/// owns only this component's handles and has no process-global or document-
/// scoped coordinator.
@MainActor
public final class NMPVisibleReferenceObservation: ObservableObject {
    @Published public private(set) var canonical: RowBatch?
    @Published public private(set) var helpers: [RowBatch?]
    @Published public private(set) var failure: String?

    public let target: NostrReferenceTarget

    private let factory: NMPReferenceObservationFactory
    private let plan: NostrReferenceDemandPlan?
    private var nextGeneration: UInt64 = 0

    private enum Lifecycle {
        case hidden
        case opening(generation: UInt64)
        case visible(generation: UInt64, handles: [NMPReferenceObservationHandle])
    }

    private var lifecycle = Lifecycle.hidden

    public init(
        target: NostrReferenceTarget,
        factory: NMPReferenceObservationFactory
    ) {
        self.target = target
        self.factory = factory
        do {
            let plan = try referenceDemandPlan(for: target)
            self.plan = plan
            self.helpers = Array(repeating: nil, count: plan.helpers.count)
            self.failure = nil
        } catch {
            self.plan = nil
            self.helpers = []
            self.failure = String(describing: error)
        }
    }

    /// Called by the visibility primitive when this component becomes
    /// render-visible. Repeated calls while already opening/visible are inert.
    public func appear() {
        guard case .hidden = lifecycle, let plan else { return }

        nextGeneration &+= 1
        let generation = nextGeneration
        lifecycle = .opening(generation: generation)
        failure = nil
        var opened: [NMPReferenceObservationHandle] = []

        do {
            opened.append(
                try factory.observe(plan.canonical) { [weak self] batch in
                    guard let self, self.accepts(generation) else { return }
                    self.canonical = batch
                }
            )
            for (index, demand) in plan.helpers.enumerated() {
                opened.append(
                    try factory.observe(demand) { [weak self] batch in
                        guard let self, self.accepts(generation) else { return }
                        self.helpers[index] = batch
                    }
                )
            }
            lifecycle = .visible(generation: generation, handles: opened)
        } catch {
            lifecycle = .hidden
            opened.forEach { $0.cancel() }
            failure = String(describing: error)
        }
    }

    /// Releases only this component's handles. The last delivered batches are
    /// intentionally retained, so scroll-away/return does not flash empty.
    public func disappear() {
        guard case .visible(_, let handles) = lifecycle else {
            lifecycle = .hidden
            return
        }
        lifecycle = .hidden
        handles.forEach { $0.cancel() }
    }

    private func accepts(_ generation: UInt64) -> Bool {
        switch lifecycle {
        case .hidden:
            return false
        case .opening(let active), .visible(let active, _):
            return active == generation
        }
    }
}

private struct NMPObserveWhileVisibleModifier: ViewModifier {
    @ObservedObject var observation: NMPVisibleReferenceObservation

    @ViewBuilder
    func body(content: Content) -> some View {
#if compiler(>=6.0)
        if #available(iOS 18.0, macOS 15.0, *) {
            content
                .onAppear { observation.appear() }
                .onScrollVisibilityChange(threshold: 0.01) { visible in
                    if visible {
                        observation.appear()
                    } else {
                        observation.disappear()
                    }
                }
                .onDisappear { observation.disappear() }
        } else {
            content
                .onAppear { observation.appear() }
                .onDisappear { observation.disappear() }
        }
#else
        content
            .onAppear { observation.appear() }
            .onDisappear { observation.disappear() }
#endif
    }
}

public extension View {
    /// Opt-in visibility scoping for a component-owned reference observation.
    /// Custom components may use this helper, observe unconditionally, or not
    /// observe at all.
    func observeWhileVisible(
        _ observation: NMPVisibleReferenceObservation
    ) -> some View {
        modifier(NMPObserveWhileVisibleModifier(observation: observation))
    }
}
