// C14 (docs/internals/canary.md "Scenario status", #1888): NIP-77
// negentropy reconciliation. Two stores holding overlapping-but-different
// event sets converge -- and converge by transferring the DIFFERENCE, not
// by refetching everything.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C13/C15/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection, and nothing here reads the relay's
// database or its log to decide correctness. `canary.md` already records
// strfry's NIP-77 as verified genuine (two processes reconciled
// bidirectionally, real negentropy log output), so the relay side is known
// good going in.
//
// THE TWO STORES, NAMED. NMP's own durable Redb store is one; the relay's
// LMDB store is the other. There is no app-visible "reconcile these two
// peers" call anywhere in the public API -- negentropy is something the
// engine does on its own behalf while serving an ordinary live query -- so
// this is written the way an app actually meets it: hold one feed open
// across a relay outage during which the relay's store moves on.
//
// WHY THE EFFICIENCY CLAIM IS THE WHOLE SCENARIO. "The app eventually saw
// all 70 events" is equally true of a plain REQ that resends all 70 and of
// a negentropy round that transfers 10. Same green result, completely
// different claims, and only the second one is about NIP-77 at all. So the
// oracle is what NMP ACTUALLY TOOK OFF THE WIRE across the reconnect:
// `RelayDiagnostics.eventsByKind`, documented as "events actually RECEIVED
// from a relay, counted by kind", sampled before the outage and again
// after convergence. A refetch shows a delta of ~70; a reconciliation
// shows ~10. That is a public fact, and it is the difference between C14
// and a scenario about reconnects wearing NIP-77's name.
//
// WHY THE RECONNECT, AND NOT A COLD START. This shape was chosen by
// measurement, not taste. The first draft primed the store, shut the
// engine down, seeded the divergence, and restarted -- and it converged
// while receiving all 70 events, with NMP reporting `nip77Behavior =
// behaviorally_proven` and `nip77Handoff = none` throughout. The
// capability probe is asynchronous: a fresh engine places its query's REQ
// as soon as the socket is up, and the probe verdict that would authorize
// a negentropy handoff lands afterwards, by which time the plain REQ has
// already refetched everything. Nothing re-plans the in-flight request.
// That is a real finding and it is recorded in `canary.md`; this scenario
// therefore establishes the probe verdict FIRST and asserts it as a
// precondition, so what it measures afterwards is reconciliation being
// used, not reconciliation being discovered.
//
// THE DIVERGENCE IS ASSERTED, NOT ASSUMED. C13's fourth falsifier again:
// the ten outage-window events are written by a SECOND strfry process over
// the SAME LMDB directory on its own ephemeral port, while the port the app
// is dialing provably refuses a TCP connection -- so "the app was missing
// them" is deterministic by construction rather than a race won, and the
// scenario checks that the app really is holding exactly the 60 at that
// moment.
//
// THE FALSIFIER THIS SCENARIO IS BUILT AROUND is
// `relay.negentropy.enabled = false`. strfry then drops 77 from its NIP-11
// `supported_nips` and refuses NEG-OPEN outright, so the SAME flow still
// converges on the same 70 rows -- over a refetch, with a delta to match.
// `testWithoutNegentropyTheSameFlowRefetchesEverything` runs exactly that
// and asserts the opposite numbers. Neither half proves anything alone.
//
// Every wait is a bounded poll on a real condition with the real stuck
// value reported on timeout -- never a fixed sleep used AS the oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C14Nip77ReconciliationTests: XCTestCase {
    /// Events both stores hold before the outage -- what negentropy gets to
    /// skip, and what a refetch has to resend.
    private static let overlapCount = 60
    /// Events only the relay's store gains, while the app is disconnected.
    private static let divergenceCount = 10

    // MARK: - Observation state

    private struct ObservedState: Sendable {
        var latest: Set<String> = []
        var everSeen: Set<String> = []
        var batches = 0
        var relayStatus = "(never reported)"
        var statusHistory: [String] = []
        var reconciledThrough: UInt64?
        var ended: String?
    }

    private actor ObservationLedger {
        private var state = ObservedState()

        func record(_ batch: RowBatch, relayURL: String) {
            state.batches += 1
            let ids = batch.rows.map(\.id)
            state.latest = Set(ids)
            state.everSeen.formUnion(ids)
            guard let source = batch.evidence.first?.sources
                .first(where: { $0.relay == relayURL })
            else { return }
            let label = Self.label(source.status)
            state.relayStatus = label
            if state.statusHistory.last != label { state.statusHistory.append(label) }
            if let through = source.reconciledThrough { state.reconciledThrough = through }
        }

        func markEnded(_ why: String) { state.ended = why }
        func current() -> ObservedState { state }

        static func label(_ status: SourceStatus) -> String {
            switch status {
            case .requesting: return "requesting"
            case .finishedStoredEvents: return "finishedStoredEvents"
            case .awaitingRequest: return "awaitingRequest"
            case .coverageSatisfied: return "coverageSatisfied"
            case .connecting: return "connecting"
            case .disconnected: return "disconnected"
            case .awaitingAuth: return "awaitingAuth"
            case .authDenied: return "authDenied"
            case .error: return "error"
            }
        }
    }

    /// `observeDiagnostics()` is PUSH-only (C17's finding; C13 and C15 each
    /// carry these same lines on purpose). This one additionally keeps the
    /// UNION of every NIP-77 state string ever pushed for the lab relay,
    /// because `nip77Handoff` is a transient: a snapshot read after
    /// reconciliation finished says `live` and nothing about how it got
    /// there, so a scenario that only samples the latest value cannot see
    /// the reconciliation it is trying to prove happened.
    private final class LatestDiagnostics: @unchecked Sendable {
        private let lock = NSLock()
        private var value = DiagnosticsSnapshot()
        private var advertisements: [String] = []
        private var behaviors: [String] = []
        private var handoffs: [String] = []

        func store(_ snapshot: DiagnosticsSnapshot) {
            lock.lock()
            value = snapshot
            for relay in snapshot.relays {
                if !advertisements.contains(relay.nip77Advertisement) {
                    advertisements.append(relay.nip77Advertisement)
                }
                if !behaviors.contains(relay.nip77Behavior) { behaviors.append(relay.nip77Behavior) }
                if !handoffs.contains(relay.nip77Handoff) { handoffs.append(relay.nip77Handoff) }
            }
            lock.unlock()
        }

        func current() -> DiagnosticsSnapshot {
            lock.lock()
            defer { lock.unlock() }
            return value
        }

        func behavior(for relayURL: String) -> String {
            current().relays.first { $0.relay == relayURL }?.nip77Behavior ?? "(no relay row)"
        }

        func nip77History() -> (advertisement: [String], behavior: [String], handoff: [String]) {
            lock.lock()
            defer { lock.unlock() }
            return (advertisements, behaviors, handoffs)
        }

        /// Events NMP actually took off `relayURL`'s wire for `kind`, summed
        /// across access contexts (this scenario has exactly one).
        func received(kind: UInt16, from relayURL: String) -> UInt64 {
            current().relays
                .filter { $0.relay == relayURL }
                .flatMap(\.eventsByKind)
                .filter { $0.kind == kind }
                .reduce(UInt64(0)) { $0 + $1.count }
        }

        func describe(relayURL: String) -> String {
            let history = nip77History()
            let rows = current().relays.filter { $0.relay == relayURL }.map {
                "  wireSubs=\($0.wireSubCount) "
                    + "nip11Nips=\($0.nip11SupportedNips.map(String.init(describing:)) ?? "nil") "
                    + "advertisement=\($0.nip77Advertisement) behavior=\($0.nip77Behavior) "
                    + "handoff=\($0.nip77Handoff) "
                    + "eventsByKind=\($0.eventsByKind.map { "\($0.kind):\($0.count)" })"
            }
            return (["relay diagnostics for \(relayURL):"] + rows + [
                "  advertisement history=\(history.advertisement)",
                "  behavior history=\(history.behavior)",
                "  handoff history=\(history.handoff)",
            ]).joined(separator: "\n")
        }
    }

    @discardableResult
    private func waitUntil(
        timeout: TimeInterval = 60,
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

    func testDivergentStoresConvergeByTransferringOnlyTheDifference() async throws {
        let run = try await reconcile(name: "c14", negentropyEnabled: true)
        print(run.log)

        try assertSharedPreconditions(run)

        // The relay is a NIP-77 peer AND NMP has already proven it
        // behaviorally, BEFORE the divergence exists. This is the
        // precondition the first draft of this scenario lacked, and its
        // absence is exactly what made that draft silently measure a
        // refetch.
        XCTAssertTrue(
            run.advertisementHistory.contains("advertised_supported"),
            "PRECONDITION: NMP never saw this relay advertise NIP-77 "
                + "(advertisement history \(run.advertisementHistory)).\n" + run.diagnostics
        )
        XCTAssertEqual(
            run.behaviorBeforeOutage, "behaviorally_proven",
            "PRECONDITION: NMP's NIP-77 verdict for this relay was "
                + "'\(run.behaviorBeforeOutage)' when the outage began, not "
                + "'behaviorally_proven'. The handoff is only reachable for a relay already "
                + "carrying a behaviorally-minted verdict, so anything measured after this point "
                + "would be measuring the absence of a probe result, not the presence of a "
                + "refetch.\n" + run.diagnostics
        )

        XCTAssertTrue(
            run.converged,
            "the app did not converge: it holds \(run.finalIDs.count) of "
                + "\(run.expectedIDs.count), missing "
                + "\(run.expectedIDs.subtracting(run.finalIDs).count). "
                + "status=\(run.relayStatus) history=\(run.statusHistory) ended=\(run.ended)\n"
                + run.diagnostics
        )
        XCTAssertEqual(
            run.finalIDs.count, run.expectedIDs.count,
            "the converged row set holds \(run.finalIDs.count) rows against \(run.expectedIDs.count) "
                + "seeded ids (\(run.expectedIDs.subtracting(run.finalIDs).count) missing, "
                + "\(run.finalIDs.subtracting(run.expectedIDs).count) unexpected). Above the id "
                + "count this is a duplicate canonical row; below it, a lost event."
        )

        // IT RECONCILED, IT DID NOT REFETCH. The claim C14 exists for.
        XCTAssertTrue(
            run.handoffHistory.contains("reconciling") || run.handoffHistory.contains("backfilling"),
            "NMP never reported this relay in a `reconciling`/`backfilling` handoff state across "
                + "the reconnect, so no negentropy session was ever opened for this query. "
                + "handoff history=\(run.handoffHistory)\n" + run.diagnostics
        )
        XCTAssertLessThan(
            run.eventsReceivedAcrossOutage, UInt64(Self.overlapCount),
            "NMP took \(run.eventsReceivedAcrossOutage) kind-1 events off the wire across the "
                + "reconnect to converge on \(run.divergenceIDs.count) it was missing. At or above "
                + "the \(Self.overlapCount)-event overlap this is a REFETCH of material the local "
                + "store already had, whatever the NIP-77 diagnostics say, and C14's actual claim "
                + "-- converge efficiently rather than by refetching everything -- is false.\n"
                + run.diagnostics
        )
        // Sharper than "fewer than the overlap": the transfer should be the
        // difference. Bounded generously (twice the divergence) because a
        // negentropy round legitimately re-sends a small amount around
        // range boundaries; the point is the ORDER of the number.
        XCTAssertLessThanOrEqual(
            run.eventsReceivedAcrossOutage, UInt64(Self.divergenceCount * 2),
            "NMP transferred \(run.eventsReceivedAcrossOutage) events for a "
                + "\(Self.divergenceCount)-event difference. Reconciliation happened, but not "
                + "efficiently.\n" + run.diagnostics
        )
    }

    /// The other half of the same claim, and the reason the numbers above
    /// mean anything. With negentropy switched off at the relay, the SAME
    /// flow still converges on the same 70 rows -- by resending the whole
    /// set. If this test failed to converge, the one above would be proving
    /// that reconnects work rather than that reconciliation is what closed
    /// the gap; if it showed the same small delta, the delta would not be
    /// evidence of NIP-77 at all.
    func testWithoutNegentropyTheSameFlowRefetchesEverything() async throws {
        let run = try await reconcile(name: "c14-noneg", negentropyEnabled: false)
        print(run.log)

        try assertSharedPreconditions(run)
        XCTAssertTrue(
            run.advertisementHistory.contains("advertised_unsupported"),
            "PRECONDITION: with `relay.negentropy.enabled = false` strfry drops 77 from its NIP-11 "
                + "`supported_nips`, and NMP must see that. advertisement history="
                + "\(run.advertisementHistory)\n" + run.diagnostics
        )

        XCTAssertTrue(
            run.converged,
            "with NIP-77 unavailable the app must STILL converge, over an ordinary REQ: it holds "
                + "\(run.finalIDs.count)/\(run.expectedIDs.count). status=\(run.relayStatus)\n"
                + run.diagnostics
        )
        XCTAssertNotEqual(
            run.behaviorBeforeOutage, "behaviorally_proven",
            "the relay refuses NEG-OPEN, yet NMP reported NIP-77 `behaviorally_proven` for it. "
                + "Then that string is not a report of a real negentropy exchange, and the passing "
                + "precondition in the scenario above does not mean what it says.\n" + run.diagnostics
        )
        XCTAssertFalse(
            run.handoffHistory.contains("reconciling"),
            "NMP reported a `reconciling` handoff against a relay that refuses NEG-OPEN.\n"
                + run.diagnostics
        )
        XCTAssertGreaterThanOrEqual(
            run.eventsReceivedAcrossOutage, UInt64(Self.overlapCount),
            "without negentropy, the reconnect took only \(run.eventsReceivedAcrossOutage) events "
                + "off the wire for a \(Self.expectedTotal)-event set. Then a plain REQ is already "
                + "transferring only the difference here, and the small delta the NIP-77 scenario "
                + "asserts is not evidence of reconciliation.\n" + run.diagnostics
        )
    }

    private static var expectedTotal: Int { overlapCount + divergenceCount }

    private func assertSharedPreconditions(_ run: Run) throws {
        // Counts, not id sets: an inequality here prints two 60-element hex
        // lists that nobody can diff by eye, and the numbers say everything.
        XCTAssertEqual(
            run.idsBeforeOutage.count, run.overlapIDs.count,
            "PRECONDITION: when the relay went down the app held \(run.idsBeforeOutage.count) rows "
                + "against the \(run.overlapIDs.count) overlap events "
                + "(\(run.overlapIDs.subtracting(run.idsBeforeOutage).count) missing, "
                + "\(run.idsBeforeOutage.subtracting(run.overlapIDs).count) unexpected). "
                + "Reconciliation against a local side that is not the overlap is measuring "
                + "something else."
        )
        XCTAssertTrue(
            run.overlapIDs.isSubset(of: run.idsBeforeOutage),
            "PRECONDITION: the app was missing "
                + "\(run.overlapIDs.subtracting(run.idsBeforeOutage).count) of the overlap events "
                + "when the relay went down"
        )
        XCTAssertTrue(
            run.portDeadDuringWrite,
            "PRECONDITION: the app's relay port accepted a TCP connection while the sidecar was "
                + "writing the divergence, so the app could have been served those events live and "
                + "'the two stores diverged' is not established."
        )
        XCTAssertTrue(
            run.idsBeforeOutage.isDisjoint(with: run.divergenceIDs),
            "PRECONDITION: the app already held "
                + "\(run.idsBeforeOutage.intersection(run.divergenceIDs).count) outage-window "
                + "events before the outage ended. There is no divergence to reconcile."
        )
        XCTAssertEqual(
            run.relayHeldIDs.count, run.expectedIDs.count,
            "PRECONDITION: a plain NMP-free client found \(run.relayHeldIDs.count) events at the "
                + "relay after the restart, not the \(run.expectedIDs.count) seeded "
                + "(\(run.expectedIDs.subtracting(run.relayHeldIDs).count) missing). 'Converged' "
                + "would be convergence on less than the whole set."
        )
        XCTAssertTrue(
            run.expectedIDs.isSubset(of: run.relayHeldIDs),
            "PRECONDITION: the relay is missing "
                + "\(run.expectedIDs.subtracting(run.relayHeldIDs).count) seeded events"
        )
    }

    // MARK: - The run

    private struct Run {
        var overlapIDs: Set<String> = []
        var divergenceIDs: Set<String> = []
        var expectedIDs: Set<String> = []
        var idsBeforeOutage: Set<String> = []
        var relayHeldIDs: Set<String> = []
        var finalIDs: Set<String> = []
        var portDeadDuringWrite = false
        var converged = false
        var behaviorBeforeOutage = ""
        var eventsReceivedBeforeOutage: UInt64 = 0
        var eventsReceivedAcrossOutage: UInt64 = 0
        var advertisementHistory: [String] = []
        var behaviorHistory: [String] = []
        var handoffHistory: [String] = []
        var relayStatus = ""
        var statusHistory: [String] = []
        var ended = ""
        var diagnostics = ""
        var log = ""
    }

    private func reconcile(name: String, negentropyEnabled: Bool) async throws -> Run {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-\(name)-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        let sidecarDir = root.appendingPathComponent("sidecar")
        let storeDir = root.appendingPathComponent("store")
        for dir in [relayDir, sidecarDir, storeDir] {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        }
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "\(name)-relay", workDir: relayDir, binaryPath: binaryPath)
        try relay.overrideConfig(
            Self.config(dataDir: relay.dataDir, port: relay.port, negentropy: negentropyEnabled)
        )
        try await relay.start()

        var run = Run()
        var log: [String] = ["", "C14 (\(name), relay negentropy=\(negentropyEnabled)) phase log:"]

        let author = try NostrKeyPair()
        let filter = NMPFilter(kinds: [1], authors: .literal([author.pubkeyHex]))
        let wireFilter: [String: Any] = ["kinds": [1], "authors": [author.pubkeyHex], "limit": 500]

        // --- The overlap: what BOTH stores will hold ----------------------
        //
        // Distinct `created_at` per event so every id is distinct and the
        // relay's ordering is stable. Real EVENT frames with real OKs
        // (`RelayHandle.seed` throws otherwise); a seed that "succeeds"
        // without one is not evidence of anything.
        let base = UInt64(Date().timeIntervalSince1970) - UInt64(Self.expectedTotal + 10)
        var overlap: [NostrEvent] = []
        for index in 0..<Self.overlapCount {
            let event = try NostrSigning.sign(
                keyPair: author, kind: 1, content: "C14 overlap #\(index)",
                createdAt: Int64(base + UInt64(index))
            )
            try await relay.seed(event)
            overlap.append(event)
        }
        run.overlapIDs = Set(overlap.map(\.id))

        // --- One engine, one query, open for the whole scenario -----------

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storeDir.appendingPathComponent("nmp.redb").path, appRelays: []
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

        let query = try engine.observe(.single(NMPDemand(selection: filter, routing: .explicit([relay.url]))))
        let ledger = ObservationLedger()
        let consumer = Task {
            do {
                for try await batch in query { await ledger.record(batch, relayURL: relay.url) }
                await ledger.markEnded("sequence ended")
            } catch {
                await ledger.markEnded("threw: \(error)")
            }
        }
        defer { consumer.cancel() }

        let warmed = await waitUntil(timeout: 90) {
            await ledger.current().latest == run.overlapIDs
        }
        // The NIP-77 capability probe is asynchronous and independent of
        // this query: it is begun on connect and its verdict arrives when it
        // arrives. Waiting for it HERE, before the divergence exists, is
        // what makes the measurement afterwards a measurement of
        // reconciliation being used rather than of it still being
        // discovered. Bounded, and the falsifier run legitimately never
        // reaches `behaviorally_proven`, so this waits for the probe to stop
        // being in flight rather than for one particular verdict.
        let probeSettled = await waitUntil(timeout: 45) {
            let verdict = diagnostics.behavior(for: relay.url)
            return verdict == "behaviorally_proven" || verdict == "behaviorally_rejected"
                || diagnostics.nip77History().advertisement.contains("advertised_unsupported")
        }
        run.behaviorBeforeOutage = diagnostics.behavior(for: relay.url)
        run.eventsReceivedBeforeOutage = diagnostics.received(kind: 1, from: relay.url)
        var state = await ledger.current()
        run.idsBeforeOutage = state.latest
        log.append(
            "warm: rows=\(state.latest.count)/\(run.overlapIDs.count) warmed=\(warmed) "
                + "probeSettled=\(probeSettled) behavior=\(run.behaviorBeforeOutage) "
                + "received=\(run.eventsReceivedBeforeOutage) status=\(state.relayStatus)"
        )

        // --- The divergence, written while the app is provably offline ----
        //
        // SIGKILL of the relay's OS process (a frozen process keeps its TCP
        // connections open, so the app would not see a disconnect at all),
        // then a SECOND strfry process on its own ephemeral port over the
        // SAME LMDB directory. The app has never been told that port exists
        // and its own port refuses a real TCP connection, so it cannot have
        // been served these events. Deterministic by construction -- C13's
        // mechanism, for the same reason.

        try await relay.kill()
        let portDead = await waitUntil(timeout: 10) { await !relay.isReachable() }

        var divergence: [NostrEvent] = []
        let sidecar = try await RelayHandle(
            name: "\(name)-sidecar", workDir: sidecarDir, binaryPath: binaryPath,
            dataDir: relay.dataDir
        )
        try sidecar.overrideConfig(
            Self.config(dataDir: relay.dataDir, port: sidecar.port, negentropy: negentropyEnabled)
        )
        try await sidecar.start()
        for index in 0..<Self.divergenceCount {
            let event = try NostrSigning.sign(
                keyPair: author, kind: 1, content: "C14 divergence #\(index)",
                createdAt: Int64(base + UInt64(Self.overlapCount + index))
            )
            try await sidecar.seed(event)
            divergence.append(event)
        }
        let stillDead = await !relay.isReachable()
        run.portDeadDuringWrite = portDead && stillDead
        try await sidecar.kill()
        run.divergenceIDs = Set(divergence.map(\.id))
        run.expectedIDs = run.overlapIDs.union(run.divergenceIDs)
        state = await ledger.current()
        run.idsBeforeOutage = state.latest
        log.append(
            "outage: portDead=\(portDead) stillDead=\(run.portDeadDuringWrite) "
                + "appHolds=\(state.latest.count) seededDuringOutage=\(run.divergenceIDs.count)"
        )

        // --- The relay returns, and the app must close the gap ------------

        try await relay.restart()
        let relayBack = await relay.isReachable(timeout: 5)

        // PRECONDITION, relay half: a plain NMP-free client, so "the relay
        // has all 70" is not something NMP is trusted to report about
        // itself.
        let probe = try await relay.probeRead(filter: wireFilter, timeout: 20)
        run.relayHeldIDs = Set(probe.servedEventIDs)
        log.append("relay probe: relayBack=\(relayBack) served=\(run.relayHeldIDs.count) eose=\(probe.reachedEOSE)")

        run.converged = await waitUntil(timeout: 120) {
            await ledger.current().latest == run.expectedIDs
        }
        // Sampled only after convergence, plus a short settle so a trailing
        // batch cannot make the count look smaller than it really was. This
        // sleep paces the measurement; it is never the thing being waited on.
        try? await Task.sleep(nanoseconds: 2_000_000_000)
        state = await ledger.current()
        let history = diagnostics.nip77History()
        run.finalIDs = state.latest
        run.eventsReceivedAcrossOutage =
            diagnostics.received(kind: 1, from: relay.url) - run.eventsReceivedBeforeOutage
        run.advertisementHistory = history.advertisement
        run.behaviorHistory = history.behavior
        run.handoffHistory = history.handoff
        run.relayStatus = state.relayStatus
        run.statusHistory = state.statusHistory
        run.ended = state.ended ?? "no"
        run.diagnostics = diagnostics.describe(relayURL: relay.url)
        log.append(
            "converge: \(run.converged) rows=\(run.finalIDs.count)/\(run.expectedIDs.count) "
                + "receivedAcrossOutage=\(run.eventsReceivedAcrossOutage) "
                + "(\(run.eventsReceivedBeforeOutage) -> "
                + "\(diagnostics.received(kind: 1, from: relay.url))) "
                + "status=\(run.relayStatus) history=\(run.statusHistory) "
                + "reconciledThrough=\(state.reconciledThrough.map(String.init) ?? "nil")"
        )
        log.append(run.diagnostics)
        run.log = log.joined(separator: "\n")

        query.cancel()
        consumer.cancel()
        try await relay.kill()
        return run
    }

    private static func config(dataDir: URL, port: UInt16, negentropy: Bool) -> String {
        """
        db = "\(dataDir.path)/"
        relay {
            bind = "127.0.0.1"
            port = \(port)
            maxFilterLimit = 1000
            info {
                name = "canary-c14-negentropy-relay"
                description = "NIP-77 reconciliation lab relay"
            }
            negentropy {
                enabled = \(negentropy)
            }
        }
        """
    }

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
