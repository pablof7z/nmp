// C2 (docs/internals/canary.md "Scenario status"): cache, then offline
// restart. Use the app against a real relay, quit it, take the relay away
// entirely, start again -- and the feed is still there, out of the durable
// store, with the same account still signed in.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, and in particular no reading of the Redb file to decide whether
// the app is correct -- the store's contents are asserted only through
// what `engine.observe(...)` delivers, which is all an app has.
//
// This is the headline local-first claim -- a README/VISION promise with,
// until now, no end-to-end evidence anywhere. `nmp-engine`'s
// `integration_capstone::watermark_cold_start_offline` is the nearest
// thing that exists and is an owner test: no store file surviving a real
// process teardown, no relay process to take away, no public API. Offline
// there means the absence of a fake transport; here it means a port that
// refuses a real TCP connection.
//
// WHY THE ONLINE HALF IS A SEPARATE PROCESS. "Restart" is the whole claim,
// and building a second `NMPEngine` over the same store path inside the
// process that just filled it is not one: the Redb pages, the allocator
// and every row the first engine decoded are still in that address space,
// so a read served from anywhere other than the durable file would look
// identical. `canary-c2-warmer` does the online half and quits; this test
// waits for it to be genuinely gone -- exited, waited on,
// `terminationStatus == 0` -- before opening the store. It is a clean app
// quit, not a crash; the crash case is C9's and is not restated here.
//
// THE OFFLINE PRECONDITION IS ASSERTED, NOT ASSUMED. "The relay was slow"
// and "the relay does not exist" produce the same green result in a test
// that only waits for rows, and only one of them proves anything. So the
// relay process is SIGKILLed and the port is required to REFUSE a real TCP
// connection (`RelayHandle.isReachable`) both before the restarted engine
// is built and again after every assertion has been made -- a relay that
// came back mid-scenario would otherwise be invisible.
//
// Every wait is a bounded race against a deadline with the real captured
// values reported on failure, never a fixed sleep used AS the oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C2CacheThenOfflineRestartTests: XCTestCase {
    func testFeedAndIdentitySurviveARestartWithNoRelayReachable() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c2-\(UUID().uuidString)")
        let relayDir = root.appendingPathComponent("relay")
        try FileManager.default.createDirectory(at: relayDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("store"), withIntermediateDirectories: true
        )
        defer { try? FileManager.default.removeItem(at: root) }

        let storePath = root.appendingPathComponent("store/nmp.redb").path
        let sessionPayloadPath = root.appendingPathComponent("session.bin").path

        let relay = try await RelayHandle(name: "c2-relay", workDir: relayDir, binaryPath: binaryPath)
        try await relay.start()

        // Two events the app will have read online, seeded over real EVENT
        // frames. Two rather than one so a restart that returns "some rows"
        // is distinguishable from one that returns the right rows.
        let keyPair = try NostrKeyPair()
        let older = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C2 older cached note",
            createdAt: Int64(Date().timeIntervalSince1970) - 120
        )
        let newer = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C2 newer cached note"
        )
        try await relay.seed(older)
        try await relay.seed(newer)

        var log: [String] = []
        defer { print((["", "C2 phase log:"] + log).joined(separator: "\n")) }

        // --- Phase 1: use the app, online, in a process that then quits ---

        let warmer = Process()
        warmer.executableURL = try Self.warmerBinaryURL()
        warmer.arguments = [storePath, sessionPayloadPath, relay.url, keyPair.pubkeyHex, "2"]
        let pipe = Pipe()
        warmer.standardOutput = pipe
        try warmer.run()

        let warmerOutput: String = await withTaskGroup(of: String?.self) { group in
            group.addTask {
                await withCheckedContinuation { continuation in
                    DispatchQueue.global().async {
                        let data = pipe.fileHandleForReading.readDataToEndOfFile()
                        continuation.resume(returning: String(data: data, encoding: .utf8) ?? "")
                    }
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 60_000_000_000)
                await ChildProcess.killAndWaitForExit(warmer)
                return nil
            }
            let result = await group.next() ?? nil
            group.cancelAll()
            return result ?? ""
        }
        warmer.waitUntilExit()

        func marker(_ prefix: String) -> String? {
            warmerOutput.split(separator: "\n")
                .first { $0.hasPrefix(prefix) }
                .map { String($0.dropFirst(prefix.count)) }
        }
        let warmedAccount = marker("ACCOUNT:")
        let cachedIDs = marker("CACHED:").map { Set($0.split(separator: " ").map(String.init)) }
        let sessionBytesWritten = marker("SESSION:")
        log.append(
            "online: exit=\(warmer.terminationStatus) account=\(warmedAccount ?? "none") "
                + "cached=\(cachedIDs?.count ?? -1) sessionBytes=\(sessionBytesWritten ?? "none") "
                + "running=\(warmer.isRunning)"
        )
        XCTAssertEqual(
            warmer.terminationStatus, 0,
            "the online half did not complete a clean session: \(warmerOutput)"
        )
        XCTAssertFalse(
            warmer.isRunning,
            "PRECONDITION: the process that wrote the store is still alive, so the read below "
                + "would not be a restart"
        )
        guard let warmedAccount, let cachedIDs, sessionBytesWritten != nil else {
            return XCTFail("the online half never reported a full session: \(warmerOutput)")
        }
        // PRECONDITION: the app really did read this feed off the network.
        // The warmer only prints CACHED once every row names the relay in
        // its `sources`, so this is the relay's own delivery, not rows that
        // appeared from nowhere.
        XCTAssertEqual(
            cachedIDs, [older.id, newer.id],
            "the online session cached \(cachedIDs) rather than the two seeded events"
        )

        // --- Phase 2: go fully offline ------------------------------------

        try await relay.kill()
        let deadline = Date().addingTimeInterval(10)
        var reachable = true
        while Date() < deadline {
            reachable = await relay.isReachable()
            if !reachable { break }
            try? await Task.sleep(nanoseconds: 25_000_000)
        }
        log.append("offline: \(relay.url) reachable=\(reachable)")
        XCTAssertFalse(
            reachable,
            "PRECONDITION: \(relay.url) still accepted a real TCP connection after the relay "
                + "process was SIGKILLed. Everything below would be an ordinary online read."
        )

        // --- Phase 3: restart, offline ------------------------------------
        //
        // Same store path, same relay URL (now a dead port -- the app has no
        // idea the relay is gone, which is the honest shape), and the
        // session payload restored exactly as it was persisted.

        let sessionBytes = try Data(contentsOf: URL(fileURLWithPath: sessionPayloadPath))
        let engine = try NMPEngine(
            config: NMPConfig(storePath: storePath, appRelays: [relay.url]),
            sessionPayload: NMPSessionPayload(bytes: sessionBytes)
        )
        defer { engine.shutdown() }

        // The identity half. Session identity not surviving restart is what
        // blocked this scenario for as long as it was blocked
        // (docs/internals/canary.md), so it is asserted, not assumed -- and
        // asserted past the public key: an account whose signer did not come
        // back is a logged-in user who cannot do anything.
        let restored = try engine.session.current
        let restoredHex = restored?.publicKey.bytes.map { String(format: "%02x", $0) }.joined()
        let accountCount = try engine.session.accounts.count
        log.append(
            "identity: accounts=\(accountCount) current=\(restoredHex ?? "none") "
                + "provider=\(String(describing: restored?.providerKind)) "
                + "signing=\(String(describing: restored?.signingAvailability))"
        )
        XCTAssertEqual(
            restoredHex, warmedAccount,
            "the restarted app is signed in as \(restoredHex ?? "nobody"), not as the account the "
                + "online session used (\(warmedAccount))"
        )
        XCTAssertEqual(accountCount, 1, "expected exactly the one account the online session added")
        XCTAssertEqual(
            restored?.providerKind, .localKey,
            "the restored account lost its signer provider, so it is a public key, not a login"
        )
        XCTAssertEqual(
            restored?.signingAvailability, .available,
            "the restored account cannot sign -- \(String(describing: restored?.signingAvailability)). "
                + "A local key needs no network, so being offline is not an excuse for this."
        )

        // The feed half. One bounded observation over the same filter, with
        // no relay that could possibly answer.
        // The read is finished only when BOTH facts are in: the cached rows,
        // and the query's own account of the source it cannot reach. Ending
        // at the first batch carrying enough rows is not good enough -- the
        // very first batch is served straight from the store, before any
        // evidence about the relay exists, so an oracle that stops there
        // reads `nil` evidence every time and could never tell "NMP said
        // nothing" from "NMP has not said it yet". Whatever the wait ends
        // on, the accumulated real values are what gets asserted.
        final class ReadState: @unchecked Sendable {
            private let lock = NSLock()
            private var value = (
                batches: 0, ids: Set<String>(), rowCount: 0, contents: Set<String>(),
                status: nil as String?, reconciled: nil as UInt64?, shortfall: [] as [String],
                ended: nil as String?
            )

            func record(_ batch: RowBatch, relayURL: String) -> Bool {
                lock.lock()
                defer { lock.unlock() }
                value.batches += 1
                if batch.rows.count >= value.rowCount {
                    value.ids = Set(batch.rows.map(\.id))
                    value.rowCount = batch.rows.count
                    value.contents = Set(batch.rows.map(\.content))
                }
                if let evidence = batch.evidence.first {
                    if let source = evidence.sources.first(where: { $0.relay == relayURL }) {
                        value.status = C2CacheThenOfflineRestartTests.label(source.status)
                        value.reconciled = source.reconciledThrough
                    }
                    value.shortfall = evidence.shortfall.map { String(describing: $0) }
                }
                return value.rowCount >= 2 && value.status != nil
            }

            func end(_ why: String) {
                lock.lock()
                value.ended = why
                lock.unlock()
            }

            func snapshot() -> (
                batches: Int, ids: Set<String>, rowCount: Int, contents: Set<String>,
                status: String?, reconciled: UInt64?, shortfall: [String], ended: String?
            ) {
                lock.lock()
                defer { lock.unlock() }
                return value
            }
        }
        let readState = ReadState()

        let query = try engine.observe(.single(NMPDemand(selection: NMPFilter(kinds: [1], authors: .literal([keyPair.pubkeyHex])))))
        await withTaskGroup(of: Void.self) { group in
            group.addTask {
                do {
                    for try await batch in query where readState.record(batch, relayURL: relay.url) {
                        return
                    }
                    readState.end("the sequence ended")
                } catch {
                    readState.end("the sequence threw: \(error)")
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 20_000_000_000)
            }
            await group.next()
            group.cancelAll()
        }
        query.cancel()
        let read = readState.snapshot()

        // Asserted AFTER the read, not only before it: a relay that came
        // back while the observation was open would have made the whole
        // thing an online read, and nothing else here would have noticed.
        let reachableAfter = await relay.isReachable()
        log.append(
            "feed: batches=\(read.batches) rows=\(read.rowCount) ids=\(read.ids.count) "
                + "contents=\(read.contents) status=\(read.status ?? "nil") "
                + "reconciledThrough=\(String(describing: read.reconciled)) "
                + "shortfall=\(read.shortfall) ended=\(read.ended ?? "no") "
                + "| relay reachable after the read=\(reachableAfter)"
        )

        XCTAssertEqual(
            read.ids, [older.id, newer.id],
            "20s offline, \(read.batches) batch(es): the restart returned \(read.ids) rather than "
                + "the two cached events (sequence \(read.ended ?? "still open")). The durable "
                + "store did not serve what the online session had already read."
        )
        XCTAssertEqual(
            read.rowCount, 2,
            "the offline restart delivered \(read.rowCount) rows against 2 cached ids -- above "
                + "that is a duplicate canonical row, below it a lost event"
        )
        // Ids alone would pass for rows that survived as bare keys. The
        // content is what the user actually sees.
        XCTAssertEqual(
            read.contents, ["C2 older cached note", "C2 newer cached note"],
            "the cached rows came back without their content: \(read.contents)"
        )
        // Honest reporting, not manufactured completeness: NMP must say this
        // planned source is not working rather than let the app read a stale
        // feed as a complete one (bug-class ledger #7).
        XCTAssertNotNil(
            read.status,
            "20s offline and across \(read.batches) batch(es) the query never reported any "
                + "acquisition evidence for the configured relay \(relay.url) (shortfall "
                + "\(read.shortfall)) -- an app cannot tell a complete feed from a stale one"
        )
        if let status = read.status {
            XCTAssertFalse(
                ["requesting", "finishedStoredEvents", "coverageSatisfied"].contains(status),
                "the query reported the dead relay as '\(status)', which claims a source that "
                    + "cannot answer a single byte"
            )
        }
        // The other half of the same honesty, and the one that makes an
        // offline cache USEFUL rather than merely present. `SourceEvidence`
        // holds two deliberately orthogonal facts, and its own doc names
        // exactly this case: "a relay can be currently `.disconnected` while
        // still carrying a perfectly good `reconciledThrough` from before it
        // dropped". If the restart kept the rows but forgot how far it had
        // proven coverage, an app has a feed it cannot reason about -- it
        // would have to treat the whole cached range as unproven again. This
        // is the public-API reading of #1087's claim.
        XCTAssertNotNil(
            read.reconciled,
            "the restart kept the cached rows but lost the durable `reconciledThrough` watermark "
                + "for \(relay.url) (status '\(read.status ?? "nil")'), so the app cannot tell "
                + "which part of its cached feed was ever proven"
        )

        XCTAssertFalse(
            reachableAfter,
            "PRECONDITION: \(relay.url) became reachable during the offline read, so the rows "
                + "above are not proof that anything was served from the store"
        )
    }

    /// A short label per `SourceStatus`, spelled out so the "is this source
    /// claiming to work" decision below is made in the open rather than
    /// inherited from a synthesized description.
    private static func label(_ status: SourceStatus) -> String {
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

    /// Same derivation C9/C17 use: SwiftPM puts every executable product and
    /// this test bundle in one build products directory.
    private static func warmerBinaryURL() throws -> URL {
        let productsDir = Bundle(for: C2CacheThenOfflineRestartTests.self)
            .bundleURL.deletingLastPathComponent()
        let candidate = productsDir.appendingPathComponent("canary-c2-warmer")
        guard FileManager.default.isExecutableFile(atPath: candidate.path) else {
            throw XCTSkip("canary-c2-warmer not found next to the test bundle at \(candidate.path)")
        }
        return candidate
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
