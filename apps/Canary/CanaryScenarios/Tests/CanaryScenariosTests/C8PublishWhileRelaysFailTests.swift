// C8 (docs/internals/canary.md "Scenario status"): publish while relays
// fail. Three real strfry destinations, one of them a port that REFUSES a
// TCP connection, and one ordinary `engine.publish(...)` aimed at all
// three. The write must land on the healthy relays, the receipt must
// report what happened at each relay separately, and the relay that could
// not be reached must stay in the write's record rather than quietly
// disappearing from it.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C2/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection. The relays are reached only over real
// `ws://` URLs to separate processes.
//
// WHAT THIS ADDS OVER WHAT IS ALREADY PROVEN. C7 published to ONE healthy
// relay. C9 case 3 has a partial success, but only across a `kill -9`, and
// its unreachable relay is a `SIGSTOP`ed process whose listening socket
// stays open and whose TCP connections stay established -- the app's
// packets are accepted and then simply never answered. That is a
// *hung* relay, not a *down* one, and the two fail differently: a hung
// relay fails by timeout, a down one by `ECONNREFUSED` on the first
// syscall. Nothing in this repository had ever published into the second.
//
// THE PRECONDITION IS A REFUSAL, NOT A SILENCE. "The relay is down" and
// "the relay is slow" produce the same green result in a scenario that
// only waits for the other two relays to succeed, and only one of them is
// this scenario's subject. So the down relay's port is probed with a real
// TCP connect that reports its ERRNO (`RelayHandle.probe`, new in
// `RelayLabKit`), and the scenario requires `ECONNREFUSED` specifically --
// an RST from the kernel, meaning nothing is listening -- rather than a
// probe that merely failed to finish.
//
// That probe exists because the obvious one does not answer this. Writing
// this precondition against `RelayHandle.isReachable` failed on its first
// run with the relay genuinely dead: `isReachable(timeout: 2)` returned
// `false` after 2.0057 seconds, the entire budget. `Network.framework`
// classifies a refused connection as `NWConnection.State.waiting`, not
// `.failed`, and keeps retrying it, so `isReachable` can only ever end on
// its own timeout and reports the identical `false` for a refused port and
// a black-holed one. It is still the right tool for "wait until this stops
// answering", which is all C2 and C13 ask of it. It cannot be the tool
// for C8's claim. Elapsed time is printed on every run either way.
//
// The other half of the same precondition, easy to forget: the two healthy
// relays are required to be genuinely reachable at the same moment. "Some
// relays are down" is not established by a scenario in which all of them
// are.
//
// WHAT THIS SCENARIO DELIBERATELY DOES NOT ASSERT. The write never reaches
// `WriteOutcome.settled`, and that is correct rather than a defect:
// settlement needs every destination terminal, the only terminal a
// permanently-unreachable relay could reach is `.gaveUp`, and offline time
// deliberately consumes no attempt ordinal -- so a relay that is simply
// down never spends the ceiling and never gives up. The scenario records
// `outcome=nil` on every run and asserts the facts that ARE available:
// per-relay states, the destination set, and the durable queue entry.
//
// Every wait is a bounded poll on a real condition with the real stuck
// values reported on timeout, never a fixed sleep used AS the oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C8PublishWhileRelaysFailTests: XCTestCase {
    // MARK: - What the receipt stream actually said

    /// The receipt stream's facts, kept as facts rather than folded into a
    /// verdict. Per-relay state is recorded with its full history, because
    /// "the failed relay was reported honestly" is a claim about what the
    /// stream SAID, and a scenario that only looks at the last value cannot
    /// tell "never mentioned" from "mentioned and then settled".
    private actor ReceiptLedger {
        private(set) var latestByRelay: [String: RelayState] = [:]
        private(set) var historyByRelay: [String: [String]] = [:]
        private(set) var destinations: [String]?
        private(set) var destinationsComplete = false
        private(set) var signedEventID: String?
        private(set) var outcome: WriteOutcome?
        private(set) var ended: String?
        private(set) var factCount = 0

        func note(_ fact: WriteFact) {
            factCount += 1
            switch fact {
            case .signing(let state):
                if case .signed(let eventID) = state { signedEventID = eventID }
            case .relay(_, let relay, let state):
                latestByRelay[relay] = state
                let label = C8PublishWhileRelaysFailTests.label(state)
                if historyByRelay[relay]?.last != label {
                    historyByRelay[relay, default: []].append(label)
                }
            case .destinations(let relays, let complete, _):
                destinations = relays
                destinationsComplete = complete
            case .outcome(let value):
                outcome = value
            }
        }

        func end(_ why: String) { ended = why }

        func hasPublished(_ relays: [String]) -> Bool {
            relays.allSatisfy {
                if case .published = latestByRelay[$0] { return true }
                return false
            }
        }

        /// A flat, printable snapshot -- taken at the exact instant a
        /// condition first held, so the assertions below are made against
        /// the state at that instant rather than whatever drifted in later.
        func snapshot() -> Snapshot {
            Snapshot(
                latestByRelay: latestByRelay,
                historyByRelay: historyByRelay,
                destinations: destinations,
                destinationsComplete: destinationsComplete,
                signedEventID: signedEventID,
                outcome: outcome,
                ended: ended,
                factCount: factCount
            )
        }
    }

    private struct Snapshot: Sendable {
        var latestByRelay: [String: RelayState]
        var historyByRelay: [String: [String]]
        var destinations: [String]?
        var destinationsComplete: Bool
        var signedEventID: String?
        var outcome: WriteOutcome?
        var ended: String?
        var factCount: Int

        func label(_ relay: String) -> String {
            latestByRelay[relay].map(C8PublishWhileRelaysFailTests.label) ?? "(never reported)"
        }
    }

    /// A short label per `RelayState`, spelled out rather than
    /// `String(describing:)` so the "did this relay publish / is it still
    /// trying / did it give up" classification below is a decision this
    /// scenario makes in the open.
    static func label(_ state: RelayState) -> String {
        switch state {
        case .waiting(let waiting):
            switch waiting {
            case .notConnected: return "waiting(notConnected)"
            case .needsAuth: return "waiting(needsAuth)"
            case .eligible: return "waiting(eligible)"
            case .backingOff(let attempt, _, let cause, let detail):
                return "waiting(backingOff attempt=\(attempt) cause=\(cause) detail=\(detail ?? "nil"))"
            case .persistenceStalled(let detail): return "waiting(persistenceStalled \(detail))"
            }
        case .attempting(let attempt, _): return "attempting(attempt=\(attempt))"
        case .sent(let attempt, _): return "sent(attempt=\(attempt))"
        case .published: return "published"
        case .rejected(let reason): return "rejected(\(reason))"
        case .authFailed(_, let source, let reason): return "authFailed(\(source) \(reason))"
        case .gaveUp: return "gaveUp"
        }
    }

    /// Bounded poll on a real condition. Returns whether it ever held; the
    /// caller reports the real stuck values on `false`. The sleep paces the
    /// poll -- it is never the thing being waited on.
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

    func testPublishLandsOnHealthyRelaysWhileADeadOneStaysInTheRecord() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c8-\(UUID().uuidString)")
        for sub in ["healthy-a", "healthy-b", "down", "store"] {
            try FileManager.default.createDirectory(
                at: root.appendingPathComponent(sub), withIntermediateDirectories: true
            )
        }
        defer { try? FileManager.default.removeItem(at: root) }

        var log: [String] = []
        defer { print((["", "C8 phase log:"] + log).joined(separator: "\n")) }

        let healthyA = try await RelayHandle(
            name: "c8-healthy-a", workDir: root.appendingPathComponent("healthy-a"),
            binaryPath: binaryPath
        )
        let healthyB = try await RelayHandle(
            name: "c8-healthy-b", workDir: root.appendingPathComponent("healthy-b"),
            binaryPath: binaryPath
        )
        // The down relay is STARTED and then killed rather than merely
        // never started: starting it is what proves the port was a real
        // relay's port and that the refusal below is this relay being gone,
        // not an address that never worked. (`RelayHandle`'s ephemeral port
        // is allocated at construction, so an unstarted handle would also
        // refuse -- and would prove nothing about a relay failing.)
        let down = try await RelayHandle(
            name: "c8-down", workDir: root.appendingPathComponent("down"), binaryPath: binaryPath
        )
        try await healthyA.start()
        try await healthyB.start()
        try await down.start()
        let downWhileUp = await down.probe(timeout: 2)
        try await down.kill()

        // --- Precondition: one relay REFUSES, the other two answer --------

        let downProbe = await down.probe(timeout: 2)
        let aProbe = await healthyA.probe(timeout: 2)
        let bProbe = await healthyB.probe(timeout: 2)
        log.append(
            "reachability: down while its process ran=\(downWhileUp.outcome) in "
                + String(format: "%.4f", downWhileUp.elapsed) + "s | down after SIGKILL="
                + "\(downProbe.outcome) in " + String(format: "%.4f", downProbe.elapsed)
                + "s of a 2s budget | healthyA=\(aProbe.outcome) healthyB=\(bProbe.outcome)"
        )
        XCTAssertEqual(
            downWhileUp.outcome, .accepted,
            "PRECONDITION: \(down.url) did not accept a connection even while its relay process "
                + "was running (\(downWhileUp.outcome)), so the refusal below is not a relay that "
                + "failed -- it is a port that never worked"
        )
        XCTAssertEqual(
            downProbe.outcome, .refused,
            "PRECONDITION: a real TCP connect to \(down.url) after its relay process was "
                + "SIGKILLed came back \(downProbe.outcome) in "
                + String(format: "%.4f", downProbe.elapsed) + "s, not `refused`. C8's subject is "
                + "a relay that is DOWN -- the kernel sending an RST because nothing is "
                + "listening. A connect that merely times out is a slow, filtered or hung relay, "
                + "which fails a different way and would make this scenario about timeouts."
        )
        XCTAssertEqual(
            aProbe.outcome, .accepted,
            "PRECONDITION: healthyA is \(aProbe.outcome) -- 'SOME relays fail' is not established "
                + "by a run in which they all do"
        )
        XCTAssertEqual(
            bProbe.outcome, .accepted,
            "PRECONDITION: healthyB is \(bProbe.outcome) -- 'SOME relays fail' is not established "
                + "by a run in which they all do"
        )

        // --- The ordinary app: one engine, one account, one publish -------
        //
        // The same three relays serve reads and writes, which is the honest
        // app shape: an app does not maintain a separate "these ones work"
        // list, and NMP is not told which of the three is dead.

        let relays = [healthyA.url, healthyB.url, down.url]
        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: root.appendingPathComponent("store/nmp.redb").path,
                appRelays: relays
            )
        )
        defer { engine.shutdown() }

        let account = try engine.session.add(privateKey: .generate(), makeCurrent: true)
        let authorHex = account.publicKey.bytes.map { String(format: "%02x", $0) }.joined()

        // Opened before the publish, as in C7: a fresh single-use key
        // authoring exactly one event means any row this query delivers is
        // unambiguously the published one.
        let query = try engine.observe(NMPFilter(kinds: [1], authors: .literal([authorHex])))
        final class RowState: @unchecked Sendable {
            private let lock = NSLock()
            private var rows: [Row] = []
            private var maxCount = 0
            func record(_ batch: RowBatch) {
                lock.lock()
                defer { lock.unlock() }
                if batch.rows.count >= maxCount {
                    rows = batch.rows
                    maxCount = batch.rows.count
                }
            }
            func current() -> [Row] {
                lock.lock()
                defer { lock.unlock() }
                return rows
            }
        }
        let rowState = RowState()
        let rowPump = Task {
            do {
                for try await batch in query { rowState.record(batch) }
            } catch {}
        }
        defer {
            rowPump.cancel()
            query.cancel()
        }

        let content = "C8 publish while relays fail"
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .event(kind: 1, content: content),
                routing: .explicit(relays: relays)
            )
        )

        let ledger = ReceiptLedger()
        let receiptPump = Task {
            do {
                for try await fact in receipt.status { await ledger.note(fact) }
                await ledger.end("the stream ended")
            } catch {
                await ledger.end("the stream threw: \(error)")
            }
        }
        defer { receiptPump.cancel() }

        // --- The write is not blocked by the relay that cannot be reached --
        //
        // Snapshotted at the exact instant both healthy relays report
        // `.published`, so what is asserted about the down relay is its
        // state AT THAT MOMENT, not whatever it drifted to afterwards.

        let bothHealthyPublished = await waitUntil(timeout: 60) {
            await ledger.hasPublished([healthyA.url, healthyB.url])
        }
        let atSuccess = await ledger.snapshot()
        log.append(
            "delivery: bothHealthyPublished=\(bothHealthyPublished) facts=\(atSuccess.factCount) "
                + "healthyA=\(atSuccess.label(healthyA.url)) healthyB=\(atSuccess.label(healthyB.url)) "
                + "down=\(atSuccess.label(down.url)) "
                + "destinations=\(atSuccess.destinations ?? []) complete=\(atSuccess.destinationsComplete) "
                + "outcome=\(String(describing: atSuccess.outcome)) ended=\(atSuccess.ended ?? "no")"
        )
        XCTAssertTrue(
            bothHealthyPublished,
            "60s after publishing to two live relays and one refused port, the receipt reports "
                + "healthyA=\(atSuccess.label(healthyA.url)) and healthyB=\(atSuccess.label(healthyB.url)) "
                + "(down=\(atSuccess.label(down.url)), \(atSuccess.factCount) facts, stream "
                + "\(atSuccess.ended ?? "still open")). A relay that cannot be reached blocked "
                + "delivery to the ones that can."
        )
        // The sharpest form of "did not block": at the instant BOTH healthy
        // relays had published, the down one had NOT. If it had somehow
        // published too there would be no failing relay in this run at all.
        XCTAssertFalse(
            {
                if case .published = atSuccess.latestByRelay[down.url] { return true }
                return false
            }(),
            "the refused port \(down.url) reported `.published` -- something answered on a port "
                + "that refuses TCP connections, so this run has no failed relay in it"
        )

        // --- The receipt reports honestly, PER RELAY ----------------------
        //
        // Three separate claims, and the third is the one that rots first.

        // 1. The failed relay was named as a destination.
        XCTAssertEqual(
            atSuccess.destinations.map(Set.init), Set(relays),
            "the write's `destinations` fact named \(atSuccess.destinations ?? []) rather than all "
                + "three requested relays -- an app cannot see what it is 'sending 0 of n' to"
        )
        XCTAssertTrue(
            atSuccess.destinationsComplete,
            "`.explicit` routing is verbatim and cannot widen, so its destination set must be "
                + "complete; it reported complete=false"
        )

        // 2. The failed relay got its OWN fact, with a state that says what
        //    is wrong with it -- not silence, and not somebody else's state.
        let downHistory = atSuccess.historyByRelay[down.url] ?? []
        XCTAssertNotNil(
            atSuccess.latestByRelay[down.url],
            "the receipt stream never reported a single fact about \(down.url) across "
                + "\(atSuccess.factCount) facts. A destination an app was told about and then "
                + "hears nothing about is exactly the silent drop this scenario exists to catch."
        )
        XCTAssertTrue(
            downHistory.allSatisfy { $0 != "published" },
            "\(down.url) is a refused port and its reported history is \(downHistory)"
        )

        // 3. The healthy relays' facts are their own, and correct.
        XCTAssertEqual(atSuccess.label(healthyA.url), "published")
        XCTAssertEqual(atSuccess.label(healthyB.url), "published")

        // --- The durable record keeps the failed relay too -----------------
        //
        // The receipt stream is a live view; `publishQueue` is what an app
        // reads back later, and it is the one that matters for "did the
        // failed relay survive in the record".
        //
        // READ WHILE THE WRITE IS STILL OPEN, and that is not incidental:
        // measured during this scenario's own falsification, a write whose
        // relays all succeed settles and `publishQueue(forEventID:)` then
        // returns ZERO entries for it. The queue is the OUTSTANDING
        // obligations, exactly as its doc says; it is not a history of
        // writes. So an app that wants to know where a finished write went
        // must have been holding its receipt -- the durable per-relay
        // record does not outlive settlement in this door. That is
        // recorded in docs/internals/canary.md as an observation, not
        // asserted here, because C8's write never settles anyway (its down
        // relay never gives up).

        let queue = try engine.publishQueue(forEventID: atSuccess.signedEventID ?? "", limit: 16)
        let entry = queue.first
        let queuedRelays = entry.map { Set($0.relays) }
        let queuedStates = entry.map { entry in
            Dictionary(entry.relayStates.map { ($0.relay, Self.label($0.state)) }) { a, _ in a }
        } ?? [:]
        log.append(
            "queue: entries=\(queue.count) eventID=\(atSuccess.signedEventID ?? "nil") "
                + "pubkey=\(entry?.pubkey ?? "nil") relays=\(entry?.relays ?? []) "
                + "states=\(queuedStates) routeComplete=\(String(describing: entry?.routeComplete)) "
                + "outcome=\(String(describing: entry?.outcome))"
        )
        XCTAssertNotNil(
            atSuccess.signedEventID,
            "the receipt never reported a signed event id, so there is nothing to look the "
                + "durable record up by"
        )
        XCTAssertEqual(
            queuedRelays, Set(relays),
            "the durable publish-queue entry lists \(entry?.relays ?? []) rather than all three "
                + "requested relays -- the relay that could not be reached was dropped from the "
                + "app's own record of where this write is going"
        )
        XCTAssertNotNil(
            queuedStates[down.url],
            "the publish-queue entry carries no state at all for \(down.url) (states "
                + "\(queuedStates)), so an app reading its queue back cannot tell that this "
                + "destination is stuck"
        )
        XCTAssertEqual(queuedStates[healthyA.url], "published")
        XCTAssertEqual(queuedStates[healthyB.url], "published")

        // --- The relays themselves hold it. Relay-side truth. -------------
        //
        // Independent of anything NMP claims: ask each live relay, over its
        // own wire, whether it has this event id. This is the difference
        // between "NMP says it published" and "the write landed".

        let eventID = atSuccess.signedEventID ?? ""
        let onA = try await healthyA.queryById(eventID)
        let onB = try await healthyB.queryById(eventID)
        log.append(
            "relay-side: healthyA has it=\(onA != nil) healthyB has it=\(onB != nil) "
                + "pubkey on A=\(onA?["pubkey"] as? String ?? "nil") "
                + "content on A=\(onA?["content"] as? String ?? "nil")"
        )
        XCTAssertNotNil(
            onA,
            "\(healthyA.url) reported `.published` on the receipt but does not serve \(eventID) "
                + "back over its own wire"
        )
        XCTAssertNotNil(
            onB,
            "\(healthyB.url) reported `.published` on the receipt but does not serve \(eventID) "
                + "back over its own wire"
        )
        XCTAssertEqual(onA?["pubkey"] as? String, authorHex)
        XCTAssertEqual(onA?["content"] as? String, content)

        // --- One canonical row, with honest provenance --------------------
        //
        // Provenance growth is a live delta on the query that was already
        // open, and it arrives after the relay's OK, so it is waited for
        // rather than read at whatever moment the assertions above
        // finished. The first draft read it immediately and captured
        // `sources=[]` -- a true value, half a second too early.

        let provenanceGrew = await waitUntil(timeout: 30) {
            let sources = Set(rowState.current().first?.sources ?? [])
            return sources.contains(healthyA.url) && sources.contains(healthyB.url)
        }
        let rows = rowState.current()
        let sources = rows.first?.sources ?? []
        log.append(
            "rows: count=\(rows.count) id=\(rows.first?.id ?? "nil") sources=\(sources) "
                + "provenanceGrew=\(provenanceGrew)"
        )
        XCTAssertEqual(
            rows.count, 1,
            "expected exactly one canonical row for this single-use author, got \(rows.count). "
                + "Two relays echoing one event back must grow ONE row's provenance, never insert "
                + "a second row."
        )
        XCTAssertEqual(rows.first?.id, eventID, "the row and the receipt disagree on the event id")
        XCTAssertTrue(
            provenanceGrew,
            "30s after both relays reported `.published`, the row's provenance is \(sources) -- "
                + "both healthy relays echoed this event back, so both belong in its sources"
        )
        XCTAssertFalse(
            sources.contains(down.url),
            "the row names \(down.url) as a source, but that port refuses connections and cannot "
                + "have delivered anything"
        )

        // --- The failure is still a failure at the end --------------------
        //
        // Asserted after every other assertion, not only before: a port
        // that came back mid-scenario would have made this an ordinary
        // three-healthy-relay publish and nothing else here would have
        // noticed.

        let downAtEnd = await down.probe(timeout: 2)
        log.append("final: \(down.url) is \(downAtEnd.outcome)")
        XCTAssertEqual(
            downAtEnd.outcome, .refused,
            "PRECONDITION: \(down.url) is \(downAtEnd.outcome) at the end of the scenario. If it "
                + "became reachable at any point, nothing above is evidence about publishing to a "
                + "relay that is down."
        )

        receiptPump.cancel()
        rowPump.cancel()
        query.cancel()
        receipt.status.cancel()
        try await healthyA.kill()
        try await healthyB.kill()
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
