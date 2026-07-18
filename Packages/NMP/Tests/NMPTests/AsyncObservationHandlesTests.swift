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
        let joined = await withTaskGroup(of: Bool.self) { group in
            group.addTask {
                for task in tasks { await task.value }
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
}
