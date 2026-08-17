// C13 (docs/internals/canary.md "Scenario status"): relay disconnect and
// reconnect -- a real strfry OS process dies and comes back on the same
// port over the same LMDB directory, and the app's subscriptions must
// resume, deliver what arrived while it was gone, and never duplicate a
// row.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The relay is reached only over a real
// `ws://` URL to a separate process.
//
// TWO CONCURRENT OBSERVATIONS SHARING ONE QUERY. Both `NMPQuery` handles
// below are opened over the IDENTICAL `NMPFilter`, so they share whatever
// wire subscription the engine derives from that filter, and they are
// consumed by two independent tasks for the whole scenario. Nothing else
// in this repository does that: the scale tests all use one handle per
// key, which is exactly why they missed #1848 -- a shared-subscription
// lifecycle defect where a demand was never closed. Sharing is only
// interesting if the sharers are then taken apart unevenly, so the last
// two phases close ONE sharer and require the other to keep working
// (a shared demand must not be withdrawn by a non-last holder), then
// close the last one and require the wire subscription to go away
// (a shared demand must be withdrawn by the last holder). Neither half is
// provable without the other.
//
// THE PRECONDITIONS ARE ASSERTED, NOT ASSUMED. C17's first draft ended
// each cycle before the subscription had ever reached the relay, and the
// only thing that caught it was a liveness assertion. A reconnect test is
// exposed to the same class of vacuity twice over, so this scenario
// asserts, with the real captured numbers:
//
//   1. the subscription was GENUINELY LIVE before the outage --
//      `observeDiagnostics()` reported >= 1 wire subscription AND both
//      observations had actually been delivered the seeded event;
//   2. the subscription was GENUINELY SEVERED during it -- the port stops
//      accepting a real TCP connection, and both observations' own
//      `SourceStatus` for that relay reports the failure
//      (`disconnected` -> `error`, measured) rather than still claiming a
//      source that cannot answer.
//
// AN API FINDING FOUND BY WRITING (2). The first draft of the severed
// precondition also required `RelayDiagnostics.wireSubCount` to fall to
// zero, on the reasonable-looking theory that a dead socket holds no wire
// subscriptions. It does not: measured at 1 for the whole 30s outage,
// with the relay row still present in the snapshot. The number counts
// subscriptions this relay is PLANNED to hold -- which is not a lie,
// since NMP is retrying -- but it means the engine-global diagnostics
// stream cannot answer "is this relay's subscription established right
// now", and the only public fact that can is per-QUERY acquisition
// evidence, which an app must already be holding a query to see. That is
// #755's subject. The assertion was removed rather than inverted: the
// scenario records the number and does not pretend either value is the
// contract.
//
// AND THE EVENT REALLY DID ARRIVE DURING THE OUTAGE. "The relay came back
// and the app then saw a new event" is not the claim; a subscription that
// silently re-established before the event was published would prove that
// just as well. So the outage-window event is written into the relay's
// LMDB directory by a SECOND strfry process on a DIFFERENT ephemeral port
// (`RelayHandle(dataDir:)`), while the port the app knows is provably
// dead. The app cannot have seen it: nothing was listening where the app
// was dialing, and the app has never been told the sidecar's port exists.
// That is deterministic by construction, not a race won.
//
// Every wait below is a bounded poll on a real condition with the real
// stuck value reported on timeout -- never a fixed sleep used AS the
// synchronization oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C13RelayDisconnectReconnectTests: XCTestCase {
    // MARK: - What each of the two sharing observations has seen

    /// One observation's delivered state. `latest` is the newest batch's
    /// row set (an `NMPQuery` element is the full current snapshot, not a
    /// delta) and `everSeen` is the union across every batch, because
    /// "no lost events" and "no duplicate rows" are different questions:
    /// a row that appears and then vanishes is a loss the latest snapshot
    /// alone would hide.
    private struct ObservedState: Sendable {
        var latest: Set<String> = []
        var everSeen: Set<String> = []
        var latestRowCount = 0
        var batches = 0
        /// Non-nil iff some batch ever carried the same event id twice.
        var duplicateWitness: String?
        /// The newest `SourceStatus` this observation reported for the lab
        /// relay, as a short label -- the query's OWN view of the link,
        /// independent of the engine-global diagnostics stream.
        var relayStatus = "(never reported)"
        /// Every distinct status label this observation has reported, in
        /// arrival order. Printed as evidence.
        var statusHistory: [String] = []
        var ended: String?
    }

    private actor ObservationLedger {
        private var states: [Int: ObservedState] = [1: ObservedState(), 2: ObservedState()]

        func record(_ observer: Int, batch: RowBatch, relayURL: String) {
            var state = states[observer] ?? ObservedState()
            state.batches += 1
            let ids = batch.rows.map(\.id)
            if Set(ids).count != ids.count {
                state.duplicateWitness = ids.joined(separator: " ")
            }
            state.latest = Set(ids)
            state.latestRowCount = ids.count
            state.everSeen.formUnion(ids)
            if let source = batch.evidence.first?.sources.first(where: { $0.relay == relayURL }) {
                let label = Self.label(source.status)
                state.relayStatus = label
                if state.statusHistory.last != label {
                    state.statusHistory.append(label)
                }
            }
            states[observer] = state
        }

        func markEnded(_ observer: Int, _ why: String) {
            states[observer]?.ended = why
        }

        func state(_ observer: Int) -> ObservedState {
            states[observer] ?? ObservedState()
        }

        /// A short label per `SourceStatus` case. Deliberately spelled out
        /// rather than `String(describing:)` so the "is this link working"
        /// classification below is a decision this scenario makes in the
        /// open, not one inherited from a synthesized description.
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

        /// The statuses that mean "this source is currently serving this
        /// query". Everything else is a source that is not working right
        /// now, which is what the outage must produce.
        static func isWorking(_ label: String) -> Bool {
            label == "requesting" || label == "finishedStoredEvents"
                || label == "coverageSatisfied"
        }

        /// The two statuses that mean the LINK ITSELF failed, as opposed to
        /// merely not having got there yet. "Not working" alone would be
        /// satisfied by `awaitingRequest`, which an observation can report
        /// while everything is perfectly healthy, so the outage assertion
        /// requires one of these to have been reported.
        static func isSevered(_ label: String) -> Bool {
            label == "disconnected" || label == "error"
        }
    }

    /// `observeDiagnostics()` is PUSH-only -- there is no synchronous "what
    /// is your current snapshot" call on `NMPEngine`, so an application that
    /// wants a point-in-time reading has to hold the stream open and keep
    /// the last value it was handed. This box is exactly that and nothing
    /// more. C17's churner contains the same nine lines; duplicating them
    /// is deliberate (`docs/internals/canary.md`: "a little duplication is
    /// preferable to hiding evidence"), because the duplication IS the
    /// evidence that every scenario needing a current resource reading has
    /// to build this itself.
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

        func wireSubCount() -> UInt32 {
            current().relays.reduce(UInt32(0)) { $0 + $1.wireSubCount }
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

    func testTwoSharedObservationsResumeAcrossARealRelayOutage() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c13-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        let sidecarDir = root.appendingPathComponent("sidecar")
        let storeDir = root.appendingPathComponent("store")
        try FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: sidecarDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: storeDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c13-relay", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()

        let keyPair = try NostrKeyPair()
        let filter = NMPFilter(kinds: [1], authors: .literal([keyPair.pubkeyHex]))

        // `before` is seeded ahead of the engine, over a real EVENT frame.
        // The two later events are signed at the moment they are published,
        // so their `created_at` genuinely follows the outage boundary.
        let before = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "C13 before the outage")
        try await relay.seed(before)

        let engine = try NMPEngine(
            config: NMPConfig(storePath: storeDir.appendingPathComponent("nmp.redb").path,
                              appRelays: [relay.url])
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

        // TWO observations over the IDENTICAL filter -- the shape nothing
        // else in this repository exercises. Both stay open across the
        // whole outage; neither is reopened afterwards, because a reopened
        // query proves the engine can start a subscription, not that it
        // resumed one.
        let sharerOne = try engine.observe(.single(NMPDemand(selection: filter)))
        let sharerTwo = try engine.observe(.single(NMPDemand(selection: filter)))

        let ledger = ObservationLedger()
        let consumers = Task {
            await withTaskGroup(of: Void.self) { group in
                group.addTask {
                    do {
                        for try await batch in sharerOne {
                            await ledger.record(1, batch: batch, relayURL: relay.url)
                        }
                        await ledger.markEnded(1, "sequence ended")
                    } catch {
                        await ledger.markEnded(1, "threw: \(error)")
                    }
                }
                group.addTask {
                    do {
                        for try await batch in sharerTwo {
                            await ledger.record(2, batch: batch, relayURL: relay.url)
                        }
                        await ledger.markEnded(2, "sequence ended")
                    } catch {
                        await ledger.markEnded(2, "threw: \(error)")
                    }
                }
            }
        }
        defer { consumers.cancel() }

        var log: [String] = []
        func note(_ line: String) {
            log.append(line)
        }
        defer { print((["", "C13 phase log:"] + log).joined(separator: "\n")) }

        // --- Phase 1: the subscription is GENUINELY LIVE ------------------
        //
        // Two independent facts, because either one alone can be true while
        // the other is false: rows can arrive purely from the local store
        // with nothing on the wire (C17's `rows=0` trap in reverse), and a
        // wire subscription can exist while nothing has been delivered.

        let bothSawBefore = await waitUntil(timeout: 30) {
            let one = await ledger.state(1).latest
            let two = await ledger.state(2).latest
            return one.contains(before.id) && two.contains(before.id)
        }
        let liveWireSubs = await waitUntil(timeout: 30) { diagnostics.wireSubCount() >= 1 }
        let wireSubsWhileLive = diagnostics.wireSubCount()
        let stateOneLive = await ledger.state(1)
        let stateTwoLive = await ledger.state(2)
        note(
            "live: bothSawBefore=\(bothSawBefore) wireSubs=\(wireSubsWhileLive) "
                + "relays=\(diagnostics.current().relays.count) "
                + "sharer1=\(stateOneLive.latestRowCount) rows/\(stateOneLive.batches) batches/"
                + "status \(stateOneLive.relayStatus) "
                + "sharer2=\(stateTwoLive.latestRowCount) rows/\(stateTwoLive.batches) batches/"
                + "status \(stateTwoLive.relayStatus)"
        )
        XCTAssertTrue(
            bothSawBefore,
            "PRECONDITION: both sharing observations must have been delivered the seeded event "
                + "before the outage. sharer1 latest=\(stateOneLive.latest) "
                + "(\(stateOneLive.batches) batches, status \(stateOneLive.relayStatus)), "
                + "sharer2 latest=\(stateTwoLive.latest) (\(stateTwoLive.batches) batches, "
                + "status \(stateTwoLive.relayStatus)). Everything after this point would be "
                + "asserted against an observation that was never working."
        )
        XCTAssertTrue(
            liveWireSubs && wireSubsWhileLive >= 1,
            "PRECONDITION: the engine reported \(wireSubsWhileLive) wire subscription(s) across "
                + "\(diagnostics.current().relays.count) relay(s) while both observations were "
                + "open. With nothing on the wire there is no subscription for the outage to "
                + "sever, and the reconnect half of this scenario would be vacuous."
        )
        // The scenario's own name has to be true: TWO observations, ONE
        // shared wire subscription. If the engine gave each observation its
        // own subscription, phases 5 and 6 would be testing two independent
        // demands and would say nothing at all about sharing -- the exact
        // blind spot that let #1848 through. This is a public fact off
        // `observeDiagnostics()`, not an assumption about internals.
        XCTAssertEqual(
            wireSubsWhileLive, 1,
            "PRECONDITION: two observations over the IDENTICAL filter produced "
                + "\(wireSubsWhileLive) wire subscription(s), so they are not sharing one. "
                + "Everything this scenario claims about shared-demand lifecycle would be vacuous."
        )

        // --- Phase 2: a REAL outage, and it is GENUINELY SEVERED ----------
        //
        // SIGKILL of the relay's OS process, not `partition()`: a frozen
        // process keeps its TCP connections open, so the app would not see
        // a disconnect at all. This closes the sockets for real.

        try await relay.kill()

        let portDead = await waitUntil(timeout: 10) { await !relay.isReachable() }
        let bothReportSevered = await waitUntil(timeout: 30) {
            let one = await ledger.state(1)
            let two = await ledger.state(2)
            return !ObservationLedger.isWorking(one.relayStatus)
                && !ObservationLedger.isWorking(two.relayStatus)
                && one.statusHistory.contains(where: ObservationLedger.isSevered)
                && two.statusHistory.contains(where: ObservationLedger.isSevered)
        }
        let stateOneOutage = await ledger.state(1)
        let stateTwoOutage = await ledger.state(2)
        // NOT an assertion -- a recorded observation, and an API finding.
        // `RelayDiagnostics.wireSubCount` does NOT fall to zero while the
        // relay's socket is dead: measured at 1 across the whole 30s outage
        // below, with the relay row still present. It counts subscriptions
        // this relay is PLANNED to hold, and NMP is meanwhile retrying, so
        // "1" is not a lie -- but it means the engine-global diagnostics
        // stream cannot answer "is this relay's subscription actually
        // established right now". The per-query `SourceEvidence.status`
        // above is the only public fact that can, and it needs a query.
        // Recorded in docs/internals/canary.md; the gap is #755's subject.
        let wireSubsDuringOutage = diagnostics.wireSubCount()
        note(
            "outage: portDead=\(portDead) wireSubs=\(wireSubsDuringOutage) (NOT asserted -- see "
                + "the finding at this line) relays=\(diagnostics.current().relays.count) "
                + "transportDegraded=\(diagnostics.current().transportDegraded ?? "nil") "
                + "sharer1 status=\(stateOneOutage.relayStatus) history=\(stateOneOutage.statusHistory) "
                + "sharer2 status=\(stateTwoOutage.relayStatus) history=\(stateTwoOutage.statusHistory)"
        )
        XCTAssertTrue(
            portDead,
            "PRECONDITION: \(relay.url) still accepted a real TCP connection after the relay "
                + "process was SIGKILLed -- there was no outage to recover from."
        )
        XCTAssertTrue(
            bothReportSevered,
            "PRECONDITION: both observations must report, through their own acquisition evidence, "
                + "that the source failed -- sharer1 is \(stateOneOutage.relayStatus) (history "
                + "\(stateOneOutage.statusHistory)), sharer2 is \(stateTwoOutage.relayStatus) "
                + "(history \(stateTwoOutage.statusHistory)). Without a severed subscription there "
                + "is nothing for the reconnect below to resume, and it would pass regardless."
        )

        // --- Phase 3: an event arrives at the relay DURING the outage -----
        //
        // A second strfry process over the SAME LMDB directory on its OWN
        // ephemeral port. Seeded over a real EVENT frame with a real OK, so
        // the relay's durable store genuinely holds it. The app cannot have
        // received it: its own relay URL is provably dead (asserted above
        // and again after), and it has never heard of this port.

        let during = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "C13 during the outage")
        let sidecar = try await RelayHandle(
            name: "c13-sidecar", workDir: sidecarDir, binaryPath: binaryPath, dataDir: relay.dataDir
        )
        try await sidecar.start()
        let seeded = try await sidecar.seed(during)
        try await sidecar.kill()

        let appPortStillDead = await !relay.isReachable()
        let everSeenOneDuringOutage = await ledger.state(1).everSeen
        let everSeenTwoDuringOutage = await ledger.state(2).everSeen
        let sawDuringWhileDisconnected = everSeenOneDuringOutage.contains(during.id)
            || everSeenTwoDuringOutage.contains(during.id)
        note(
            "outage-window write: seeded=\(seeded) appPortStillDead=\(appPortStillDead) "
                + "appAlreadySawIt=\(sawDuringWhileDisconnected) id=\(during.id)"
        )
        XCTAssertTrue(seeded, "the sidecar relay did not accept the outage-window event")
        XCTAssertTrue(
            appPortStillDead,
            "PRECONDITION: \(relay.url) became reachable while the sidecar was writing -- the "
                + "app could have been connected, so 'arrived during the outage' is not established."
        )
        XCTAssertFalse(
            sawDuringWhileDisconnected,
            "PRECONDITION: an observation was delivered \(during.id) while the relay was dead, "
                + "which means it did not arrive during an outage at all."
        )

        // --- Phase 4: the relay returns; both sharers must resume ---------
        //
        // Same port, same LMDB directory, a brand new OS process. Nothing
        // in the app is touched: no reopened query, no new engine, no
        // app-side retry. If NMP does not resume the subscription by
        // itself, this is where it fails.

        try await relay.restart()
        let relayBack = await relay.isReachable(timeout: 5)

        let bothSawDuring = await waitUntil(timeout: 60) {
            let one = await ledger.state(1).latest
            let two = await ledger.state(2).latest
            return one.contains(during.id) && two.contains(during.id)
        }
        let wireSubsAfterReconnect = diagnostics.wireSubCount()
        let stateOneBack = await ledger.state(1)
        let stateTwoBack = await ledger.state(2)
        note(
            "reconnect: relayBack=\(relayBack) bothSawDuring=\(bothSawDuring) "
                + "wireSubs=\(wireSubsAfterReconnect) "
                + "sharer1=\(stateOneBack.latestRowCount) rows/\(stateOneBack.batches) batches/"
                + "status \(stateOneBack.relayStatus) history=\(stateOneBack.statusHistory) "
                + "sharer2=\(stateTwoBack.latestRowCount) rows/\(stateTwoBack.batches) batches/"
                + "status \(stateTwoBack.relayStatus) history=\(stateTwoBack.statusHistory)"
        )
        XCTAssertTrue(relayBack, "the relay did not come back up on \(relay.url)")
        XCTAssertTrue(
            bothSawDuring,
            "the subscription did not resume: 60s after the relay came back on the same port with "
                + "the outage-window event \(during.id) durably in its store, sharer1 has "
                + "\(stateOneBack.latest) (status \(stateOneBack.relayStatus), history "
                + "\(stateOneBack.statusHistory), ended \(stateOneBack.ended ?? "no")) and sharer2 "
                + "has \(stateTwoBack.latest) (status \(stateTwoBack.relayStatus), history "
                + "\(stateTwoBack.statusHistory), ended \(stateTwoBack.ended ?? "no"))"
        )
        // No duplicate, no loss: EXACTLY the two events, once each, in both
        // sharers. A duplicate canonical row would raise the count; a lost
        // event would drop one of the ids.
        XCTAssertEqual(
            stateOneBack.latest, [before.id, during.id],
            "sharer1 after the reconnect must hold exactly the two seeded ids"
        )
        XCTAssertEqual(
            stateTwoBack.latest, [before.id, during.id],
            "sharer2 after the reconnect must hold exactly the two seeded ids"
        )
        XCTAssertEqual(
            stateOneBack.latestRowCount, 2,
            "sharer1 delivered \(stateOneBack.latestRowCount) rows against the 2 expected event ids. "
                + "Above the id count this is a duplicate canonical row; below it, a lost event."
        )
        XCTAssertEqual(
            stateTwoBack.latestRowCount, 2,
            "sharer2 delivered \(stateTwoBack.latestRowCount) rows against the 2 expected event ids. "
                + "Above the id count this is a duplicate canonical row; below it, a lost event."
        )
        XCTAssertNil(
            stateOneBack.duplicateWitness,
            "sharer1 was delivered a batch carrying the same id twice: "
                + "\(stateOneBack.duplicateWitness ?? "")"
        )
        XCTAssertNil(
            stateTwoBack.duplicateWitness,
            "sharer2 was delivered a batch carrying the same id twice: "
                + "\(stateTwoBack.duplicateWitness ?? "")"
        )
        XCTAssertEqual(
            stateOneBack.everSeen, [before.id, during.id],
            "sharer1 saw an id across its whole life that is not one of the two seeded events"
        )
        XCTAssertEqual(
            stateTwoBack.everSeen, [before.id, during.id],
            "sharer2 saw an id across its whole life that is not one of the two seeded events"
        )

        // --- Phase 5: closing ONE sharer must not close the other ---------
        //
        // The #1848 shape, on the far side of a reconnect. Close sharer1
        // and publish a third event; sharer2 shares the same wire
        // subscription and must still receive it.

        sharerOne.cancel()
        let sharerOneEnded = await waitUntil(timeout: 10) { await ledger.state(1).ended != nil }

        let after = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "C13 after one sharer closed")
        try await relay.seed(after)

        let survivorSawAfter = await waitUntil(timeout: 60) {
            await ledger.state(2).latest.contains(after.id)
        }
        let stateOneClosed = await ledger.state(1)
        let stateTwoSurvivor = await ledger.state(2)
        note(
            "one sharer closed: sharer1 ended=\(stateOneClosed.ended ?? "still open") "
                + "latest=\(stateOneClosed.latest.count) rows | survivorSawAfter="
                + "\(survivorSawAfter) sharer2=\(stateTwoSurvivor.latestRowCount) rows/status "
                + "\(stateTwoSurvivor.relayStatus) wireSubs=\(diagnostics.wireSubCount())"
        )
        XCTAssertTrue(
            sharerOneEnded,
            "sharer1's sequence never ended after `cancel()` -- its state below is not the state "
                + "of a closed observation"
        )
        XCTAssertTrue(
            survivorSawAfter,
            "closing ONE of two observations sharing a query withdrew the wire subscription the "
                + "other still needed: 60s after publishing \(after.id) to a live relay, the "
                + "surviving observation holds \(stateTwoSurvivor.latest) with status "
                + "\(stateTwoSurvivor.relayStatus) (history \(stateTwoSurvivor.statusHistory), "
                + "ended \(stateTwoSurvivor.ended ?? "no")). This is #1848's shape."
        )
        XCTAssertEqual(
            stateTwoSurvivor.latest, [before.id, during.id, after.id],
            "the surviving observation must hold exactly the three seeded ids"
        )
        XCTAssertEqual(
            stateTwoSurvivor.latestRowCount, 3,
            "the surviving observation delivered \(stateTwoSurvivor.latestRowCount) rows against the "
                + "3 expected event ids. Above the id count this is a duplicate canonical row; "
                + "below it, a lost event."
        )
        XCTAssertFalse(
            stateOneClosed.everSeen.contains(after.id),
            "the CLOSED observation was still being delivered rows (\(after.id)) after `cancel()`"
        )

        // --- Phase 6: closing the LAST sharer must release the wire -------
        //
        // The other half of the same claim. Without this, phase 5 would
        // pass just as well for an engine that never withdraws anything.

        sharerTwo.cancel()
        let released = await waitUntil(timeout: 15) { diagnostics.wireSubCount() == 0 }
        let wireSubsAtEnd = diagnostics.wireSubCount()
        note("both closed: wireSubs=\(wireSubsAtEnd) relays=\(diagnostics.current().relays.count)")
        XCTAssertTrue(
            released && wireSubsAtEnd == 0,
            "closing the LAST observation sharing the query left \(wireSubsAtEnd) wire "
                + "subscription(s) alive after a bounded 15s wait -- the shared demand was never "
                + "withdrawn (#1848's own defect)"
        )

        consumers.cancel()
        try await relay.kill()
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
