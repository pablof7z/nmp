// #680: the pull-based observation adapter. Every long-lived observation is
// now a UniFFI async OBJECT handle (`NmpRowStream`, `NmpDiagnosticsStream`,
// `NmpFollowStream`, `NmpReceiptStream`, `NmpFollowActionStream`) exposing
// `next() async throws -> Frame?` (nil == the stream ended) plus a sync
// `cancel()`. There is no callback protocol and no drain thread anymore: the
// wrapper simply awaits `next()` in a loop and maps each frame.
//
// This file holds the ONE shared pump that turns any such handle into a Swift
// `AsyncThrowingStream`, so each observation type stays a thin, declarative
// `AsyncSequence`. Two things are load-bearing and documented at their call
// sites:
//
//   * `cancel()` on CONSUMER cancellation -- Swift `Task` cancellation does
//     NOT reach Rust and does NOT interrupt an in-flight
//     `await handle.next()`. So `continuation.onTermination` forwards only
//     `.cancelled` to `handle.cancel()`, which wakes the parked `next()` to
//     `None` and unwinds the pump. A producer-side `.finished` (especially a
//     typed `ConcurrentNext` refusal from a second iterator) must NOT cancel
//     the shared handle out from under the iterator that owns the accepted
//     pull.
//   * `ConcurrentNext` -- the handle is single-consumer. A sequence-level
//     claim admits only one live iterator/pump over a shared handle; a second
//     iterator surfaces `NMPError.concurrentNext` before it can race or cancel
//     the owner's Rust pull. The Rust guard remains the final backstop.

import Foundation
import NMPFFI

/// The shared shape of every #680 pull handle: `next()`/`cancel()`. Each
/// generated stream object already satisfies this; the extensions below just
/// name the conformance so one generic pump serves them all.
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
/// shared when an `AsyncSequence` value is copied, and uses an enum rather than
/// a lifecycle boolean (architecture gate 3). A claim is released only when
/// that iterator's `AsyncThrowingStream` terminates.
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

/// Build an `AsyncThrowingStream` that pulls `handle.next()` in a loop, maps
/// each frame through `map`, and ends when `next()` returns `nil`. `cancel()`
/// runs only on consumer-drop / task cancellation; a producer-side finish
/// already has its own terminal fact and does not own the shared handle.
///
/// - `throttle`: when `true`, delivery is spaced through `FrameCoalescer`
///   (#17 replay-jank falsifier) -- a ~16ms latest-wins coalesce over the
///   pulled snapshots, applied wrapper-side over the pull loop. Only valid for
///   latest-wins snapshot streams, where a dropped intermediate is harmless
///   because every delivered element is already the complete current state.
/// - `bufferingPolicy`: `.bufferingNewest(1)` for those same snapshot streams
///   (a slow consumer keeps only the newest complete snapshot -- the memory
///   bound the old `.bufferingNewest(1)` gave, now on the pull side); the FIFO
///   fact streams (receipts, follow actions) stay `.unbounded` and un-throttled.
func nmpPullStream<Handle: NMPPullHandle, Element: Sendable>(
    handle: Handle,
    iteratorGate: NMPPullIteratorGate,
    bufferingPolicy: AsyncThrowingStream<Element, Error>.Continuation.BufferingPolicy = .unbounded,
    throttle: Bool = false,
    map: @escaping @Sendable (Handle.Frame) -> Element
) -> AsyncThrowingStream<Element, Error> {
    guard iteratorGate.claim() else {
        return AsyncThrowingStream<Element, Error> { continuation in
            continuation.finish(throwing: NMPError.concurrentNext)
        }
    }

    return AsyncThrowingStream<Element, Error>(bufferingPolicy: bufferingPolicy) { continuation in
        let coalescer: FrameCoalescer<Element>? = throttle
            ? FrameCoalescer<Element> { continuation.yield($0) }
            : nil

        // A dropped/cancelled consumer must wake an accepted Rust pull.
        // Producer-side finish is different: a second iterator can finish
        // with `ConcurrentNext`, and letting that loser cancel the shared
        // handle would turn the winner's accepted pull into a silent `nil`.
        continuation.onTermination = { termination in
            if case .cancelled = termination {
                handle.cancel()
            }
            // Release only after cancellation has closed the native handle;
            // otherwise a replacement iterator could claim the gate in the
            // gap and have its fresh pull cancelled by the prior owner.
            iteratorGate.release()
        }

        Task {
            do {
                while let frame = try await handle.next() {
                    let element = map(frame)
                    if let coalescer {
                        coalescer.push(element)
                    } else {
                        continuation.yield(element)
                    }
                }
                coalescer?.flushNow()
                continuation.finish()
            } catch let error as FfiError {
                coalescer?.flushNow()
                continuation.finish(throwing: NMPError(error))
            } catch {
                coalescer?.flushNow()
                continuation.finish(throwing: error)
            }
        }
    }
}
