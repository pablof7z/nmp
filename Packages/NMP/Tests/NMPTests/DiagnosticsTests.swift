// Bounded, no-network construction/shape tests for the diagnostic surface
// (M5 plan §1.4): `observeDiagnostics()` must deliver a snapshot immediately
// on registration (the engine thread primes the mailbox with the CURRENT
// snapshot before replying -- see `nmp-engine::runtime`'s
// `Cmd::ObserveDiagnostics` handler), so these never depend on a live relay
// and never poll -- a bounded timeout race, same discipline as
// `LiveRelayTests`'s own `firstNonEmptyBatch`.

import XCTest
@testable import NMP

final class DiagnosticsTests: XCTestCase {
    /// #680: opening many diagnostics observations no longer touches any
    /// native-task capacity -- there is no admission ceiling, no census, and
    /// no `executorSaturated`. Dozens of concurrent diagnostics streams on one
    /// engine all open, and cancelling them leaves nothing to reconcile.
    func testManyDiagnosticsObservationsOpenWithoutACapacityCeiling() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        var held: [NMPDiagnostics] = []
        for _ in 0..<64 {
            held.append(try engine.observeDiagnostics())
        }
        for diagnostics in held {
            diagnostics.cancel()
        }
    }

    /// #442: construction/shutdown is an exact join barrier. Repeating the
    /// full native path must not accumulate verifier, relay, bridge, or
    /// runtime threads between engine lifetimes.
    func testRepeatedEngineConstructionAndShutdown() throws {
        for _ in 0..<32 {
            let engine = try NMPEngine(config: NMPConfig())
            engine.shutdown()
        }
    }

    /// A freshly constructed engine with no subscriptions and no configured
    /// indexer relays has compiled no plan yet -- `observeDiagnostics()`
    /// must still deliver a well-formed (empty, never fabricated) snapshot
    /// right away.
    func testObserveDiagnosticsYieldsAnImmediateSnapshot() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let diagnostics = try engine.observeDiagnostics()
        let snapshot = await Self.firstSnapshot(from: diagnostics, timeoutSeconds: 5)
        diagnostics.cancel()

        guard let snapshot else {
            return XCTFail(
                "observeDiagnostics() must deliver a snapshot immediately on registration -- "
                    + "no network required"
            )
        }

        XCTAssertEqual(snapshot.relays.count, 0)
        XCTAssertEqual(snapshot.uncoveredAuthorCount, 0)
        XCTAssertEqual(snapshot.droppedMergeRules.count, 0)
        XCTAssertNil(snapshot.transportDegraded)
    }

    /// Subscribing to a literal-author query with no author route or
    /// operator policy leaves the router with nowhere to route the atom.
    /// Pending relay admission may make the immediate snapshot precede that
    /// routing decision; the first settled snapshot must report zero relays
    /// (never a fabricated one) AND count the author as genuinely uncovered.
    func testObserveDiagnosticsNeverFabricatesARelayForAnUnroutableAuthor() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let hexPubkey = String(repeating: "a", count: 64)
        let query = try engine.observe(
            .single(
                NMPDemand(
                    selection: NMPFilter(kinds: [1], authors: .literal([hexPubkey]))
                )
            )
        )

        let diagnostics = try engine.observeDiagnostics()
        let snapshot = await Self.firstSnapshot(
            from: diagnostics,
            timeoutSeconds: 5,
            matching: { $0.uncoveredAuthorCount == 1 }
        )
        diagnostics.cancel()
        query.cancel()

        guard let snapshot else {
            return XCTFail("expected relay admission to settle in diagnostics")
        }

        XCTAssertEqual(snapshot.relays.count, 0)
        XCTAssertEqual(
            snapshot.uncoveredAuthorCount, 1,
            "the one demanded author with no known write relay must show up as uncovered"
        )
    }

    /// Races the stream's first snapshot against a hard timeout so this test
    /// can never hang.
    private static func firstSnapshot(
        from diagnostics: NMPDiagnostics,
        timeoutSeconds: UInt64,
        matching predicate: @escaping @Sendable (DiagnosticsSnapshot) -> Bool = { _ in true }
    ) async -> DiagnosticsSnapshot? {
        await withTaskGroup(of: DiagnosticsSnapshot?.self) { group in
            group.addTask {
                do {
                    for try await snapshot in diagnostics where predicate(snapshot) {
                        return snapshot
                    }
                } catch {
                    return nil
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                return nil
            }

            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }
}
