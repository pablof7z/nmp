// The bounded LIVE test (M4 plan §8 step D "Green", updated for M5's
// self-bootstrapping outbox): proves the whole Swift -> NMPFFI ->
// nmp-engine -> real-relay path end to end, using ONLY the public `NMP`
// surface (no raw websocket code in this file). Every network wait is
// bounded (~15s) so this can never hang a CI run.
//
// ARCHITECTURE NOTE (M5): the engine now self-navigates outbox routing from
// the configured indexer relays alone. `nmp-router` still routes DISCOVERY
// kinds (0/3/10002) to the indexers automatically, but `nmp-engine`'s own
// `EngineCore` ALSO watches active content demand for authors whose write
// relays are still unknown and opens its own internal kind:10002 discovery
// reads against those same indexers -- so a kind:1 query, even one whose
// authors are a *derived* binding ("my follows' notes"), resolves who the
// authors are AND discovers where they write, entirely on its own. There is
// no bootstrap phase and no pre-resolved write-relay map anymore
// (`NMPConfig` no longer has a `writeRelays` field at all).
import XCTest
@testable import NMP

final class LiveRelayTests: XCTestCase {
    /// fiatjaf -- a known, always-active npub, used only as a read target.
    /// No secret key is used anywhere in this test: `setActiveAccount` may
    /// re-root reads onto an account this process holds no key for (read-
    /// only browsing is legal; see `NMPEngine.setActiveAccount`'s doc).
    static let fiatjafHex = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
    static let indexerRelays = ["wss://purplepag.es", "wss://relay.primal.net"]

    /// THE headline live proof: construct the engine from ONLY the two
    /// operator indexer relays (no write-relay map -- there is no such
    /// field anymore), add a read-only account for fiatjaf, and observe the
    /// reactive follow-feed (kind:1 authored by whoever his kind:3
    /// currently names). This app never resolves a single relay itself --
    /// the engine discovers fiatjaf's own write relays live (via its
    /// internal kind:10002 auto-discovery) and re-routes the content atom
    /// to them on its own.
    func testFollowFeedResolvesFromIndexerRelaysAlone() async throws {
        let engine = try NMPEngine(config: NMPConfig(indexerRelays: Self.indexerRelays))
        defer { engine.shutdown() }

        try engine.setActiveAccount(Self.fiatjafHex)

        let followFeed = NMPFilter(
            kinds: [1],
            authors: .derived(
                inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                project: .tag("p")
            ),
            limit: 50
        )
        let query = try engine.observe(followFeed)
        let rows = await Self.firstNonEmptyBatch(from: query, timeoutSeconds: 30)
        query.cancel()

        guard let rows else {
            throw XCTSkip(
                "Observed no follow-feed rows within 30s from \(Self.indexerRelays) alone -- "
                    + "the indexers, or fiatjaf's follows' write relays, may be unreachable "
                    + "from this test environment. Package build + construction tests still "
                    + "pass independently of this network condition."
            )
        }

        XCTAssertGreaterThan(rows.count, 0, "expected at least one real note")
        for row in rows.prefix(5) {
            XCTAssertEqual(row.kind, 1)
            XCTAssertFalse(row.id.isEmpty)
        }
    }

    /// The diagnostic surface (M5), proven live: once the follow feed has
    /// actually produced rows, `observeDiagnostics()` must show a relay
    /// whose `eventsByKind` reports a REAL received kind:1 count > 0 --
    /// never fabricated, and matching what the row stream already proved
    /// arrived.
    func testDiagnosticsSnapshotShowsRealEventsByKindForTheFollowFeed() async throws {
        let engine = try NMPEngine(config: NMPConfig(indexerRelays: Self.indexerRelays))
        defer { engine.shutdown() }

        try engine.setActiveAccount(Self.fiatjafHex)

        let followFeed = NMPFilter(
            kinds: [1],
            authors: .derived(
                inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                project: .tag("p")
            ),
            limit: 50
        )
        let query = try engine.observe(followFeed)
        // Keep the native query demand owned through the diagnostics sample;
        // diagnostics intentionally projects only the current relay plan.
        defer { query.cancel() }
        let rows = await Self.firstNonEmptyBatch(from: query, timeoutSeconds: 30)

        guard rows != nil else {
            throw XCTSkip(
                "Observed no follow-feed rows within 30s from \(Self.indexerRelays) alone -- "
                    + "diagnostics has nothing real to report in this test environment."
            )
        }

        let diagnostics = try engine.observeDiagnostics()
        defer { diagnostics.cancel() }
        let snapshot = await Self.firstSnapshotWithReceivedKind1(from: diagnostics, timeoutSeconds: 10)

        guard let snapshot else {
            return XCTFail(
                "expected a diagnostics snapshot reporting a real kind:1 event count, once the "
                    + "follow feed had already produced rows"
            )
        }

        XCTAssertGreaterThanOrEqual(snapshot.relays.count, 1)
        let hasReceivedKind1 = snapshot.relays.contains { relay in
            relay.eventsByKind.contains { $0.kind == 1 && $0.count > 0 }
        }
        XCTAssertTrue(
            hasReceivedKind1,
            "at least one relay must show a real received kind:1 count, matching the rows "
                + "already observed"
        )
    }

    /// The same self-bootstrapping proof for a LITERAL author set (no
    /// derived binding involved at all): fiatjaf's own kind:1 notes, from a
    /// fresh engine configured with ONLY the indexer relays.
    func testAuthorsOwnNotesArriveWithNoWriteRelayConfigured() async throws {
        let engine = try NMPEngine(config: NMPConfig(indexerRelays: Self.indexerRelays))
        defer { engine.shutdown() }

        let notesFilter = NMPFilter(kinds: [1], authors: .literal([Self.fiatjafHex]), limit: 20)
        let query = try engine.observe(notesFilter)
        let rows = await Self.firstNonEmptyBatch(from: query, timeoutSeconds: 30)
        query.cancel()

        guard let rows else {
            throw XCTSkip(
                "Observed no kind:1 notes for fiatjaf within 30s from \(Self.indexerRelays) "
                    + "alone -- his resolved write relays may be unreachable from this test "
                    + "environment."
            )
        }

        XCTAssertGreaterThan(rows.count, 0, "expected at least one real note")
        for row in rows.prefix(5) {
            XCTAssertEqual(row.kind, 1)
            XCTAssertEqual(row.pubkey, Self.fiatjafHex)
            XCTAssertFalse(row.id.isEmpty)
            XCTAssertFalse(row.content.isEmpty)
        }
    }

    /// Races the query's first non-empty snapshot against a hard timeout so
    /// this test can never hang, regardless of what the live network does.
    private static func firstNonEmptyBatch(from query: NMPQuery, timeoutSeconds: UInt64) async -> [Row]? {
        await withTaskGroup(of: [Row]?.self) { group in
            group.addTask {
                for await batch in query {
                    if !batch.rows.isEmpty {
                        return batch.rows
                    }
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

    /// Races the diagnostics stream's first snapshot that already reports a
    /// received kind:1 event against a hard timeout so this test can never
    /// hang. `NMPDiagnostics` is latest-wins (see `nmp-engine::runtime::
    /// diagnostics_channel`'s doc), so the count only ever grows -- once a
    /// row is known to have arrived, a subsequent snapshot showing it is a
    /// matter of when, not if.
    private static func firstSnapshotWithReceivedKind1(
        from diagnostics: NMPDiagnostics, timeoutSeconds: UInt64
    ) async -> DiagnosticsSnapshot? {
        await withTaskGroup(of: DiagnosticsSnapshot?.self) { group in
            group.addTask {
                for await snapshot in diagnostics {
                    let hasKind1 = snapshot.relays.contains { relay in
                        relay.eventsByKind.contains { $0.kind == 1 && $0.count > 0 }
                    }
                    if hasKind1 {
                        return snapshot
                    }
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
