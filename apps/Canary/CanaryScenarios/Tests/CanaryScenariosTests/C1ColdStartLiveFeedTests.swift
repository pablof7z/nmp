// C1 (docs/internals/canary.md "Scenario status"): cold start and live
// feed, against a real strfry child process launched by RelayLabKit.
//
// Run with `swift test` from apps/Canary/CanaryScenarios -- no xcodegen,
// no xcodebuild, no simulator; see that package's README.md for the two
// one-time prerequisites (the NMP xcframework, strfry). This package is
// macOS-only, not iOS -- RelayLabKit spawns the relay via
// `Foundation.Process`, which the iOS SDK does not expose at all (device
// or simulator). It drives the exact same public `NMP` Swift module the
// iOS `Canary` app links (`NMP`'s own Package.swift already declares
// macOS(.v13)), so this is the app's real read path, just built for the
// host platform instead of the simulator. Plain `import NMP` only -- no
// `@testable`, no internal crate, no direct Redb inspection: the relay
// lab is reached only over a real `ws://` URL to a separate strfry
// process (see docs/internals/canary.md "What is enforced, and what is
// only reviewed").
//
// Every wait below is a bounded race against a bounded `Task.sleep`
// deadline (the SAME shape `Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`
// already uses for its own live-relay bounds) -- never a fixed sleep used
// AS the synchronization oracle. A broken historical-catch-up path, a
// broken live-delivery path, or a duplicate canonical row all fail this
// test loudly rather than racing ahead or hanging CI.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C1ColdStartLiveFeedTests: XCTestCase {
    /// One continuous outcome for the whole scenario -- deliberately not
    /// three separate assertions racing three separate timeouts, because
    /// the query must stay open (one iterator, one subscription) from
    /// before the live publish to after it. Breaking out of `for try
    /// await batch in query` early tears the subscription down
    /// (`NMPQuery`'s own doc: "Demand teardown is ITERATOR-OWNED"), which
    /// would make the live half start a NEW subscription rather than
    /// observing the SAME one live -- exactly the thing C1 must prove.
    private enum Outcome: Sendable {
        case success(historicalRowCount: Int, finalRowCount: Int, finalIDs: Set<String>)
        case queryEndedEarly
        case timedOut
        case threw(String)
    }

    func testColdStartThenLiveDeliveryWithNoDuplicateRow() async throws {
        let binaryPath = try Self.locateStrfryBinary()

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c1-\(UUID().uuidString)")
        let workDir = root.appendingPathComponent("relay")
        try FileManager.default.createDirectory(at: workDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let relay = try await RelayHandle(name: "c1-relay", workDir: workDir, binaryPath: binaryPath)
        try await relay.start()

        let keyPair = try NostrKeyPair()
        // The historical fact: seeded BEFORE the app's engine ever exists,
        // over a real EVENT frame -- an "empty store, seeded relay" start.
        let historicalEvent = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C1 historical row"
        )
        try await relay.seed(historicalEvent)

        // The live fact: signed now, published only once the query has
        // already delivered the historical row (see the task body below).
        let liveEvent = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, content: "C1 live row"
        )

        // "Construct the engine normally": `storePath: nil` is NMP's own
        // documented spelling for an empty, engine-owned temporary store
        // (`NMPConfig.storePath`'s doc) -- not a lab-specific concept.
        let engine = try NMPEngine(
            config: NMPConfig(appRelays: [relay.url])
        )
        defer { engine.shutdown() }

        // "Open one feed observation": the plain public filter algebra,
        // no account/session needed for a literal-author read (mirrors
        // `LiveRelayTests.testAuthorsOwnNotesArriveThroughOperatorAppRelays`).
        let filter = NMPFilter(kinds: [1], authors: .literal([keyPair.pubkeyHex]))
        let query = try engine.observe(.single(NMPDemand(selection: filter)))

        let outcome = await withTaskGroup(of: Outcome.self) { group in
            group.addTask {
                do {
                    var sawHistorical = false
                    var historicalRowCount = 0
                    for try await batch in query {
                        if !sawHistorical {
                            guard batch.rows.contains(where: { $0.id == historicalEvent.id }) else {
                                continue
                            }
                            sawHistorical = true
                            historicalRowCount = batch.rows.count
                            // Still inside the SAME iterator/subscription --
                            // "publish another matching event through the
                            // relay" while the observation stays open.
                            do {
                                try await relay.seed(liveEvent)
                            } catch {
                                return .threw("seeding the live event failed: \(error)")
                            }
                            continue
                        }
                        guard batch.rows.contains(where: { $0.id == liveEvent.id }) else {
                            continue
                        }
                        return .success(
                            historicalRowCount: historicalRowCount,
                            finalRowCount: batch.rows.count,
                            finalIDs: Set(batch.rows.map(\.id))
                        )
                    }
                    return .queryEndedEarly
                } catch {
                    return .threw("\(error)")
                }
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 20_000_000_000)
                return .timedOut
            }

            let result = await group.next() ?? .timedOut
            group.cancelAll()
            return result
        }

        query.cancel()
        try await relay.kill()

        switch outcome {
        case .success(let historicalRowCount, let finalRowCount, let finalIDs):
            XCTAssertEqual(
                historicalRowCount, 1,
                "expected exactly the one seeded historical row before the live publish"
            )
            XCTAssertEqual(
                finalRowCount, 2,
                "expected exactly two rows after the live publish -- either fewer means the "
                    + "live event was wrongly treated as a duplicate of the historical one, or "
                    + "more means a duplicate canonical row was admitted"
            )
            XCTAssertEqual(
                finalIDs, [historicalEvent.id, liveEvent.id],
                "the exact two seeded ids, each exactly once"
            )
        case .queryEndedEarly:
            XCTFail("the query's AsyncSequence ended before observing both the historical and live rows")
        case .timedOut:
            XCTFail("C1 timed out (20s) waiting for the historical row and/or the live delivery")
        case .threw(let message):
            XCTFail("C1 threw: \(message)")
        }
    }

    /// `setup-strfry.sh`'s own default cache location (`$RELAY_LAB_CACHE_DIR`,
    /// or `~/Library/Caches/nmp-canary-relay-lab`). This test does not build
    /// strfry itself -- consistent with the lab's own "build once via the
    /// documented script, never vendor the binary" design -- so a machine
    /// that has not run `apps/Canary/setup-strfry.sh` skips rather than
    /// fails, the same shape `LiveRelayTests` uses for an unreachable network.
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
