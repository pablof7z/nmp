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

    /// Subscribing to a literal-author query with NO write relay known for
    /// that author (no indexer, no fixture) leaves the router with nowhere
    /// to route the atom -- proving the ABSENCE case: the snapshot must
    /// report zero relays (never a fabricated one) AND must count the
    /// author as genuinely uncovered, both real numbers read off the
    /// running engine.
    func testObserveDiagnosticsNeverFabricatesARelayForAnUnroutableAuthor() async throws {
        let engine = try NMPEngine(config: NMPConfig(indexerRelays: []))
        defer { engine.shutdown() }

        let hexPubkey = String(repeating: "a", count: 64)
        let query = try engine.observe(NMPFilter(kinds: [1], authors: .literal([hexPubkey])))

        let diagnostics = try engine.observeDiagnostics()
        let snapshot = await Self.firstSnapshot(from: diagnostics, timeoutSeconds: 5)
        diagnostics.cancel()
        query.cancel()

        guard let snapshot else {
            return XCTFail("expected an immediate diagnostics snapshot")
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
        from diagnostics: NMPDiagnostics, timeoutSeconds: UInt64
    ) async -> DiagnosticsSnapshot? {
        await withTaskGroup(of: DiagnosticsSnapshot?.self) { group in
            group.addTask {
                for await snapshot in diagnostics {
                    return snapshot
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
