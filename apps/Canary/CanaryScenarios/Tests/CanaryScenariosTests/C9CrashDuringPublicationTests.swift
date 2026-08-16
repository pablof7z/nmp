// C9 (docs/internals/canary.md "Scenario status"): crash and restart
// during publication -- whether durable write obligations survive real
// process death, against a real strfry child process.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7: RelayLabKit
// spawns processes via `Foundation.Process`, unavailable on iOS.
//
// THE HARD REQUIREMENT: this terminates a real, separate OS process, not
// an in-process `Engine` value. Dropping an `Engine` and constructing a
// new one in the SAME process proves almost nothing about crash safety --
// ordinary Swift cleanup still runs. `canary-c9-publisher` (a sibling
// executable target in this package) is the process that actually gets
// `kill -9`ed. This file never injects an internal crash point; the
// only failure mode exercised is real process death, matched against
// recovery driven entirely through the public `NMP` API -- no
// `@testable`, no internal crate, no direct Redb inspection.
//
// Every wait is a bounded race against a `Task.sleep` deadline, never a
// fixed sleep used AS the oracle (same shape as C1/C7).

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C9CrashDuringPublicationTests: XCTestCase {
    // MARK: - Child process line reader

    /// Reads the publisher child's stdout as lines, live, so the parent
    /// can wait for a specific marker (`ACCOUNT:...`, `ACCEPTED:...`,
    /// `PARTIAL:...`) with a bounded timeout instead of guessing how long
    /// acceptance or delivery takes.
    private final class ProcessLineReader: @unchecked Sendable {
        let pipe = Pipe()
        private let channel = AsyncStream<String>.makeStream(bufferingPolicy: .unbounded)
        private var buffer = Data()
        private let lock = NSLock()

        func attach(to process: Process) {
            process.standardOutput = pipe
            pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
                guard let self else { return }
                let data = handle.availableData
                self.lock.lock()
                defer { self.lock.unlock() }
                guard !data.isEmpty else {
                    self.channel.continuation.finish()
                    return
                }
                self.buffer.append(data)
                while let newline = self.buffer.firstIndex(of: 0x0A) {
                    let lineData = self.buffer[..<newline]
                    self.buffer.removeSubrange(...newline)
                    if let line = String(data: lineData, encoding: .utf8) {
                        self.channel.continuation.yield(line)
                    }
                }
            }
        }

        /// Bounded wait for a line starting with `prefix`. Returns the
        /// text after the prefix, or `nil` on timeout.
        func waitForLine(prefix: String, timeout: TimeInterval) async -> String? {
            await withTaskGroup(of: String?.self) { group in
                group.addTask { [channel] in
                    var iterator = channel.stream.makeAsyncIterator()
                    while let line = await iterator.next() {
                        if line.hasPrefix(prefix) {
                            return String(line.dropFirst(prefix.count))
                        }
                        if line.hasPrefix("FAILED:") {
                            return nil
                        }
                    }
                    return nil
                }
                group.addTask {
                    try? await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
                    return nil
                }
                let result = await group.next() ?? nil
                group.cancelAll()
                return result
            }
        }
    }

    // MARK: - Spawning the publisher

    private func spawnPublisher(
        storePath: String,
        sessionPayloadPath: String,
        correlation: String,
        mode: String,
        relays: [String]
    ) throws -> (process: Process, reader: ProcessLineReader) {
        let process = Process()
        process.executableURL = try Self.publisherBinaryURL()
        process.arguments = [storePath, sessionPayloadPath, correlation, mode] + relays
        let reader = ProcessLineReader()
        reader.attach(to: process)
        try process.run()
        return (process, reader)
    }

    /// SwiftPM places every executable product and this test bundle's
    /// `.xctest` in the same build products directory (e.g.
    /// `.build/arm64-apple-macosx/debug/`) -- derive the sibling
    /// `canary-c9-publisher` binary from this TEST BUNDLE's own location
    /// rather than hardcoding that path shape, which would break under a
    /// different configuration or architecture. `CommandLine.arguments[0]`
    /// is NOT useable here -- under `swift test` that resolves to the
    /// `xctest` runner itself (e.g. inside the Xcode toolchain), not
    /// anything in this package's own build products.
    private static func publisherBinaryURL() throws -> URL {
        let productsDir = Bundle(for: C9CrashDuringPublicationTests.self).bundleURL.deletingLastPathComponent()
        let candidate = productsDir.appendingPathComponent("canary-c9-publisher")
        guard FileManager.default.isExecutableFile(atPath: candidate.path) else {
            throw XCTSkip("canary-c9-publisher not found next to the test bundle at \(candidate.path)")
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

    // MARK: - Shared recovery assertion

    private enum RecoveryOutcome: Sendable {
        case settled(finalRowCount: Int, finalIDs: Set<String>, sources: [String], relayRegressed: Bool)
        case notReattached
        case rowNeverVisible
        case receiptNeverSettled(String)
        case threw(String)
    }

    /// Opens a FRESH engine over the SAME store path + restored session,
    /// reattaches the receipt by correlation, and races the row query
    /// against the reattached receipt stream until both a settled outcome
    /// and full row visibility are observed -- exactly the C7 shape, plus
    /// tracking whether `watchForResend` (when given) ever regresses from
    /// `.published` back to `.waiting`/`.sent`, which would mean that
    /// relay was asked to redo work it had already finished.
    private func assertRecovery(
        storePath: String,
        sessionPayloadPath: String,
        correlation: String,
        accountHex: String,
        appRelays: [String],
        expectedSources: [String],
        watchForResend: String? = nil,
        timeout: TimeInterval = 25
    ) async throws -> RecoveryOutcome {
        let sessionBytes = try Data(contentsOf: URL(fileURLWithPath: sessionPayloadPath))
        let engine = try NMPEngine(
            config: NMPConfig(storePath: storePath, appRelays: appRelays),
            sessionPayload: NMPSessionPayload(bytes: sessionBytes)
        )
        defer { engine.shutdown() }

        let reattachment = try engine.reattachReceipt(correlation: correlation)
        guard case .attached(let receipt) = reattachment else {
            return .notReattached
        }

        let filter = NMPFilter(kinds: [1], authors: .literal([accountHex]))
        let query = try engine.observe(filter)

        actor Tracker {
            private var lastRelayState: [String: RelayState] = [:]
            private(set) var regressed = false

            func note(_ fact: WriteFact, watchFor relay: String?) {
                guard let relay, case .relay(_, let factRelay, let state) = fact, factRelay == relay else {
                    return
                }
                if case .published = lastRelayState[relay], case .waiting = state {
                    regressed = true
                }
                if case .published = lastRelayState[relay], case .sent = state {
                    regressed = true
                }
                lastRelayState[relay] = state
            }
        }
        let tracker = Tracker()

        enum Step: Sendable {
            case row(count: Int, ids: Set<String>, sources: [String])
            case receiptSettled
            /// The receipt stream stopped WITHOUT a settled outcome, either by
            /// ending or by throwing. Distinct from `.receiptSettled` because
            /// collapsing them makes "delivery never resumed" report as
            /// "delivery resumed" -- see the comment on the receipt task.
            case receiptUnsettled(String)
            case timedOut
        }

        var rowResult: (count: Int, ids: Set<String>, sources: [String])?
        var receiptSettled = false
        var receiptUnsettledReason: String?
        let wantedSources = Set(expectedSources)

        // ONE continuous iteration of the row query for its whole life,
        // same discipline as C1/C7: it only returns once its OWN proof is
        // captured (every expected relay present in this row's
        // `sources`), never on the row's first, possibly-still-bare
        // sighting. A fresh `.observe()` after settlement is NOT a
        // substitute for this -- provenance growth is a live delta on
        // this same subscription, not something a brand-new query is
        // guaranteed to already see relayed back from the store.
        await withTaskGroup(of: Step.self) { group in
            group.addTask {
                do {
                    for try await batch in query {
                        if batch.rows.count > 1 {
                            return .row(count: batch.rows.count, ids: Set(batch.rows.map(\.id)), sources: [])
                        }
                        guard let row = batch.rows.first else { continue }
                        if wantedSources.isSubset(of: Set(row.sources)) {
                            return .row(count: batch.rows.count, ids: Set(batch.rows.map(\.id)), sources: row.sources)
                        }
                    }
                } catch {}
                return .row(count: 0, ids: [], sources: [])
            }
            // A stream that ends without settling, or throws, is NOT a settled
            // delivery. Both used to fall through to `.receiptSettled`, so the
            // two failures this scenario exists to detect -- delivery never
            // resuming after the crash, and the receipt stream breaking --
            // reported as success. `.receiptNeverSettled` was reachable only
            // by the 25s timeout. C7 already distinguishes these; this is the
            // newer scenario weakening the oracle it was copied from.
            group.addTask {
                do {
                    for try await fact in receipt.status {
                        await tracker.note(fact, watchFor: watchForResend)
                        if case .outcome(let outcome) = fact, case .settled = outcome {
                            return .receiptSettled
                        }
                    }
                    return .receiptUnsettled("the receipt stream ended without a settled outcome")
                } catch {
                    return .receiptUnsettled("the receipt stream threw: \(error)")
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: UInt64(timeout * 1_000_000_000))
                return .timedOut
            }

            var remaining = 2
            while remaining > 0 {
                guard let step = await group.next() else { break }
                switch step {
                case .row(let count, let ids, let sources):
                    rowResult = (count, ids, sources)
                    remaining -= 1
                case .receiptSettled:
                    receiptSettled = true
                    remaining -= 1
                case .receiptUnsettled(let reason):
                    receiptUnsettledReason = reason
                    remaining -= 1
                case .timedOut:
                    remaining = 0
                }
            }
            group.cancelAll()
        }

        query.cancel()

        guard let rowResult, !rowResult.ids.isEmpty else { return .rowNeverVisible }
        guard receiptSettled else {
            return .receiptNeverSettled(
                receiptUnsettledReason ?? "no settled outcome arrived before the timeout"
            )
        }

        let regressed = await tracker.regressed
        return .settled(
            finalRowCount: rowResult.count,
            finalIDs: rowResult.ids,
            sources: rowResult.sources,
            relayRegressed: regressed
        )
    }

    // MARK: - Case 1: kill after local acceptance, relay reachable

    func testKillAfterLocalAcceptanceBeforeDeliveryCompletes() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c9-case1-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(
            name: "c9-case1-relay", workDir: root.appendingPathComponent("relay"), binaryPath: binaryPath
        )
        try await relay.start()

        let storePath = root.appendingPathComponent("store").path
        let sessionPayloadPath = root.appendingPathComponent("session.bin").path
        let correlation = UUID().uuidString

        let (process, reader) = try spawnPublisher(
            storePath: storePath, sessionPayloadPath: sessionPayloadPath,
            correlation: correlation, mode: "plain", relays: [relay.url]
        )

        guard let accountHex = await reader.waitForLine(prefix: "ACCOUNT:", timeout: 10) else {
            await ChildProcess.killAndWaitForExit(process)
            return XCTFail("publisher never printed ACCOUNT: within 10s")
        }
        guard await reader.waitForLine(prefix: "ACCEPTED:", timeout: 10) != nil else {
            await ChildProcess.killAndWaitForExit(process)
            return XCTFail("publisher never printed ACCEPTED: within 10s")
        }

        // Kill as fast as possible after seeing acceptance -- no
        // artificial delay. The relay is healthy and reachable; this is a
        // real race against real network timing, exactly the "nothing
        // gets a chance to run cleanup" case the contract asks for.
        await ChildProcess.killAndWaitForExit(process)
        XCTAssertFalse(process.isRunning, "publisher did not actually die")

        let outcome = try await assertRecovery(
            storePath: storePath, sessionPayloadPath: sessionPayloadPath,
            correlation: correlation, accountHex: accountHex, appRelays: [relay.url],
            expectedSources: [relay.url]
        )
        try await relay.kill()

        switch outcome {
        case .settled(let finalRowCount, let finalIDs, let sources, _):
            XCTAssertEqual(finalRowCount, 1, "expected exactly one canonical row, got \(finalRowCount)")
            XCTAssertEqual(finalIDs.count, 1)
            XCTAssertTrue(sources.contains(relay.url), "expected delivery to resume to \(relay.url)")
        case .notReattached:
            XCTFail("reattachReceipt(correlation:) did not find the obligation after restart")
        case .rowNeverVisible:
            XCTFail("locally accepted canonical state did not survive the crash")
        case .receiptNeverSettled(let reason):
            XCTFail(
                "delivery did not resume/settle after restart with no further app action: \(reason)"
            )
        case .threw(let message):
            XCTFail("recovery threw: \(message)")
        }
    }

    // MARK: - Case 2: kill while the relay is unreachable (partitioned)

    func testKillWhileRelayPartitioned() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c9-case2-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(
            name: "c9-case2-relay", workDir: root.appendingPathComponent("relay"), binaryPath: binaryPath
        )
        try await relay.start()
        // Partitioned BEFORE the publisher ever starts: nothing was ever
        // sent, deterministically, by construction -- not a timing race.
        relay.partition()

        let storePath = root.appendingPathComponent("store").path
        let sessionPayloadPath = root.appendingPathComponent("session.bin").path
        let correlation = UUID().uuidString

        let (process, reader) = try spawnPublisher(
            storePath: storePath, sessionPayloadPath: sessionPayloadPath,
            correlation: correlation, mode: "plain", relays: [relay.url]
        )

        guard let accountHex = await reader.waitForLine(prefix: "ACCOUNT:", timeout: 10) else {
            relay.heal()
            await ChildProcess.killAndWaitForExit(process)
            return XCTFail("publisher never printed ACCOUNT: within 10s")
        }
        // Acceptance is LOCAL and does not require the relay to answer --
        // this must still print even though the relay is frozen.
        guard await reader.waitForLine(prefix: "ACCEPTED:", timeout: 10) != nil else {
            relay.heal()
            await ChildProcess.killAndWaitForExit(process)
            return XCTFail("local acceptance never happened even though it needs no relay")
        }

        await ChildProcess.killAndWaitForExit(process)
        XCTAssertFalse(process.isRunning, "publisher did not actually die")

        // Heal AFTER the crash so recovery can actually complete --
        // C9 is about crash recovery, not indefinite offline convergence.
        relay.heal()

        let outcome = try await assertRecovery(
            storePath: storePath, sessionPayloadPath: sessionPayloadPath,
            correlation: correlation, accountHex: accountHex, appRelays: [relay.url],
            expectedSources: [relay.url]
        )
        try await relay.kill()

        switch outcome {
        case .settled(let finalRowCount, let finalIDs, let sources, _):
            XCTAssertEqual(finalRowCount, 1, "expected exactly one canonical row, got \(finalRowCount)")
            XCTAssertEqual(finalIDs.count, 1)
            XCTAssertTrue(
                sources.contains(relay.url),
                "expected delivery to resume to \(relay.url) once healed, got \(sources)"
            )
        case .notReattached:
            XCTFail("reattachReceipt(correlation:) did not find the obligation after restart")
        case .rowNeverVisible:
            XCTFail("locally accepted canonical state did not survive the crash")
        case .receiptNeverSettled:
            XCTFail("delivery did not resume/settle after restart+heal with no further app action")
        case .threw(let message):
            XCTFail("recovery threw: \(message)")
        }
    }

    // MARK: - Case 3: kill after one relay succeeded, another did not

    func testKillAfterPartialRelaySuccessNoResendToTheSucceededRelay() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c9-case3-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relayA = try await RelayHandle(
            name: "c9-case3-relay-a", workDir: root.appendingPathComponent("relay-a"), binaryPath: binaryPath
        )
        try await relayA.start()
        let relayB = try await RelayHandle(
            name: "c9-case3-relay-b", workDir: root.appendingPathComponent("relay-b"), binaryPath: binaryPath
        )
        try await relayB.start()
        // B is unreachable from the very first attempt -- deterministic,
        // not a timing race against which relay answers first.
        relayB.partition()

        let storePath = root.appendingPathComponent("store").path
        let sessionPayloadPath = root.appendingPathComponent("session.bin").path
        let correlation = UUID().uuidString

        let (process, reader) = try spawnPublisher(
            storePath: storePath, sessionPayloadPath: sessionPayloadPath,
            correlation: correlation, mode: "await-partial", relays: [relayA.url, relayB.url]
        )

        guard let accountHex = await reader.waitForLine(prefix: "ACCOUNT:", timeout: 10) else {
            relayB.heal()
            await ChildProcess.killAndWaitForExit(process)
            return XCTFail("publisher never printed ACCOUNT: within 10s")
        }
        guard await reader.waitForLine(prefix: "PARTIAL:", timeout: 20) != nil else {
            relayB.heal()
            await ChildProcess.killAndWaitForExit(process)
            return XCTFail("publisher never observed one relay published while the other stayed pending")
        }

        await ChildProcess.killAndWaitForExit(process)
        XCTAssertFalse(process.isRunning, "publisher did not actually die")

        relayB.heal()

        let outcome = try await assertRecovery(
            storePath: storePath, sessionPayloadPath: sessionPayloadPath,
            correlation: correlation, accountHex: accountHex,
            appRelays: [relayA.url, relayB.url],
            expectedSources: [relayA.url, relayB.url], watchForResend: relayA.url
        )
        try await relayA.kill()
        try await relayB.kill()

        switch outcome {
        case .settled(let finalRowCount, let finalIDs, let sources, let relayRegressed):
            XCTAssertEqual(finalRowCount, 1, "expected exactly one canonical row, got \(finalRowCount)")
            XCTAssertEqual(finalIDs.count, 1)
            XCTAssertTrue(sources.contains(relayA.url), "relay A's earlier success must still be recorded")
            XCTAssertTrue(sources.contains(relayB.url), "relay B must complete delivery once healed")
            XCTAssertFalse(
                relayRegressed,
                "relay A (already published before the crash) was asked to redo work as if new"
            )
        case .notReattached:
            XCTFail("reattachReceipt(correlation:) did not find the obligation after restart")
        case .rowNeverVisible:
            XCTFail("locally accepted canonical state did not survive the crash")
        case .receiptNeverSettled:
            XCTFail("delivery to the still-pending relay did not resume/settle after restart")
        case .threw(let message):
            XCTFail("recovery threw: \(message)")
        }
    }
}
