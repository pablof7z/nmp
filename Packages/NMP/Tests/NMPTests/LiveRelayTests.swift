// The bounded LIVE test (M4 plan §8 step D "Green"): proves the whole
// Swift -> NMPFFI -> nmp-engine -> real-relay path end to end, using ONLY
// the public `NMP` surface (no raw websocket code in this file). Every
// network wait is bounded (~15s) so this can never hang a CI run.
//
// ARCHITECTURE NOTE (discovered while building this test, and confirmed
// against `nmp-demo`'s own working CLI): `nmp-router` routes DISCOVERY
// kinds (0/3/10002) to the configured indexer relays automatically, but
// CONTENT kinds (e.g. kind:1) route ONLY to each author's own write relays
// (`RelayDirectory::write_relays`) -- `NmpEngineConfig.writeRelays` is a
// static, caller-supplied snapshot; `nmp-ffi` does NO live NIP-65
// resolution of its own (see that config's doc, and `nmp-demo/src/
// bootstrap.rs`'s doc: "app itself must resolve NIP-65 write relays ...
// before spawning the engine"). So a kind:1 query whose authors are a
// *derived* binding (e.g. "my follows' notes") can resolve WHO the authors
// are entirely reactively (the resolver graph does that live), but cannot
// receive their notes unless this app has already told the engine where
// those authors write -- exactly like `nmp-demo` does via its own
// bootstrap phase before it ever spawns the engine.
//
// This test proves the full pipe with the smallest faithful instance of
// that same two-phase shape, entirely through the public `NMP` API:
//   Phase 1 (discovery, indexer-routed): observe fiatjaf's own kind:10002
//            relay-list to learn HIS write relays.
//   Phase 2 (content, write-relay-routed): construct a SECOND engine (the
//            directory is a one-shot snapshot fixed at construction --
//            same reason `nmp-demo` re-spawns after its own bootstrap) with
//            that resolved write-relay map, and observe kind:1 authored by
//            fiatjaf -- his own real notes, delivered over the real wire.
import XCTest
@testable import NMP

final class LiveRelayTests: XCTestCase {
    /// fiatjaf -- a known, always-active npub, used only as a read target.
    /// No secret key is used anywhere in this test: `setActiveAccount` may
    /// re-root reads onto an account this process holds no key for (read-
    /// only browsing is legal; see `NMPEngine.setActiveAccount`'s doc).
    static let fiatjafHex = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
    static let indexerRelays = ["wss://purplepag.es", "wss://relay.nostr.band"]

    /// The literal ask this harness was built to answer: constructing the
    /// engine from the two operator indexer relays, adding a read-only
    /// account for fiatjaf, and observing the reactive follow-feed
    /// (kind:1 authored by whoever his kind:3 currently names). Per the
    /// architecture note above, this is EXPECTED to observe zero rows with
    /// indexer relays alone and no pre-resolved write-relay map -- the
    /// resolver still computes the derived author set live and correctly
    /// (see `FilterBuilderTests` for that grammar proven in isolation);
    /// there is simply nowhere configured to route the resulting per-author
    /// content reads. `testFiatjafsOwnNotesArriveThroughTheResolvedWriteRelay`
    /// below completes the proof by supplying that map.
    func testFollowFeedWithIndexerRelaysOnlyObservesNoRowsByDesign() async throws {
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
        let rows = await Self.firstNonEmptyBatch(from: query, timeoutSeconds: 15)
        query.cancel()

        XCTAssertNil(
            rows,
            "expected no rows for a content-kind query with no write-relay map configured "
                + "(architecture note above) -- if this now observes rows, nmp-ffi has grown "
                + "live NIP-65 routing and this test's premise should be revisited"
        )
    }

    /// The end-to-end proof: real notes, from a real relay, delivered
    /// through the Swift `AsyncSequence`. Phase 1 resolves fiatjaf's own
    /// write relays via a discovery-kind query (indexer-routed, no
    /// bootstrap map needed); phase 2 spawns a fresh engine configured with
    /// that map and observes his kind:1 notes.
    func testFiatjafsOwnNotesArriveThroughTheResolvedWriteRelay() async throws {
        let writeRelays = try await Self.resolveWriteRelays(for: Self.fiatjafHex, timeoutSeconds: 15)
        guard let writeRelays, !writeRelays.isEmpty else {
            throw XCTSkip(
                "Could not resolve fiatjaf's kind:10002 relay list from "
                    + "\(Self.indexerRelays) within 15s -- indexer relays may be unreachable "
                    + "from this test environment. Package build + construction tests still "
                    + "pass independently of this network condition."
            )
        }

        let engine = try NMPEngine(config: NMPConfig(
            indexerRelays: Self.indexerRelays,
            writeRelays: [Self.fiatjafHex: writeRelays]
        ))
        defer { engine.shutdown() }

        let notesFilter = NMPFilter(kinds: [1], authors: .literal([Self.fiatjafHex]), limit: 20)
        let query = try engine.observe(notesFilter)
        let rows = await Self.firstNonEmptyBatch(from: query, timeoutSeconds: 15)
        query.cancel()

        guard let rows else {
            throw XCTSkip(
                "Resolved \(writeRelays.count) write relay(s) for fiatjaf but observed no kind:1 "
                    + "notes within 15s -- those specific relays may be unreachable from this "
                    + "test environment, or may not currently hold his notes."
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

    /// Phase 1 of the two-phase bootstrap: a plain discovery-kind `NMPQuery`
    /// (kind:10002, indexer-routed) for `author`'s own relay list, parsed
    /// down to its write-relay URLs (marker absent or `"write"`). Entirely
    /// through the public `NMP` API -- no raw wire code.
    private static func resolveWriteRelays(for author: String, timeoutSeconds: UInt64) async throws -> [String]? {
        let engine = try NMPEngine(config: NMPConfig(indexerRelays: indexerRelays))
        defer { engine.shutdown() }

        let relayListFilter = NMPFilter(kinds: [10_002], authors: .literal([author]), limit: 5)
        let query = try engine.observe(relayListFilter)
        let rows = await firstNonEmptyBatch(from: query, timeoutSeconds: timeoutSeconds)
        query.cancel()

        guard let event = rows?.max(by: { $0.createdAt < $1.createdAt }) else {
            return nil
        }
        let urls: [String] = event.tags.compactMap { tag in
            guard tag.first == "r", let url = tag.dropFirst().first else { return nil }
            if tag.count > 2, tag[2] == "read" { return nil }
            return url
        }
        return urls
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
}
