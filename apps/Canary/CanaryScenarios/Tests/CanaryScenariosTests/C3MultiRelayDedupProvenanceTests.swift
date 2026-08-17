// C3 (docs/internals/canary.md "Scenario status"): multi-relay dedup and
// provenance. The SAME event arrives from three separate real relay
// processes, one after another, and the app must see ONE canonical row,
// ONCE, whose `sources` grows to name every relay that supplied it.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C2/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The three relays are reached only over
// real `ws://` URLs to three separate OS processes.
//
// THREE FAILURES ARE IN SCOPE AND THEY ARE DIFFERENT FAILURES.
//
//   - TWO ROWS. The second delivery is admitted as a new canonical row.
//     Caught by the row count, and by the id set.
//   - A REPLACED ROW. The second delivery removes the row and re-adds it.
//     The id set and the row count both survive that, so neither catches
//     it. What does: the row's POSITION. A companion event is seeded
//     alongside the shared one specifically so the delivered array has two
//     entries and an index exists to be disturbed -- an unbounded
//     observation folds exact rebased deltas in arrival order
//     (`RowAccumulator`), so a removed-and-re-added row moves to the end
//     while an in-place provenance update leaves `order` untouched. The
//     index is pinned from the moment the pre-growth state is established
//     and required to hold across both growths.
//   - LOST PROVENANCE. The row stays one row in one place and simply never
//     learns who else has it, which is the failure that makes `sources`
//     useless. Caught by requiring the exact three-relay set at the end,
//     and each individual relay at its own step.
//
// THE PRECONDITION IS THE POINT, and it is asserted rather than assumed.
// "The app ended up with one row naming three relays" is trivially true of
// an app that was handed the event by all three relays at once during a
// cold start -- nothing about dedup would have been exercised, because
// there would never have been a moment when the row existed with fewer
// sources than it finished with. So relay B and relay C start EMPTY and
// are seeded only after:
//
//   1. the row is delivered with `sources` EXACTLY `[relayA]` -- the app
//      genuinely does not yet have this event from B or C; and
//   2. B and C are nonetheless genuinely subscribed and genuinely empty:
//      `observeDiagnostics()` reports a wire subscription on each of the
//      three relays, and an ordinary client REQ against B and C for this
//      id comes back empty. B and C have nothing to give, not nothing to
//      say.
//
// Only then is the identical event -- same id, same signature, the same
// bytes, a real duplicate on the wire, not a re-signed lookalike -- written
// into B, and later into C. Each write is a real `EVENT` frame answered by
// a real `OK`, asserted, so "the second relay genuinely delivered it" is
// established by construction rather than inferred from the app agreeing.
//
// Every wait below is a bounded poll on a real condition with the real
// stuck values reported on timeout -- never a fixed sleep used AS the
// synchronization oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C3MultiRelayDedupProvenanceTests: XCTestCase {
    // MARK: - What the one observation has been delivered

    /// The delivered state of the single observation. Deliberately records
    /// more than the final assertions need: a scenario about "one row, once"
    /// has to be able to say what it saw at every beat, or a failure is just
    /// a timeout.
    private struct ObservedState: Sendable {
        var batches = 0
        /// The newest batch's rows, in delivered order.
        var latest: [Row] = []
        /// Union of every id ever delivered in any batch. A row that appears
        /// and then vanishes is invisible to `latest` alone.
        var everSeen: Set<String> = []
        /// The largest row count any single batch ever carried. A duplicate
        /// canonical row raises this even if a later batch settles back.
        var maxRowCount = 0
        /// Non-nil iff some batch carried the same event id twice.
        var duplicateWitness: String?
        /// Every distinct `sources` value the shared row has ever been
        /// delivered with, in arrival order. This is the provenance history,
        /// printed as evidence on every run.
        var sourcesHistory: [[String]] = []
        var ended: String?

        /// The shared row as most recently delivered, or `nil`.
        func shared(_ id: String) -> Row? { latest.first { $0.id == id } }
        /// Its index in the delivered array, or `nil`.
        func sharedIndex(_ id: String) -> Int? { latest.firstIndex { $0.id == id } }
    }

    private actor ObservationLedger {
        private var state = ObservedState()
        private let sharedID: String

        init(sharedID: String) {
            self.sharedID = sharedID
        }

        func record(_ batch: RowBatch) {
            state.batches += 1
            let ids = batch.rows.map(\.id)
            if Set(ids).count != ids.count {
                state.duplicateWitness = ids.joined(separator: " ")
            }
            state.latest = batch.rows
            state.everSeen.formUnion(ids)
            state.maxRowCount = max(state.maxRowCount, ids.count)
            if let row = batch.rows.first(where: { $0.id == sharedID }),
               state.sourcesHistory.last != row.sources {
                state.sourcesHistory.append(row.sources)
            }
        }

        func markEnded(_ why: String) { state.ended = why }
        func current() -> ObservedState { state }
    }

    /// `observeDiagnostics()` is PUSH-only -- there is no synchronous "what
    /// is your current snapshot" call on `NMPEngine`, so an application that
    /// wants a point-in-time reading has to hold the stream open and keep
    /// the last value it was handed. This box is exactly that and nothing
    /// more. C13's scenario and C17's churner contain the same lines;
    /// duplicating them is deliberate (`docs/internals/canary.md`: "a little
    /// duplication is preferable to hiding evidence") -- the duplication IS
    /// the evidence that every scenario needing a current resource reading
    /// has to build this itself.
    private final class LatestDiagnostics: @unchecked Sendable {
        private let lock = NSLock()
        private var value = DiagnosticsSnapshot()

        func store(_ snapshot: DiagnosticsSnapshot) {
            lock.lock()
            value = snapshot
            lock.unlock()
        }

        func current() -> DiagnosticsSnapshot {
            lock.lock()
            defer { lock.unlock() }
            return value
        }

        /// The relay URLs currently reporting at least one wire subscription.
        func subscribedRelays() -> Set<String> {
            Set(current().relays.filter { $0.wireSubCount >= 1 }.map(\.relay))
        }
    }

    /// Bounded poll on a real condition. Returns whether the condition ever
    /// held; the caller reports the real stuck values on `false`. The sleep
    /// paces the poll -- it is never the thing being waited on.
    @discardableResult
    private func waitUntil(
        timeout: TimeInterval = 30,
        _ condition: () async -> Bool
    ) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await condition() { return true }
            try? await Task.sleep(nanoseconds: 25_000_000)
        }
        return await condition()
    }

    // MARK: - The scenario

    func testSameEventFromThreeRelaysIsOneRowWithGrowingProvenance() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c3-\(UUID().uuidString)")
        let storeDir = root.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        // Three relays, each with its OWN data directory -- three genuinely
        // independent stores, so "the same event arrived from another relay"
        // means another relay's own copy, not a second view of one database.
        var relays: [RelayHandle] = []
        for name in ["c3-relay-a", "c3-relay-b", "c3-relay-c"] {
            let dir = root.appendingPathComponent(name)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let handle = try await RelayHandle(name: name, workDir: dir, binaryPath: binaryPath)
            try await handle.start()
            relays.append(handle)
        }
        let relayA = relays[0], relayB = relays[1], relayC = relays[2]

        let keyPair = try NostrKeyPair()
        let filter = NMPFilter(kinds: [1], authors: .literal([keyPair.pubkeyHex]))

        // The event under test, and a companion. The companion exists for
        // one reason: it gives the delivered array a second entry, so the
        // shared row HAS an index that a remove-and-re-add would disturb.
        // It lives only on relay A for the whole scenario.
        let shared = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C3 the event every relay has"
        )
        let companion = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C3 companion, relay A only"
        )
        try await relayA.seed(companion)
        try await relayA.seed(shared)

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storeDir.appendingPathComponent("nmp.redb").path,
                appRelays: [relayA.url, relayB.url, relayC.url]
            )
        )
        defer { engine.shutdown() }

        let diagnostics = LatestDiagnostics()
        let diagnosticsStream = try engine.observeDiagnostics()
        let diagnosticsPump = Task {
            do {
                for try await snapshot in diagnosticsStream { diagnostics.store(snapshot) }
            } catch {}
        }
        defer {
            diagnosticsPump.cancel()
            diagnosticsStream.cancel()
        }

        // ONE observation, open for the whole scenario. Never reopened: a
        // reopened query would read the settled store and prove nothing
        // about what happened to a live row as each delivery arrived.
        let query = try engine.observe(.single(NMPDemand(selection: filter)))
        let ledger = ObservationLedger(sharedID: shared.id)
        let consumer = Task {
            do {
                for try await batch in query { await ledger.record(batch) }
                await ledger.markEnded("sequence ended")
            } catch {
                await ledger.markEnded("threw: \(error)")
            }
        }
        defer { consumer.cancel() }

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C3 phase log:"] + log).joined(separator: "\n")) }

        // --- Phase 1: exactly one supplier, and the other two are live -----
        //
        // Both halves matter and neither implies the other. `sources ==
        // [relayA]` alone is satisfied by an engine that never dialled B or
        // C at all, which would make phase 2 a first delivery rather than a
        // duplicate one. A wire subscription on B and C alone is satisfied
        // by relays that already had the event.

        let onlyRelayA = await waitUntil(timeout: 30) {
            let state = await ledger.current()
            guard let row = state.shared(shared.id) else { return false }
            return row.sources == [relayA.url] && state.latest.count == 2
        }
        let allThreeSubscribed = await waitUntil(timeout: 30) {
            diagnostics.subscribedRelays().isSuperset(of: [relayA.url, relayB.url, relayC.url])
        }
        // A fact about the FIXTURE, established by an ordinary client REQ
        // against the relay's own wire protocol -- not a reading of NMP's
        // state. B and C are empty of this id, so nothing they could have
        // sent the app can explain the row.
        let bHasItBefore = try await relayB.queryById(shared.id) != nil
        let cHasItBefore = try await relayC.queryById(shared.id) != nil

        let beforeState = await ledger.current()
        let indexBefore = beforeState.sharedIndex(shared.id)
        note(
            "one supplier: onlyRelayA=\(onlyRelayA) allThreeSubscribed=\(allThreeSubscribed) "
                + "subscribed=\(diagnostics.subscribedRelays().count)/3 "
                + "relaysInSnapshot=\(diagnostics.current().relays.count) "
                + "rows=\(beforeState.latest.count) batches=\(beforeState.batches) "
                + "sources=\(beforeState.shared(shared.id)?.sources ?? []) "
                + "sharedIndex=\(indexBefore.map(String.init) ?? "nil") "
                + "relayBHasIt=\(bHasItBefore) relayCHasIt=\(cHasItBefore)"
        )
        XCTAssertTrue(
            onlyRelayA,
            "PRECONDITION: the shared row must be delivered naming ONLY \(relayA.url) before the "
                + "other two relays are seeded. Delivered "
                + "\(beforeState.latest.count) row(s) across \(beforeState.batches) batch(es); the "
                + "shared row's sources are \(beforeState.shared(shared.id)?.sources ?? []). "
                + "Without a first-supplier-only state there is no provenance GROWTH to observe, "
                + "and everything below would pass for an app handed the event by all three at once."
        )
        XCTAssertTrue(
            allThreeSubscribed,
            "PRECONDITION: only \(diagnostics.subscribedRelays().count) of the 3 relays report a "
                + "wire subscription (\(diagnostics.subscribedRelays().sorted())). A relay the "
                + "engine never subscribed to cannot deliver anything, so seeding it below would "
                + "prove nothing about dedup."
        )
        XCTAssertFalse(
            bHasItBefore,
            "PRECONDITION: relay B already holds \(shared.id) before it was seeded -- the relays "
                + "are not independent, so 'the same event arrived from a second relay' is not "
                + "established."
        )
        XCTAssertFalse(
            cHasItBefore,
            "PRECONDITION: relay C already holds \(shared.id) before it was seeded."
        )
        XCTAssertNotNil(
            indexBefore,
            "PRECONDITION: the shared row has no index in the delivered array, so the "
                + "replaced-row check below has nothing to compare against"
        )

        // --- Phase 2: the SAME event, from a second real relay ------------
        //
        // Identical id and identical signature: the very bytes relay A
        // already supplied, written into an independent relay over a real
        // EVENT frame and answered by a real OK. This is the duplicate.

        let acceptedByB = try await relayB.seed(shared)
        let grewToB = await waitUntil(timeout: 30) {
            await ledger.current().shared(shared.id)?.sources.contains(relayB.url) ?? false
        }
        let afterB = await ledger.current()
        note(
            "second supplier: acceptedByB=\(acceptedByB) grewToB=\(grewToB) "
                + "rows=\(afterB.latest.count) maxRows=\(afterB.maxRowCount) "
                + "batches=\(afterB.batches) sources=\(afterB.shared(shared.id)?.sources ?? []) "
                + "sharedIndex=\(afterB.sharedIndex(shared.id).map(String.init) ?? "nil") "
                + "history=\(afterB.sourcesHistory)"
        )
        XCTAssertTrue(acceptedByB, "relay B did not accept the shared event with a real OK")
        XCTAssertTrue(
            grewToB,
            "the row's provenance never grew to name the second relay: 30s after \(relayB.url) "
                + "accepted \(shared.id), the row's sources are still "
                + "\(afterB.shared(shared.id)?.sources ?? []) (history \(afterB.sourcesHistory), "
                + "\(afterB.batches) batches, ended \(afterB.ended ?? "no")). An app cannot tell "
                + "who has an event if the second supplier never appears."
        )
        assertOneCanonicalRow(
            afterB, shared: shared, companion: companion, indexBefore: indexBefore,
            step: "after relay B"
        )

        // --- Phase 3: and from a third ------------------------------------

        let acceptedByC = try await relayC.seed(shared)
        let grewToC = await waitUntil(timeout: 30) {
            await ledger.current().shared(shared.id)?.sources.contains(relayC.url) ?? false
        }
        let afterC = await ledger.current()
        note(
            "third supplier: acceptedByC=\(acceptedByC) grewToC=\(grewToC) "
                + "rows=\(afterC.latest.count) maxRows=\(afterC.maxRowCount) "
                + "batches=\(afterC.batches) sources=\(afterC.shared(shared.id)?.sources ?? []) "
                + "sharedIndex=\(afterC.sharedIndex(shared.id).map(String.init) ?? "nil") "
                + "history=\(afterC.sourcesHistory)"
        )
        XCTAssertTrue(acceptedByC, "relay C did not accept the shared event with a real OK")
        XCTAssertTrue(
            grewToC,
            "the row's provenance never grew to name the third relay: 30s after \(relayC.url) "
                + "accepted \(shared.id), the row's sources are still "
                + "\(afterC.shared(shared.id)?.sources ?? []) (history \(afterC.sourcesHistory))"
        )
        assertOneCanonicalRow(
            afterC, shared: shared, companion: companion, indexBefore: indexBefore,
            step: "after relay C"
        )

        // --- Phase 4: the finished provenance -----------------------------
        //
        // EXACTLY the three relays, sorted and deduplicated as `Row.sources`
        // documents itself to be. A superset would mean a relay that never
        // supplied this event is being credited with it; a subset is the
        // lost-provenance failure.

        let finalRow = afterC.shared(shared.id)
        XCTAssertEqual(
            finalRow?.sources ?? [], [relayA.url, relayB.url, relayC.url].sorted(),
            "the shared row must finish naming exactly the three relays that supplied it, sorted "
                + "and deduplicated. Provenance history across the run: \(afterC.sourcesHistory)"
        )
        // The companion never left relay A, so its provenance must NOT have
        // grown. Without this, an engine that simply unions every relay it
        // ever talked to into every row's `sources` would pass everything
        // above -- and `sources` would mean nothing.
        let companionRow = afterC.latest.first { $0.id == companion.id }
        XCTAssertEqual(
            companionRow?.sources ?? [], [relayA.url],
            "the companion event was only ever written to \(relayA.url), so crediting any other "
                + "relay with it makes `sources` a list of relays the engine knows rather than "
                + "relays that supplied the row. Got \(companionRow?.sources ?? [])"
        )

        consumer.cancel()
        query.cancel()
        for relay in relays { try await relay.kill() }
    }

    // MARK: - The one-row invariant, asserted after every delivery

    /// Everything that must remain true of the shared row no matter how many
    /// relays have supplied it. Called after each provenance growth rather
    /// than once at the end, so a transient second row or a moved row is
    /// caught at the step that caused it.
    private func assertOneCanonicalRow(
        _ state: ObservedState,
        shared: NostrEvent,
        companion: NostrEvent,
        indexBefore: Int?,
        step: String
    ) {
        XCTAssertEqual(
            state.latest.count, 2,
            "\(step): the observation delivered \(state.latest.count) rows against the 2 distinct "
                + "events that exist. More is a duplicate canonical row for the same event id; "
                + "fewer is a lost one. Ids: \(state.latest.map(\.id))"
        )
        XCTAssertEqual(
            Set(state.latest.map(\.id)), [shared.id, companion.id],
            "\(step): the delivered ids must be exactly the two seeded events"
        )
        XCTAssertEqual(
            state.maxRowCount, 2,
            "\(step): some batch carried \(state.maxRowCount) rows. A duplicate canonical row that "
                + "is delivered and then reconciled away is still a duplicate the app rendered."
        )
        XCTAssertEqual(
            state.everSeen, [shared.id, companion.id],
            "\(step): an id was delivered at some point that is not one of the two seeded events"
        )
        XCTAssertNil(
            state.duplicateWitness,
            "\(step): a single batch carried the same id twice: \(state.duplicateWitness ?? "")"
        )
        // The replaced-row check. `sources` growing in place leaves the
        // accumulator's insertion order untouched; removing the row and
        // re-adding it moves it to the end of the delivered array.
        XCTAssertEqual(
            state.sharedIndex(shared.id), indexBefore,
            "\(step): the shared row moved from index \(indexBefore.map(String.init) ?? "nil") to "
                + "\(state.sharedIndex(shared.id).map(String.init) ?? "nil"). An unbounded "
                + "observation folds exact rebased deltas in arrival order, so a row that changes "
                + "position was REMOVED and RE-ADDED rather than having its provenance updated in "
                + "place -- a replaced row, which the row count and the id set both survive."
        )
        // A replacement that re-inserted the event verbatim would also keep
        // the index. These fields say the row still IS the event, not a
        // rebuilt approximation of it.
        let row = state.shared(shared.id)
        XCTAssertEqual(row?.content, shared.content, "\(step): the shared row's content changed")
        XCTAssertEqual(
            row?.createdAt, UInt64(shared.created_at),
            "\(step): the shared row's created_at changed"
        )
        XCTAssertEqual(row?.pubkey, shared.pubkey, "\(step): the shared row's pubkey changed")
        XCTAssertEqual(row?.kind, 1, "\(step): the shared row's kind changed")
        XCTAssertEqual(
            row?.signature, .signed(signature: shared.sig),
            "\(step): the shared row's signature is not the one the author produced"
        )
        XCTAssertEqual(
            row?.sources, row?.sources.sorted(),
            "\(step): `Row.sources` documents itself as sorted; got \(row?.sources ?? [])"
        )
        XCTAssertEqual(
            row?.sources.count, Set(row?.sources ?? []).count,
            "\(step): `Row.sources` documents itself as deduplicated; got \(row?.sources ?? [])"
        )
    }

    /// `setup-strfry.sh`'s own default cache location. Same shape as every
    /// other scenario in this package: skip by name rather than crash.
    private static func locateStrfryBinary() throws -> URL {
        let cacheDir = ProcessInfo.processInfo.environment["RELAY_LAB_CACHE_DIR"]
            ?? (NSHomeDirectory() + "/Library/Caches/nmp-canary-relay-lab")
        let binary = URL(fileURLWithPath: cacheDir).appendingPathComponent("strfry/strfry")
        guard FileManager.default.isExecutableFile(atPath: binary.path) else {
            throw XCTSkip(
                "strfry is not built at \(binary.path) -- run apps/Canary/setup-strfry.sh first"
            )
        }
        return binary
    }
}
