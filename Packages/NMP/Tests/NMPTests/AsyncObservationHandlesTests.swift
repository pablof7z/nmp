// #680 acceptance falsifier (Swift half): pull-based async observation
// handles removed the one-OS-thread-per-observer bridge AND the global
// native-task capacity surface. Under the OLD design a default engine
// admitted only 12 concurrent observers and refused the 13th unrelated
// observation with `executorSaturated`; opening an app-, group-, inbox-,
// timeline-, and profile-observer on one engine broke composition.
//
// This test opens FAR more than that ceiling across every stream family, on a
// default `NMPConfig()` that no longer even HAS a `maxNativeTasks` field, and
// then cancels them all. It fails to compile against the removed capacity
// config, and would fail at runtime (a thrown `executorSaturated`) under the
// old admission model. It passes only under the pull-based handles.

import XCTest
@testable import NMP

final class AsyncObservationHandlesTests: XCTestCase {
    /// Opening hundreds of simultaneous observations on one engine must never
    /// refuse for capacity -- there is no ceiling, census, or error to hit.
    func testHundredsOfSimultaneousObservationsOpenWithNoCapacityCeiling() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        // Well past the old default cap of 12, mixing families the way an
        // ordinary app composition would.
        var queries: [NMPQuery] = []
        var diagnostics: [NMPDiagnostics] = []
        var following: [NMPFollowingObservation] = []
        for k in 0..<250 {
            queries.append(try engine.observe(NMPFilter(kinds: [UInt16(9_000 + k % 400)])))
        }
        for _ in 0..<40 {
            diagnostics.append(try engine.observeDiagnostics())
        }
        for k in 0..<40 {
            following.append(try engine.observeFollowing(String(repeating: "ab", count: 32)))
            _ = k
        }

        XCTAssertEqual(queries.count, 250)
        XCTAssertEqual(diagnostics.count, 40)
        XCTAssertEqual(following.count, 40)

        // Cancellation is idempotent and leaves nothing to reconcile (#680) --
        // no idle barrier, no census that must return to zero.
        for query in queries { query.cancel() }
        for stream in diagnostics { stream.cancel() }
        for stream in following { stream.cancel() }
        for query in queries { query.cancel() }
    }

    /// The same many-observation composition, but each observation is actively
    /// ITERATED (each opens a real pull handle + pump) and then its consuming
    /// task is cancelled. Cancellation must reach Rust via `handle.cancel()`
    /// (Swift task cancellation alone never interrupts the `await`), so every
    /// task terminates -- no hang, no capacity error anywhere.
    func testManySimultaneousIteratingObservationsCancelCleanly() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let count = 96
        let queries = try (0..<count).map { k in
            try engine.observe(NMPFilter(kinds: [UInt16(7_000 + k)]))
        }

        // Each task iterates its own observation forever; we hold them live,
        // then cancel every task and require every one to unwind promptly.
        var tasks: [Task<Void, Never>] = []
        for query in queries {
            tasks.append(Task {
                do {
                    for try await _ in query {
                        if Task.isCancelled { break }
                    }
                } catch {
                    // No capacity error exists to surface here (#680); a clean
                    // end or the single-consumer signal is the only outcome.
                }
            })
        }

        // Let the pumps spin up and (with no network) settle on their initial
        // empty batch.
        try await Task.sleep(nanoseconds: 200_000_000)

        // Cancel every consuming task AND withdraw every handle. Both together
        // are what liveness requires: task cancellation is delivered, and
        // `cancel()` wakes any parked `next()` inside Rust.
        for task in tasks { task.cancel() }
        for query in queries { query.cancel() }

        // Every task must finish -- race the join against a hard timeout so a
        // regression (a bricked handle) fails loudly instead of hanging.
        // Capture an immutable copy: a `var` captured in the concurrent task
        // closure is a data-race error under strict concurrency checking.
        let joinTasks = tasks
        let joined = await withTaskGroup(of: Bool.self) { group in
            group.addTask {
                for task in joinTasks { await task.value }
                return true
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 10_000_000_000)
                return false
            }
            let result = await group.next() ?? false
            group.cancelAll()
            return result
        }
        XCTAssertTrue(joined, "every cancelled observation task must unwind -- no bricked handle")
    }

    // MARK: - Item 3: cancel while a pull is genuinely PARKED

    /// A live observation with an idle, genuinely PARKED second pull (initial
    /// current-state frame already consumed, no data flowing) must be woken by
    /// the EXPLICIT handle `cancel()` -- a Rust-side wake, not a wrapper
    /// timeout. Proves, in one shot:
    ///   (a) `cancel()` wakes the parked `next()` IMMEDIATELY (bounded well
    ///       under 1s -- the join races a hard timeout so a bricked handle
    ///       fails loudly instead of hanging);
    ///   (b) NO post-cancel frame is delivered -- the parked pull resolves to
    ///       the terminal `nil`, never a late row;
    ///   (c) `cancel()` is idempotent (calling it twice more is safe);
    ///   (d) the wake is driven by the EXPLICIT `cancel()` path, NOT ARC
    ///       `deinit`/finalization -- the query is held live for the whole test.
    func testCancelWakesAParkedNextImmediatelyWithNoPostCancelFrameAndIsIdempotent() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        // An active account roots identity so the initial current-state frame
        // is delivered deterministically (matches nmp-ffi's consumer_isolation
        // oracle). No relays, no further writes -> after that first frame the
        // pull has nothing to deliver and genuinely parks.
        let registration = try await engine.addAccount(secretKey: Self.testSecretKey)
        try engine.setActiveAccount(registration.publicKey)
        let query = try engine.observe(NMPFilter(kinds: [8_811]))

        // Signalled once the iterating task has consumed the initial frame and
        // is about to park on its SECOND `next()`.
        let (parkedSignal, parkedCont) = AsyncStream<Void>.makeStream()

        final class Outcome: @unchecked Sendable {
            var framesAfterFirst = 0
            var secondWasTerminalNil = false
            var threw: Error?
        }
        let outcome = Outcome()

        let task = Task { () -> Void in
            do {
                var iterator = query.makeAsyncIterator()
                // 1st next(): the initial current-state batch.
                _ = try await iterator.next()
                // Announce we are about to park on a second next() with no data
                // in flight -- it will only resolve when the handle is cancelled.
                parkedCont.yield(())
                parkedCont.finish()
                if try await iterator.next() != nil {
                    outcome.framesAfterFirst += 1 // a post-cancel frame -> BAD
                } else {
                    outcome.secondWasTerminalNil = true // clean terminal nil -> good
                }
            } catch {
                outcome.threw = error
            }
        }

        // Wait until the pull is parked on its second next(), then give it a
        // beat to actually suspend inside Rust before we cancel.
        for await _ in parkedSignal { break }
        try await Task.sleep(nanoseconds: 100_000_000)

        // (d) EXPLICIT teardown (not deinit): the query value stays alive here.
        let start = Date()
        query.cancel()

        // (a) bounded, Rust-side wake: the task must unwind well under 1s.
        let joined = await Self.join(task, withinSeconds: 3)
        let elapsed = Date().timeIntervalSince(start)
        XCTAssertTrue(joined, "explicit cancel must wake the parked next() -- no bricked handle")
        XCTAssertLessThan(elapsed, 1.0, "the parked pull woke immediately (Rust-side), not on a timeout")
        // (b) no frame after cancel; the parked pull resolved to terminal nil.
        XCTAssertEqual(outcome.framesAfterFirst, 0, "no post-cancel frame may be delivered")
        XCTAssertTrue(
            outcome.secondWasTerminalNil,
            "the parked next() resolves to the terminal nil on cancel"
        )
        XCTAssertNil(outcome.threw, "a clean cancel ends with nil, not a thrown error")
        // (c) idempotent: two more cancels are safe no-ops.
        query.cancel()
        query.cancel()
    }

    // MARK: - Item 4: multi-consumer contract

    /// The RECOMMENDED multi-consumer pattern: TWO INDEPENDENT observations
    /// (two `engine.observe`), iterated CONCURRENTLY. Each owns its own Rust
    /// handle, so both deliver their initial current-state batch and NEITHER
    /// can surface `concurrentNext` (there is no shared single-consumer handle
    /// to contend). Post-#680 a second observation is cheap -- no OS thread.
    func testTwoIndependentObservationsBothDeliverConcurrentlyWithNoError() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let registration = try await engine.addAccount(secretKey: Self.testSecretKey)
        try engine.setActiveAccount(registration.publicKey)

        let a = try engine.observe(NMPFilter(kinds: [8_821]))
        let b = try engine.observe(NMPFilter(kinds: [8_822]))

        async let firstA = Self.firstBatch(from: a, withinSeconds: 5)
        async let firstB = Self.firstBatch(from: b, withinSeconds: 5)
        let (resultA, resultB) = await (firstA, firstB)

        XCTAssertNotNil(resultA, "independent observation A delivers its initial batch")
        XCTAssertNotNil(resultB, "independent observation B delivers its initial batch")
        a.cancel()
        b.cancel()
    }

    /// CONTRACT: a Swift `NMPQuery` is SINGLE-CONSUMER, matching `AsyncStream`.
    /// It holds ONE `NmpRowStream` handle; every `makeAsyncIterator()` pumps
    /// that SAME handle. Two CONCURRENT iterators over one `NMPQuery` therefore
    /// race the single Rust handle, and the contract is LOUD and deterministic:
    /// the losing pull surfaces a TYPED `NMPError.concurrentNext` -- never a
    /// hang, never a silently dropped/duplicated frame. (Independent consumers
    /// must instead open INDEPENDENT observations -- see the test above.)
    ///
    /// The load-bearing assertions: NEITHER iterator hangs, and any error that
    /// surfaces is EXACTLY `concurrentNext` (never some other error, never
    /// silent loss). Acceptable outcomes are {one batch, one concurrentNext}
    /// (the expected race result) or {both batches} (if the two pulls happened
    /// to serialize) -- both are contractful; anything else fails.
    func testTwoConcurrentIteratorsOverOneNMPQueryAreLoudNeverHung() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let registration = try await engine.addAccount(secretKey: Self.testSecretKey)
        try engine.setActiveAccount(registration.publicKey)
        let query = try engine.observe(NMPFilter(kinds: [8_831]))

        func pullOne() -> Task<Pull, Never> {
            Task {
                do {
                    var iterator = query.makeAsyncIterator()
                    return try await iterator.next() != nil ? .batch : .endedNil
                } catch let error as NMPError where error == .concurrentNext {
                    return .concurrentNext
                } catch {
                    return .otherError(String(describing: error))
                }
            }
        }

        let t1 = pullOne()
        let t2 = pullOne()
        // No-hang requirement: each pull must resolve within a hard bound.
        let r1 = await Self.value(of: t1, withinSeconds: 5)
        let r2 = await Self.value(of: t2, withinSeconds: 5)
        query.cancel()

        // NO HANG: both concurrent iterators completed within the bound.
        XCTAssertNotNil(r1, "iterator 1 must not hang on the single-consumer handle")
        XCTAssertNotNil(r2, "iterator 2 must not hang on the single-consumer handle")

        let outcomes = [r1, r2].compactMap { $0 }
        // The only failure mode allowed is the TYPED concurrentNext; never some
        // other error, and never a silent nil-without-cause.
        for outcome in outcomes {
            if case .otherError(let description) = outcome {
                XCTFail("the loud failure must be concurrentNext, not: \(description)")
            }
            if case .endedNil = outcome {
                XCTFail("a concurrent iterator ended with silent nil instead of a batch or concurrentNext")
            }
        }
        let batches = outcomes.filter { if case .batch = $0 { return true } else { return false } }.count
        let concurrentNexts = outcomes.filter { if case .concurrentNext = $0 { return true } else { return false } }.count
        // Record the exact result for the report.
        print("ITEM4_SWIFT_ONE_QUERY_TWO_ITERATORS batches=\(batches) concurrentNext=\(concurrentNexts)")
        XCTAssertTrue(
            (batches == 1 && concurrentNexts == 1) || batches == 2,
            "contractful outcomes only: {batch, concurrentNext} or {batch, batch}; got batches=\(batches) concurrentNext=\(concurrentNexts)"
        )
    }

    // MARK: - Fixtures / helpers

    /// The well-known low secret key used by nmp-ffi's own oracles; activating
    /// an account roots identity so the initial current-state frame is
    /// delivered deterministically.
    static let testSecretKey =
        "0000000000000000000000000000000000000000000000000000000000000001"

    enum Pull: Sendable {
        case batch
        case concurrentNext
        case otherError(String)
        case endedNil
    }

    /// Await a `Task`'s completion, but bounded by a hard timeout so a bricked
    /// handle (a pull that never wakes) fails loudly instead of hanging the
    /// suite. Returns `true` iff the task finished within the bound.
    static func join(_ task: Task<Void, Never>, withinSeconds seconds: UInt64) async -> Bool {
        await withTaskGroup(of: Bool.self) { group in
            group.addTask {
                await task.value
                return true
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
                return false
            }
            let result = await group.next() ?? false
            group.cancelAll()
            return result
        }
    }

    /// The value of a `Task`, or `nil` if it did not finish within the bound
    /// (the no-hang detector).
    static func value<T: Sendable>(of task: Task<T, Never>, withinSeconds seconds: UInt64) async -> T? {
        await withTaskGroup(of: T?.self) { group in
            group.addTask { await task.value }
            group.addTask {
                try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
                return nil
            }
            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }

    /// Pull the first delivered batch from a query, bounded by a timeout.
    static func firstBatch(from query: NMPQuery, withinSeconds seconds: UInt64) async -> RowBatch? {
        await withTaskGroup(of: RowBatch?.self) { group in
            group.addTask {
                do {
                    var iterator = query.makeAsyncIterator()
                    return try await iterator.next()
                } catch {
                    return nil
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
                return nil
            }
            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }
}
