// C10 (docs/internals/canary.md "Scenario status"): offline write, then
// convergence. Compose and publish a note with nothing reachable at all,
// then let the network come back and require the write to go out and
// settle by itself -- no app-side retry, no re-publish, no reattach, no
// second engine, and no second copy of the event anywhere.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C2/C7/C8/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection.
//
// C10 IS TRIVIALLY FAKEABLE, AND THE FAKE LOOKS EXACTLY LIKE THE REAL
// THING. A write that completes before the network is taken away, followed
// by a relay coming back, produces a green run with every convergence
// assertion satisfied and proves nothing whatsoever. C13's fourth
// falsifier is the same lesson from the read side: publishing the
// "outage-window" event before the outage left every behavioural assertion
// green and only the precondition caught it. So this scenario spends most
// of its length establishing two facts BEFORE it asserts any behaviour:
//
//   1. NOTHING WAS REACHABLE WHEN THE WRITE WAS MADE. The relay process is
//      SIGKILLed and its port is required to answer a real TCP connect
//      with `ECONNREFUSED` -- an RST from the kernel, nothing listening --
//      not merely to fail to answer. `RelayHandle.probe` (new in
//      RelayLabKit) reports the errno; `isReachable` cannot, because
//      Network.framework classifies a refused connection as `.waiting` and
//      retries it, so it returns the same `false` after the full timeout
//      for a refused port and for a black hole. Probed before the engine
//      is constructed, after the write is accepted, and again at the end.
//
//   2. THE WRITE HAD NOT ALREADY GONE OUT. This is the one that would
//      quietly rot. "The relay has it after the reconnect" is satisfied by
//      a write that reached the relay BEFORE the outage, so absence has to
//      be proven at the relay itself while it is down. A SECOND strfry
//      process is started over the SAME LMDB directory on its OWN
//      ephemeral port (`RelayHandle(dataDir:)`, C13's mechanism used to
//      prove absence rather than presence), asked for the event id and for
//      everything this author ever wrote, and both come back empty. The
//      app cannot have been talking to it: it is dialing a port that
//      refuses connections and has never been told the sidecar's port
//      exists.
//
// NO APP-SIDE RETRY, MECHANICALLY. Between the accepted write and the
// converged one this scenario calls `publish` exactly once, never calls
// `reattachReceipt`, never constructs a second engine, and never reopens
// the query. The facts that prove convergence arrive on the SAME `Receipt`
// the single `publish` returned. If NMP required an app to drive
// redelivery, there is nothing here that would do it.
//
// WHAT "NO DUPLICATE PUBLICATION" CAN AND CANNOT PROVE, stated honestly
// rather than implied. The relay is asked for every event this author
// wrote and must hold exactly one, which rules out the failure that
// actually loses data: a write re-signed on the retry path becomes a
// DIFFERENT event id (an event id is the hash of its contents, and a fresh
// `created_at` changes it), so it would appear as a second row here. It
// does NOT rule out NMP sending the identical EVENT frame twice, because
// the relay deduplicates by id and the receipt would look the same. That
// is the same limit C9 recorded for its no-resend assertion, and closing
// it needs relay-side inbound frame counts or a public delivery-attempt
// fact. Neither exists.
//
// AN OBSERVATION THIS SCENARIO RECORDS BUT DOES NOT ASSERT. The relay's
// per-relay state history is printed on every run and reads, every time:
//
//     ["waiting(notConnected)", "waiting(needsAuth)", "sent(attempt=1)",
//      "published"]
//
// `RelayWaiting.needsAuth` appears against a strfry that has no NIP-42
// configuration at all and never sends an `AUTH` frame. Nothing is broken
// -- the write lane needs its own identity-scoped session established
// before it can send, and this is that -- but the word an app would show a
// person out of that case is "this relay wants you to authenticate", which
// is not what happened. Recorded as a finding in
// `docs/internals/canary.md` rather than asserted, so neither the presence
// nor the absence of this state is silently promoted to a contract.
//
// Every wait is a bounded poll on a real condition with the real stuck
// values reported on timeout, never a fixed sleep used AS the oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C10OfflineWriteThenConvergenceTests: XCTestCase {
    // MARK: - What the receipt stream said, kept as facts

    private actor ReceiptLedger {
        private(set) var latestByRelay: [String: RelayState] = [:]
        private(set) var historyByRelay: [String: [String]] = [:]
        private(set) var signedEventID: String?
        private(set) var outcome: WriteOutcome?
        private(set) var ended: String?
        private(set) var factCount = 0
        /// Every distinct event id the signing stage ever announced. More
        /// than one means the write was signed twice -- two different
        /// events, which is the duplicate-publication failure that matters.
        private(set) var signedEventIDs: Set<String> = []

        func note(_ fact: WriteFact) {
            factCount += 1
            switch fact {
            case .signing(let state):
                if case .signed(let eventID) = state {
                    signedEventID = eventID
                    signedEventIDs.insert(eventID)
                }
            case .relay(_, let relay, let state):
                latestByRelay[relay] = state
                let label = C10OfflineWriteThenConvergenceTests.label(state)
                if historyByRelay[relay]?.last != label {
                    historyByRelay[relay, default: []].append(label)
                }
            case .destinations:
                break
            case .outcome(let value):
                outcome = value
            }
        }

        func end(_ why: String) { ended = why }

        /// One call rather than two, because two `await`s on an actor in a
        /// boolean `&&` cannot be written in a synchronous closure.
        func hasPublishedAndSettled(_ relay: String) -> Bool {
            guard case .published = latestByRelay[relay] else { return false }
            guard case .settled = outcome else { return false }
            return true
        }

        func snapshot() -> Snapshot {
            Snapshot(
                latestByRelay: latestByRelay, historyByRelay: historyByRelay,
                signedEventID: signedEventID, signedEventIDs: signedEventIDs,
                outcome: outcome, ended: ended, factCount: factCount
            )
        }
    }

    private struct Snapshot: Sendable {
        var latestByRelay: [String: RelayState]
        var historyByRelay: [String: [String]]
        var signedEventID: String?
        var signedEventIDs: Set<String>
        var outcome: WriteOutcome?
        var ended: String?
        var factCount: Int

        func label(_ relay: String) -> String {
            latestByRelay[relay].map(C10OfflineWriteThenConvergenceTests.label) ?? "(never reported)"
        }
    }

    /// A short label per `RelayState`, spelled out rather than
    /// `String(describing:)` so the classifications below are decisions
    /// this scenario makes in the open.
    static func label(_ state: RelayState) -> String {
        switch state {
        case .waiting(let waiting):
            switch waiting {
            case .notConnected: return "waiting(notConnected)"
            case .needsAuth: return "waiting(needsAuth)"
            case .eligible: return "waiting(eligible)"
            case .backingOff(let attempt, _, let cause, _):
                return "waiting(backingOff attempt=\(attempt) cause=\(cause))"
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

    /// The labels that would mean the write had already reached, or been
    /// handed to, a live connection. Any of them appearing while the port
    /// refuses connections destroys the "it had not gone out" precondition.
    ///
    /// `.attempting` counts, and that is a deliberate reading of its own
    /// doc: it means an attempt is RUNNING with its ordinal spent, which the
    /// engine only does once it holds a live session for the lane. Its
    /// content is "nothing about the wire is proved", which is exactly why
    /// it belongs here rather than being waved through -- a precondition
    /// that only rules out proven sends would accept a write that was
    /// mid-handoff to a relay this scenario claims is unreachable.
    private static func meansItWentOut(_ label: String) -> Bool {
        label == "published" || label.hasPrefix("sent(") || label.hasPrefix("attempting(")
    }

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

    func testWriteMadeWithNothingReachableConvergesByItselfWhenTheRelayReturns() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c10-\(UUID().uuidString)")
        for sub in ["relay", "sidecar", "store"] {
            try FileManager.default.createDirectory(
                at: root.appendingPathComponent(sub), withIntermediateDirectories: true
            )
        }
        defer { try? FileManager.default.removeItem(at: root) }

        var log: [String] = []
        defer { print((["", "C10 phase log:"] + log).joined(separator: "\n")) }

        let relay = try await RelayHandle(
            name: "c10-relay", workDir: root.appendingPathComponent("relay"), binaryPath: binaryPath
        )
        // Started first so the port is provably a working relay's port, and
        // its LMDB directory provably exists, before it is taken away.
        try await relay.start()
        let whileUp = await relay.probe(timeout: 2)
        try await relay.kill()

        // --- Precondition 1: nothing is reachable ------------------------

        let beforeWrite = await relay.probe(timeout: 2)
        log.append(
            "offline: \(relay.url) while running=\(whileUp.outcome) | before the write="
                + "\(beforeWrite.outcome) in " + String(format: "%.4f", beforeWrite.elapsed) + "s"
        )
        XCTAssertEqual(
            whileUp.outcome, .accepted,
            "PRECONDITION: \(relay.url) never worked even with its relay process running "
                + "(\(whileUp.outcome)) -- the offline state below is not a relay going away"
        )
        XCTAssertEqual(
            beforeWrite.outcome, .refused,
            "PRECONDITION: a real TCP connect to \(relay.url) came back \(beforeWrite.outcome), "
                + "not `refused`. C10's whole claim is a write made with NOTHING reachable; a "
                + "connect that merely times out leaves that unestablished."
        )

        // --- The app writes, offline --------------------------------------
        //
        // A persistent store, and the relay URL the app has always had. The
        // app is not told the relay is gone -- that is the honest shape,
        // and it is also what makes this a convergence test rather than a
        // configuration one.

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: root.appendingPathComponent("store/nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { engine.shutdown() }

        let account = try engine.session.add(privateKey: .generate(), makeCurrent: true)
        let authorHex = account.publicKey.bytes.map { String(format: "%02x", $0) }.joined()

        let query = try engine.observe(.single(NMPDemand(selection: NMPFilter(kinds: [1], authors: .literal([authorHex])))))
        final class RowState: @unchecked Sendable {
            private let lock = NSLock()
            private var rows: [Row] = []
            private var maxCount = 0
            private var everSeenIDs: Set<String> = []
            func record(_ batch: RowBatch) {
                lock.lock()
                defer { lock.unlock() }
                everSeenIDs.formUnion(batch.rows.map(\.id))
                if batch.rows.count >= maxCount {
                    rows = batch.rows
                    maxCount = batch.rows.count
                }
            }
            func current() -> (rows: [Row], everSeen: Set<String>) {
                lock.lock()
                defer { lock.unlock() }
                return (rows, everSeenIDs)
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

        let content = "C10 written with nothing reachable"
        // The ONE publish call in this scenario. Nothing below re-issues
        // it, reattaches to it, or otherwise nudges NMP.
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .event(kind: 1, content: content),
                routing: .explicit(relays: [relay.url])
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

        // Local acceptance is the local-first claim and needs no relay: the
        // row must be readable through the app's own live query while
        // nothing is reachable at all.
        let localRowArrived = await waitUntil(timeout: 20) {
            !rowState.current().rows.isEmpty
        }
        let signedOffline = await waitUntil(timeout: 20) {
            await ledger.snapshot().signedEventID != nil
        }
        let offlineSnapshot = await ledger.snapshot()
        let offlineRows = rowState.current().rows
        let duringWrite = await relay.probe(timeout: 2)
        log.append(
            "accepted offline: row=\(localRowArrived) rows=\(offlineRows.count) "
                + "id=\(offlineRows.first?.id ?? "nil") sources=\(offlineRows.first?.sources ?? []) "
                + "signed=\(signedOffline) eventID=\(offlineSnapshot.signedEventID ?? "nil") "
                + "relayState=\(offlineSnapshot.label(relay.url)) "
                + "history=\(offlineSnapshot.historyByRelay[relay.url] ?? []) "
                + "port during the write=\(duringWrite.outcome)"
        )
        XCTAssertTrue(
            localRowArrived,
            "20s after an accepted write with no relay reachable, the app's own live query has "
                + "delivered no row. A locally accepted write is supposed to be visible "
                + "immediately, from cache, with zero relays."
        )
        XCTAssertTrue(
            signedOffline,
            "the write was never signed with nothing reachable. Signing is local and needs no "
                + "network; a local key that will not sign offline makes the whole scenario moot."
        )
        XCTAssertEqual(offlineRows.count, 1, "expected exactly one locally accepted row")
        XCTAssertEqual(offlineRows.first?.id, offlineSnapshot.signedEventID)
        XCTAssertEqual(offlineRows.first?.content, content)
        XCTAssertEqual(
            offlineRows.first?.sources ?? [], [],
            "the locally accepted row claims provenance \(offlineRows.first?.sources ?? []) while "
                + "the only configured relay refuses TCP connections -- nothing can have "
                + "delivered it"
        )
        XCTAssertEqual(
            duringWrite.outcome, .refused,
            "PRECONDITION: \(relay.url) is \(duringWrite.outcome) immediately after the write was "
                + "accepted, so the write was not necessarily made offline"
        )

        // --- Precondition 2: it had NOT gone out --------------------------
        //
        // Two independent readings, because either one alone is weak. The
        // receipt is NMP's own account of itself; the sidecar is the
        // relay's durable store, read by something the app has never heard
        // of, over its own wire.

        guard let eventID = offlineSnapshot.signedEventID else {
            return XCTFail("no signed event id, so there is nothing to look for at the relay")
        }
        let offlineHistory = offlineSnapshot.historyByRelay[relay.url] ?? []
        XCTAssertFalse(
            offlineHistory.contains(where: Self.meansItWentOut),
            "PRECONDITION: the receipt reports \(offlineHistory) for \(relay.url) while that port "
                + "refuses connections -- the write's bytes had already crossed the transport "
                + "handoff, so there is nothing offline about what follows"
        )

        let sidecar = try await RelayHandle(
            name: "c10-sidecar", workDir: root.appendingPathComponent("sidecar"),
            binaryPath: binaryPath, dataDir: relay.dataDir
        )
        try await sidecar.start()
        let sidecarHasEvent = try await sidecar.queryById(eventID)
        let sidecarAuthorIDs = try await sidecar.queryIDsByAuthor(authorHex, kinds: [1])
        try await sidecar.kill()

        let afterSidecar = await relay.probe(timeout: 2)
        log.append(
            "relay store while down: has the event=\(sidecarHasEvent != nil) "
                + "author events=\(sidecarAuthorIDs.count) \(sidecarAuthorIDs) | app port still "
                + "\(afterSidecar.outcome)"
        )
        XCTAssertNil(
            sidecarHasEvent,
            "PRECONDITION: the relay's own durable store ALREADY holds \(eventID) while its port "
                + "is refusing connections. The write went out before the outage, so every "
                + "convergence assertion below would pass without NMP converging anything -- this "
                + "is C13's fourth falsifier in write form."
        )
        XCTAssertEqual(
            sidecarAuthorIDs, [],
            "PRECONDITION: the relay's durable store already holds \(sidecarAuthorIDs.count) "
                + "event(s) by this author while it is down"
        )
        XCTAssertEqual(
            afterSidecar.outcome, .refused,
            "PRECONDITION: \(relay.url) is \(afterSidecar.outcome) after the sidecar closed -- the "
                + "app's port must be dead for the whole offline window, not just at its start"
        )

        // --- The network comes back. The app does nothing. ----------------

        let reconnectedAt = Date()
        try await relay.restart()
        let backUp = await relay.probe(timeout: 2)

        // 120s: measured from the runtime's own reconnect schedule, which
        // doubles from 3s with up to 5s of per-URL jitter re-paid on every
        // retry (6, 12, 24, ... capped at 300). The offline window above is
        // a few seconds, so the pending redial is early in that ramp -- but
        // the bound is generous rather than tight, because a scenario that
        // fails on a slow machine teaches nothing.
        let converged = await waitUntil(timeout: 120) {
            await ledger.hasPublishedAndSettled(relay.url)
        }
        let convergenceSeconds = Date().timeIntervalSince(reconnectedAt)
        let final = await ledger.snapshot()
        log.append(
            "convergence: relay back=\(backUp.outcome) converged=\(converged) in "
                + String(format: "%.1f", convergenceSeconds) + "s | relay="
                + "\(final.label(relay.url)) history=\(final.historyByRelay[relay.url] ?? []) "
                + "outcome=\(String(describing: final.outcome)) facts=\(final.factCount) "
                + "ended=\(final.ended ?? "no")"
        )
        XCTAssertEqual(backUp.outcome, .accepted, "the relay did not come back on \(relay.url)")
        XCTAssertTrue(
            converged,
            "120s after the relay returned on the same port, the write has NOT gone out by "
                + "itself: \(relay.url) is \(final.label(relay.url)) (history "
                + "\(final.historyByRelay[relay.url] ?? [])), outcome "
                + "\(String(describing: final.outcome)), stream \(final.ended ?? "still open"). "
                + "The app published once and did nothing else, which is the whole claim -- if "
                + "this needs an app-side retry, offline-first writing is an app's problem, not "
                + "NMP's."
        )
        XCTAssertEqual(final.outcome, .settled, "expected WriteOutcome.settled")

        // --- It really landed, and exactly once ---------------------------

        let onRelay = try await relay.queryById(eventID)
        let authorIDs = try await relay.queryIDsByAuthor(authorHex, kinds: [1])
        log.append(
            "relay-side: has the event=\(onRelay != nil) pubkey=\(onRelay?["pubkey"] as? String ?? "nil") "
                + "content=\(onRelay?["content"] as? String ?? "nil") author events=\(authorIDs.count) "
                + "\(authorIDs) | signed event ids=\(final.signedEventIDs)"
        )
        XCTAssertNotNil(
            onRelay,
            "the receipt says \(relay.url) published \(eventID), but the relay does not serve it "
                + "back over its own wire"
        )
        XCTAssertEqual(onRelay?["pubkey"] as? String, authorHex)
        XCTAssertEqual(onRelay?["content"] as? String, content)
        XCTAssertEqual(
            authorIDs, [eventID],
            "the relay holds \(authorIDs.count) events by this author (\(authorIDs)) where the "
                + "app published once. A second id here is a write that was re-signed on the "
                + "retry path -- a different event, with different bytes, published as if it were "
                + "the same one."
        )
        XCTAssertEqual(
            final.signedEventIDs, [eventID],
            "the write was signed into \(final.signedEventIDs.count) distinct event ids "
                + "(\(final.signedEventIDs)); one accepted intent is one event"
        )

        // --- One canonical row, provenance grown, nothing duplicated ------

        let provenanceGrew = await waitUntil(timeout: 30) {
            (rowState.current().rows.first?.sources ?? []).contains(relay.url)
        }
        let (rows, everSeen) = rowState.current()
        log.append(
            "rows: count=\(rows.count) sources=\(rows.first?.sources ?? []) "
                + "provenanceGrew=\(provenanceGrew) everSeen=\(everSeen)"
        )
        XCTAssertEqual(
            rows.count, 1,
            "expected one canonical row after convergence, got \(rows.count) -- the relay's echo "
                + "of the app's own write must fold into the row it already had"
        )
        XCTAssertEqual(
            everSeen, [eventID],
            "the query saw \(everSeen) across its whole life against the one event this scenario "
                + "published"
        )
        XCTAssertTrue(
            provenanceGrew,
            "30s after the relay published it, the row's provenance is "
                + "\(rows.first?.sources ?? []) -- the delivery is not reflected in what the app "
                + "can see about where this event has been"
        )

        // The queue is the app's own durable record. After settlement the
        // obligation is finished; recorded rather than asserted into a
        // contract, since "what a settled entry looks like when you read it
        // back" is not what C10 claims.
        let queue = try engine.publishQueue(forEventID: eventID, limit: 16)
        log.append(
            "queue after settlement: entries=\(queue.count) "
                + "outcomes=\(queue.map { String(describing: $0.outcome) })"
        )

        receiptPump.cancel()
        rowPump.cancel()
        query.cancel()
        receipt.status.cancel()
        try await relay.kill()
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
