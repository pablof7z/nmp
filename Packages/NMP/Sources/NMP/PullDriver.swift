// #680: the pull-based observation adapter. Every long-lived observation is
// a UniFFI async object handle exposing `next()` plus synchronous `cancel()`.
//
// Swift deliberately does not add a producer task or AsyncThrowingStream
// between the app and that handle. The iterator itself owns the native pull:
// one app `next()` performs one native `next()`, so FIFO fact streams inherit
// Rust backpressure and snapshot streams inherit the engine's bounded
// latest-value mailbox instead of gaining a second Swift queue.

import Foundation
import NMPFFI

/// The shared shape of every #680 pull handle. The generated stream objects
/// already satisfy this; these extensions only name the conformance.
protocol NMPPullHandle: AnyObject, Sendable {
    associatedtype Frame: Sendable
    func next() async throws -> Frame?
    func cancel()
}

extension NmpRowStream: NMPPullHandle {}
extension NmpDiagnosticsStream: NMPPullHandle {}
extension NmpFollowStream: NMPPullHandle {}
extension NmpReceiptStream: NMPPullHandle {}
extension NmpFollowActionStream: NMPPullHandle {}

/// One live Swift iterator may own a native pull handle at a time. The gate is
/// shared when an AsyncSequence value is copied and uses an enum rather than a
/// lifecycle boolean (architecture gate 3).
final class NMPPullIteratorGate: @unchecked Sendable {
    private enum State {
        case available
        case claimed
    }

    private let lock = NSLock()
    private var state = State.available

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard case .available = state else { return false }
        state = .claimed
        return true
    }

    func release() {
        lock.lock()
        state = .available
        lock.unlock()
    }
}

/// Reference-owned iterator state. A public AsyncIterator is a small struct
/// retaining one of these cores; when a `for try await` loop exits normally
/// (including `break`), dropping that iterator drops this core, whose `deinit`
/// cancels the parked native pull and releases the sequence claim.
///
/// The state enum makes ownership and teardown explicit:
///
/// - `denied`: a competing iterator never owned the handle;
/// - `ready`: the owner may begin one pull;
/// - `pulling`: a pull is in flight, so a copied iterator is refused loudly;
/// - `ended`: cancellation/terminal/error has closed this owner's lifecycle.
final class NMPPullIteratorCore<Handle: NMPPullHandle, Element: Sendable>:
    @unchecked Sendable
{
    private enum State {
        case denied
        case ready
        case pulling
        case ended
    }

    private let handle: Handle
    private let gate: NMPPullIteratorGate
    private let map: @Sendable (Handle.Frame) -> Element
    private let throttle: Bool
    private let clock = ContinuousClock()
    private let lock = NSLock()
    private var state: State
    private var lastDeliveryTime: ContinuousClock.Instant?

    init(
        handle: Handle,
        iteratorGate: NMPPullIteratorGate,
        throttle: Bool = false,
        map: @escaping @Sendable (Handle.Frame) -> Element
    ) {
        self.handle = handle
        self.gate = iteratorGate
        self.map = map
        self.throttle = throttle
        self.state = iteratorGate.claim() ? .ready : .denied
    }

    deinit {
        cancel()
    }

    func next() async throws -> Element? {
        switch beginPull() {
        case .refused:
            throw NMPError.concurrentNext
        case .ended:
            return nil
        case .started:
            break
        }

        do {
            let frame = try await withTaskCancellationHandler {
                try await handle.next()
            } onCancel: {
                self.cancel()
            }

            guard let frame else {
                finish(cancelNative: false)
                return nil
            }

            let element = map(frame)
            let delay = deliveryDelay()
            if delay > .zero {
                try await Task.sleep(for: delay)
            }

            guard completeDelivery() else {
                return nil
            }
            return element
        } catch is CancellationError {
            cancel()
            return nil
        } catch let error as FfiError {
            finish(cancelNative: true)
            throw NMPError(error)
        } catch {
            finish(cancelNative: true)
            throw error
        }
    }

    /// Idempotently close the native handle before releasing the iterator
    /// claim. The ordering prevents a replacement iterator from claiming in
    /// the gap and having its pull cancelled by the prior owner.
    func cancel() {
        finish(cancelNative: true)
    }

    private enum BeginPull {
        case refused
        case ended
        case started
    }

    private func beginPull() -> BeginPull {
        lock.lock()
        defer { lock.unlock() }
        switch state {
        case .denied, .pulling:
            return .refused
        case .ended:
            return .ended
        case .ready:
            state = .pulling
            return .started
        }
    }

    private func completeDelivery() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard case .pulling = state else { return false }
        lastDeliveryTime = clock.now
        state = .ready
        return true
    }

    private func deliveryDelay() -> Duration {
        guard throttle else { return .zero }
        let interval = Duration.milliseconds(16)
        lock.lock()
        let last = lastDeliveryTime
        lock.unlock()
        guard let last else { return .zero }
        let elapsed = last.duration(to: clock.now)
        return elapsed < interval ? interval - elapsed : .zero
    }

    private func finish(cancelNative: Bool) {
        lock.lock()
        let owned: Bool
        switch state {
        case .ready, .pulling:
            state = .ended
            owned = true
        case .denied, .ended:
            owned = false
        }
        lock.unlock()

        guard owned else { return }
        if cancelNative {
            handle.cancel()
        }
        gate.release()
    }
}
