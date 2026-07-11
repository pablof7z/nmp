// M5 replay-jank fix, Swift-delivery half (#17; docs/known-gaps.md's
// "Unbounded historical replay can peg the main thread" entry). Historical
// replay can push dozens of deltas through `RowBridge`/`DiagnosticsBridge`
// faster than any `for await` consumer can re-render. Delivering one full
// snapshot per delta forces the run loop through that many consecutive
// re-renders. `FrameCoalescer` collapses a burst arriving faster than
// `interval` into a single delivery of the LATEST value -- intermediate
// values are dropped, never queued, so a slow consumer's backlog cannot
// grow. This is purely an internal Swift-side delivery-cadence change: it
// does not touch the FFI surface or either bridge's public shape.

import Foundation

/// Coalesces a burst of rapid `push` calls into at most one `deliver` call
/// per `interval` (default ~16ms, i.e. no faster than one run-loop tick at
/// 60Hz). Always delivers the most recently pushed value once the window
/// closes -- correctness (eventual/final state) is preserved; only
/// intermediate deliveries are dropped, exactly the same shape as
/// `AsyncStream`'s own `.bufferingNewest(1)` policy, applied at the
/// producer instead of the consumer's buffer.
///
/// Thread-safe: `push` is called from the FFI callback thread (`RowBridge`/
/// `DiagnosticsBridge`'s `RowObserver`/`DiagnosticsObserver` conformance),
/// concurrently with `flushNow` from wherever `onClosed` fires.
final class FrameCoalescer<Value: Sendable>: @unchecked Sendable {
    private let interval: Duration
    private let deliver: @Sendable (Value) -> Void
    private let clock: ContinuousClock

    private let lock = NSLock()
    private var pending: Value?
    private var scheduled = false
    private var lastDeliveryTime: ContinuousClock.Instant?

    init(
        interval: Duration = .milliseconds(16),
        clock: ContinuousClock = ContinuousClock(),
        deliver: @escaping @Sendable (Value) -> Void
    ) {
        self.interval = interval
        self.clock = clock
        self.deliver = deliver
    }

    /// Push a new value, coalescing with whatever is already pending. If no
    /// delivery is currently scheduled, schedules one -- immediately if at
    /// least `interval` has elapsed since the last delivery, otherwise for
    /// the remainder of the window.
    func push(_ value: Value) {
        lock.lock()
        pending = value
        guard !scheduled else {
            lock.unlock()
            return
        }
        scheduled = true
        let now = clock.now
        let elapsed = lastDeliveryTime.map { $0.duration(to: now) }
        let delay: Duration = {
            guard let elapsed, elapsed < interval else { return .zero }
            return interval - elapsed
        }()
        lock.unlock()

        Task { [weak self] in
            if delay > .zero {
                try? await Task.sleep(for: delay)
            }
            self?.flush()
        }
    }

    /// Deliver whatever is pending right now, synchronously, instead of
    /// waiting for the scheduled window -- used on stream close so a final
    /// coalesced burst is never silently dropped just because it finished
    /// before its flush fired. Safe to call more than once; a second call
    /// after the pending value has already been delivered is a no-op.
    func flushNow() {
        flush()
    }

    private func flush() {
        lock.lock()
        guard let value = pending else {
            scheduled = false
            lock.unlock()
            return
        }
        pending = nil
        scheduled = false
        lastDeliveryTime = clock.now
        lock.unlock()
        deliver(value)
    }
}
