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
//   * `cancel()` on termination -- Swift `Task` cancellation does NOT reach
//     Rust and does NOT interrupt an in-flight `await handle.next()`. The
//     future only completes when a value/None arrives or the handle is
//     cancelled. So `continuation.onTermination` (which fires both when the
//     stream finishes AND when the last consumer drops the iterator, i.e. on
//     task cancellation) calls `handle.cancel()`, which wakes the parked
//     `next()` to `None` and unwinds the pump. This is mandatory for liveness.
//   * `ConcurrentNext` -- the handle is single-consumer. If a second pump ever
//     awaits `next()` while one is in flight (two iterators over one handle),
//     Rust rejects it with `FfiError.ConcurrentNext`; the pump surfaces that
//     as `NMPError.concurrentNext` on the stream, never a hang.

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

/// Build an `AsyncThrowingStream` that pulls `handle.next()` in a loop, maps
/// each frame through `map`, and ends when `next()` returns `nil`. `cancel()`
/// runs on stream termination (finish or consumer-drop / task cancellation).
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
    bufferingPolicy: AsyncThrowingStream<Element, Error>.Continuation.BufferingPolicy = .unbounded,
    throttle: Bool = false,
    map: @escaping @Sendable (Handle.Frame) -> Element
) -> AsyncThrowingStream<Element, Error> {
    AsyncThrowingStream<Element, Error>(bufferingPolicy: bufferingPolicy) { continuation in
        let coalescer: FrameCoalescer<Element>? = throttle
            ? FrameCoalescer<Element> { continuation.yield($0) }
            : nil

        // Fires on normal finish AND when the last consumer drops its
        // iterator (task cancellation) -- the mandatory Rust-side cancel.
        continuation.onTermination = { _ in handle.cancel() }

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
