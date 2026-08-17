// canary-c2-warmer: the ONLINE half of C2, in a process that then exits.
//
// C2 claims that an app which has been used online still shows its feed
// after a restart with nothing reachable. The word doing all the work is
// "restart". Constructing a second `NMPEngine` over the same store path
// inside the process that just filled it is a much weaker claim than it
// looks: the Redb pages, the allocator, and every row the first engine
// ever decoded are all still in that process's address space, so a read
// that quietly came from somewhere other than the durable file would look
// identical.
//
// So the online half runs HERE, and this process is fully gone -- exited,
// waited on, `terminationStatus` checked -- before the scenario opens the
// store again. Whatever the parent reads afterwards came off disk, from a
// process that never saw these events in memory. C9 spawns a child for a
// different reason (`kill -9` proves nothing against an in-process
// `Engine` drop); this is the same split for the cold-read reason.
//
// This is an ORDINARY app session, not a crash: it publishes nothing,
// exports its session the way a shipped app persists one, and quits
// cleanly. The crash case is C9's and is not restated here.
//
// Public `NMP` API only: `import NMP`, no `@testable`, no internal crate,
// no Redb inspection. It talks to the relay by URL over a real socket; the
// relay's lifecycle stays the parent's job, so there is no RelayLabKit
// dependency here.
//
// Usage:
//   canary-c2-warmer <storePath> <sessionPayloadPath> <relay> <authorHex> <expectedRows>
//
// Output, in order:
//   ACCOUNT:<hex>            the local-key account this session is signed in as
//   CACHED:<id> <id> ...     every row delivered, each with the relay in its sources
//   SESSION:<byteCount>      the exported payload, written to <sessionPayloadPath>
// or FAILED:<reason> with a non-zero exit.

import Foundation
import NMP

@main
struct CanaryC2Warmer {
    static func main() async {
        setbuf(stdout, nil)
        let args = CommandLine.arguments
        guard args.count == 6, let expectedRows = Int(args[5]) else {
            print("usage: canary-c2-warmer <storePath> <sessionPayloadPath> <relay> <authorHex> <expectedRows>")
            exit(2)
        }
        let storePath = args[1]
        let sessionPayloadPath = args[2]
        let relay = args[3]
        let authorHex = args[4]

        do {
            try await run(
                storePath: storePath, sessionPayloadPath: sessionPayloadPath,
                relay: relay, authorHex: authorHex, expectedRows: expectedRows
            )
        } catch {
            print("FAILED:\(error)")
            exit(1)
        }
    }

    static func run(
        storePath: String,
        sessionPayloadPath: String,
        relay: String,
        authorHex: String,
        expectedRows: Int
    ) async throws {
        let engine = try NMPEngine(
            config: NMPConfig(storePath: storePath, appRelays: [relay])
        )

        // The app signs in. A generated local key, made current, exactly as
        // `AppModel` does -- the identity C2 requires to still be there
        // after the restart.
        let account = try engine.session.add(privateKey: .generate(), makeCurrent: true)
        print("ACCOUNT:\(account.publicKey.bytes.map { String(format: "%02x", $0) }.joined())")

        // The app reads its feed. The loop deliberately does NOT stop at the
        // first batch carrying the right row count: it waits until every row
        // names the RELAY in its `sources`. Without that, a run where the
        // rows somehow came from anywhere else would still print CACHED and
        // the whole scenario would rest on rows the network never delivered.
        let query = try engine.observe(NMPFilter(kinds: [1], authors: .literal([authorHex])))
        let cached: [String] = try await withThrowingTaskGroup(of: [String]?.self) { group in
            group.addTask {
                for try await batch in query {
                    guard batch.rows.count == expectedRows else { continue }
                    guard batch.rows.allSatisfy({ $0.sources.contains(relay) }) else { continue }
                    return batch.rows.map(\.id)
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 30_000_000_000)
                return nil
            }
            let first = try await group.next() ?? nil
            group.cancelAll()
            return first ?? []
        }
        query.cancel()

        guard cached.count == expectedRows else {
            print("FAILED:the relay delivered \(cached.count) rows, expected \(expectedRows)")
            engine.shutdown()
            exit(1)
        }
        print("CACHED:\(cached.joined(separator: " "))")

        // The app persists its session the way a shipped app does -- one
        // opaque value, written whole.
        let payload = try engine.session.export()
        try payload.bytes.write(to: URL(fileURLWithPath: sessionPayloadPath))
        print("SESSION:\(payload.bytes.count)")

        // A clean quit, not a crash.
        engine.shutdown()
        exit(0)
    }
}
