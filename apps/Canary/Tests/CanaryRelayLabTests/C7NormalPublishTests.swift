// C7 (docs/internals/canary.md "Scenario status"): normal publish, against
// a real strfry child process. The write path's first real exercise --
// see docs/known-gaps.md / docs/internals/canary.md: "the app has never
// called publish" was the starting position before the Compose screen.
// This scenario drives `WriteIntent`/`Receipt` directly, the same public
// door the Compose screen uses, without going through any SwiftUI code.
//
// Same platform note as C1ColdStartLiveFeedTests: macOS, because
// RelayLabKit spawns the relay via `Foundation.Process`, unavailable on
// iOS. Plain `import NMP`, no `@testable`, no internal crate, no direct
// Redb inspection -- the relay is reached only over a real `ws://` URL to
// a separate strfry process.
//
// Every wait is a bounded race against a `Task.sleep` deadline, never a
// fixed sleep used AS the oracle (same shape as C1). The live query
// opened before `publish` is iterated in ONE continuous loop for its
// whole life -- breaking it early to re-open a second one would turn
// "the write is visible before any relay confirms it" into "reopen the
// query and eventually find it," which proves nothing about acceptance
// timing.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C7NormalPublishTests: XCTestCase {
    // MARK: - Coordinator

    /// The one piece of state genuinely shared between the two independent
    /// consuming tasks below: whether the published event was ever visible
    /// in the live query before the receipt stream reported this exact
    /// relay confirming it (`RelayState.published`). Everything else each
    /// task tracks and reports for itself.
    private actor Coordinator {
        private var sawPublishedRelayFact = false
        private var rowEverSeen = false
        private(set) var rowVisibleBeforeConfirmed: Bool?

        /// Called once per row-query batch that contains our row. Returns
        /// whether the relay's echo has already been folded into it
        /// (`sources` contains the relay URL) -- the row task's own signal
        /// to stop.
        func noteRowVisible(_ row: Row, relayURL: String) -> Bool {
            if !rowEverSeen {
                rowEverSeen = true
                rowVisibleBeforeConfirmed = !sawPublishedRelayFact
            }
            return row.sources.contains(relayURL)
        }

        func noteFact(_ fact: WriteFact) {
            if case .relay(_, _, let state) = fact, case .published = state {
                sawPublishedRelayFact = true
            }
        }
    }

    // MARK: - Outcomes

    private enum RowFinding: Sendable {
        case echoFolded(id: String, sources: [String], signature: RowSignature)
        case duplicate(count: Int)
        case endedEarly
        case threw(String)
    }

    private enum ReceiptFinding: Sendable {
        case settled(signedEventID: String?)
        case otherOutcome(WriteOutcome, signedEventID: String?)
        case endedEarly
        case threw(String)
    }

    private enum StepResult: Sendable {
        case row(RowFinding)
        case receipt(ReceiptFinding)
        case timedOut
    }

    // MARK: - The scenario

    func testNormalPublishLocalAcceptanceThenRelayEchoNoDuplicateRow() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c7-\(UUID().uuidString)")
        let workDir = root.appendingPathComponent("relay")
        try FileManager.default.createDirectory(at: workDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c7-relay", workDir: workDir, binaryPath: binaryPath)
        try await relay.start()

        // "Construct the engine normally": empty store, pointed at the lab
        // relay for both reads (`appRelays`, additive to every read) and,
        // explicitly below, this write's destination.
        let engine = try NMPEngine(config: NMPConfig(appRelays: [relay.url]))
        defer { engine.shutdown() }

        // A real local-key account -- this is a REAL signed publish, not a
        // pre-signed/verbatim one.
        let account = try engine.session.add(privateKey: NMPPrivateKey.generate(), makeCurrent: true)
        let authorHex = Self.hex(account.publicKey.bytes)

        // Open the read query BEFORE publishing. A fresh, single-use
        // keypair authoring exactly one event in this whole scenario means
        // ANY row this query ever delivers is unambiguously the published
        // event -- no need to know its id in advance to recognize it.
        let filter = NMPFilter(kinds: [1], authors: .literal([authorHex]))
        let query = try engine.observe(filter)

        let coordinator = Coordinator()

        // "Create a real signed publish through WriteIntent against a real
        // strfry" -- `.explicit(relays:)` because `.auto` outbox routing is
        // a separate, unconfigured capability here (the same finding
        // recorded for the Compose screen; see docs/internals/canary.md).
        let intent = WriteIntent(
            payload: .event(kind: 1, content: "C7 normal publish"),
            routing: .explicit(relays: [relay.url])
        )
        // "publish() returning is acceptance" -- reaching the next line
        // without a thrown error IS local acceptance; there is no separate
        // "was it accepted" question to ask afterward.
        let receipt = try await engine.publish(intent)

        var rowFinding: RowFinding?
        var receiptFinding: ReceiptFinding?

        await withTaskGroup(of: StepResult.self) { group in
            group.addTask {
                do {
                    for try await batch in query {
                        // The filter is scoped to this one fresh author --
                        // more than one row here is a duplicate canonical
                        // row, full stop, regardless of ids.
                        if batch.rows.count > 1 {
                            return .row(.duplicate(count: batch.rows.count))
                        }
                        guard let row = batch.rows.first else { continue }
                        let echoFolded = await coordinator.noteRowVisible(row, relayURL: relay.url)
                        if echoFolded {
                            return .row(.echoFolded(id: row.id, sources: row.sources, signature: row.signature))
                        }
                    }
                    return .row(.endedEarly)
                } catch {
                    return .row(.threw("\(error)"))
                }
            }
            group.addTask {
                do {
                    var signedEventID: String?
                    for try await fact in receipt.status {
                        await coordinator.noteFact(fact)
                        if case .signing(.signed(let eventID)) = fact {
                            signedEventID = eventID
                        }
                        if case .outcome(let outcome) = fact {
                            if case .settled = outcome {
                                return .receipt(.settled(signedEventID: signedEventID))
                            }
                            return .receipt(.otherOutcome(outcome, signedEventID: signedEventID))
                        }
                    }
                    return .receipt(.endedEarly)
                } catch {
                    return .receipt(.threw("\(error)"))
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 25_000_000_000)
                return .timedOut
            }

            var remaining = 2
            while remaining > 0 {
                guard let result = await group.next() else { break }
                switch result {
                case .row(let finding):
                    rowFinding = finding
                    remaining -= 1
                case .receipt(let finding):
                    receiptFinding = finding
                    remaining -= 1
                case .timedOut:
                    remaining = 0
                }
            }
            group.cancelAll()
        }

        query.cancel()
        try await relay.kill()

        let rowVisibleBeforeConfirmed = await coordinator.rowVisibleBeforeConfirmed

        // -- Assertions --

        guard let rowFinding else {
            return XCTFail("timed out (25s) before the live query observed the relay's echo folded into the row")
        }
        guard let receiptFinding else {
            return XCTFail("timed out (25s) before the receipt stream reached a terminal outcome")
        }

        guard case .echoFolded(let rowID, let sources, let signature) = rowFinding else {
            switch rowFinding {
            case .duplicate(let count):
                return XCTFail("duplicate canonical row: \(count) rows for one author+kind, expected at most 1")
            case .endedEarly:
                return XCTFail("the row query's AsyncSequence ended before the relay's echo was folded in")
            case .threw(let message):
                return XCTFail("the row query threw: \(message)")
            case .echoFolded:
                fatalError("unreachable")
            }
        }

        guard case .settled(let signedEventID) = receiptFinding else {
            switch receiptFinding {
            case .otherOutcome(let outcome, _):
                return XCTFail("expected WriteOutcome.settled, got \(outcome)")
            case .endedEarly:
                return XCTFail("the receipt stream ended before reaching a terminal outcome")
            case .threw(let message):
                return XCTFail("the receipt stream threw: \(message)")
            case .settled:
                fatalError("unreachable")
            }
        }

        // The part that matters most: the row was visible through the live
        // query BEFORE this relay's confirmation reached the receipt
        // stream -- optimistic local acceptance, not something the app had
        // to wait on a network round trip to see.
        XCTAssertEqual(
            rowVisibleBeforeConfirmed, true,
            "expected the locally accepted row to be visible in the live query before the relay's OK, not after"
        )

        // The sharpest assertion: the SAME row's provenance grew to
        // include the relay -- the echo was folded into the one canonical
        // row, never inserted as a second one.
        XCTAssertTrue(
            sources.contains(relay.url),
            "expected the row's sources to include the relay after its echo, got \(sources)"
        )

        // Cross-check the two independent observation paths agree on
        // WHICH event this was -- the row query's id and the receipt
        // stream's signed event id must be the exact same value.
        XCTAssertEqual(signedEventID, rowID, "the row query and the receipt stream must agree on the event id")

        if case .signed = signature {
            // Expected by the time the scenario concludes; not re-asserted
            // as a separate failure mode since `.pending` here would also
            // be a legitimate transient early state -- this is checked at
            // the END of the scenario, once things are quiescent.
        } else {
            XCTFail("expected the row's signature to be RowSignature.signed by the end of the scenario")
        }
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

    private static func hex(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }
}
