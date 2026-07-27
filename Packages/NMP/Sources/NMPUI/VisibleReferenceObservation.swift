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

/// App-selected construction seam for an ordinary NMP observation.
///
/// The app owns locator-to-demand policy inside this seam. NMPUI passes the
/// exact authored locator and never chooses acquisition by inferring kind:0,
/// source authority, cache, freshness, relay admission, or hidden helper
/// observations.
public struct NMPReferenceObservationFactory: @unchecked Sendable {
    public typealias Receive = @MainActor @Sendable (RowBatch) -> Void
    public typealias Open = @MainActor @Sendable (
        NostrReferenceTarget,
        @escaping Receive
    ) throws -> NMPReferenceObservationHandle
    public typealias Resolve = @Sendable (NostrReferenceTarget) throws -> NMPDemand

    private let open: Open

    public init(open: @escaping Open) {
        self.open = open
    }

    /// Open one ordinary observation because the calling component explicitly
    /// chose resolution. Parsing, document walking, and visibility never call
    /// this method on their own.
    @MainActor
    public func observe(
        _ target: NostrReferenceTarget,
        receive: @escaping Receive
    ) throws -> NMPReferenceObservationHandle {
        try open(target, receive)
    }

    /// Production factory over the app's existing engine and explicit
    /// locator-to-demand policy. The resolver runs once for each selected
    /// component; decoding or parsing alone never invokes it.
    public static func live(
        engine: NMPEngine,
        resolve: @escaping Resolve
    ) -> NMPReferenceObservationFactory {
        NMPReferenceObservationFactory { target, receive in
            let demand = try resolve(target)
            let query = try engine.observe(demand)
            let task = Task { @MainActor in
                // #680: an observation is a throwing `AsyncSequence` (its
                // `next()` surfaces the single-consumer misuse error). This is
                // the sole consumer, so a throw here is terminal teardown, not
                // misuse — end the iteration quietly.
                do {
                    for try await batch in query {
                        guard !Task.isCancelled else { break }
                        receive(batch)
                    }
                } catch {}
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
/// The last batch survives hidden periods. The lifecycle owns only this
/// component's one ordinary observation and has no process-global or
/// document-scoped coordinator.
@MainActor
public final class NMPVisibleReferenceObservation: ObservableObject {
    @Published public private(set) var latest: RowBatch?
    @Published public private(set) var failure: String?

    public let target: NostrReferenceTarget

    private let factory: NMPReferenceObservationFactory
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
        self.failure = nil
    }

    /// Called by the visibility primitive when this component becomes
    /// render-visible. Repeated calls while already opening/visible are inert.
    public func appear() {
        guard case .hidden = lifecycle else { return }

        nextGeneration &+= 1
        let generation = nextGeneration
        lifecycle = .opening(generation: generation)
        failure = nil
        do {
            let handle = try factory.observe(target) { [weak self] batch in
                    guard let self, self.accepts(generation) else { return }
                    self.latest = batch
                }
            lifecycle = .visible(generation: generation, handles: [handle])
        } catch {
            lifecycle = .hidden
            failure = String(describing: error)
        }
    }

    /// Releases only this component's handle. The last delivered batch is
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
