// relay-lab-lifecycle: Swift port of scratchpad/canary-lab/lab.py.
//
// Usage: relay-lab-lifecycle <path-to-strfry-binary>
//
// Demonstrates: start a real strfry child process on an ephemeral port +
// isolated temp data dir, wait for a REAL TCP accept (bounded poll, never
// a sleep), publish one seed event over the real wire, confirm it queries
// back, kill -9 the process, restart a NEW process against the SAME data
// dir, and confirm the event survived on disk.

import Foundation
import RelayLabKit

@main
struct RelayLabLifecycle {
    static func main() async {
        do {
            try await run()
        } catch {
            FileHandle.standardError.write(Data("FAILED: \(error)\n".utf8))
            exit(1)
        }
    }

    static func run() async throws {
        guard CommandLine.arguments.count > 1 else {
            print("usage: relay-lab-lifecycle <path-to-strfry-binary>")
            exit(2)
        }
        let binaryPath = URL(fileURLWithPath: CommandLine.arguments[1])

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-lab-swift-\(UUID().uuidString)")
        let workDir = root.appendingPathComponent("host-a")
        try FileManager.default.createDirectory(at: workDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        print("[1] starting real strfry child process")
        let relay = try await RelayHandle(name: "host-a", workDir: workDir, binaryPath: binaryPath)
        let t0 = Date()
        try await relay.start()
        print(String(format: "    accepted a real TCP connection after %.3fs (port=%d)", -t0.timeIntervalSinceNow, relay.port))

        print("[2] publishing one seed event over the real wire (EVENT frame + OK)")
        let keyPair = try NostrKeyPair()
        let event = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "canary-lab-swift seed event")
        _ = try await relay.seed(event)
        print("    published event \(event.id)")

        print("[3] confirming it round-trips over a REQ query")
        guard try await relay.queryById(event.id) != nil else {
            throw NSError(domain: "relay-lab", code: 1, userInfo: [NSLocalizedDescriptionKey: "seeded event not queryable before kill"])
        }
        print("    queried it back")

        print("[4] kill -9 the relay process")
        try await relay.kill()
        print("    process terminated")

        print("[5] restarting a NEW process against the SAME data_dir + port")
        try await relay.restart()
        print("    new process started, same port=\(relay.port), same data_dir=\(relay.dataDir.path)")

        print("[6] querying the event back from the restarted process")
        guard let got = try await relay.queryById(event.id) else {
            print("    FAIL: event did not survive restart")
            try await relay.kill()
            exit(1)
        }
        guard (got["id"] as? String) == event.id else {
            print("    FAIL: id mismatch")
            try await relay.kill()
            exit(1)
        }
        print("    SUCCESS: event \(event.id) survived a kill+restart against the same on-disk LMDB database")

        print("[7] partition: SIGSTOP the process, confirm a seed attempt no longer completes")
        relay.partition()
        let partitionedEvent = try NostrSigning.sign(keyPair: keyPair, kind: 1, content: "should not be admitted while partitioned")
        var sawPartitionEffect = false
        do {
            _ = try await relay.seed(partitionedEvent, timeout: 2)
            print("    UNEXPECTED: seed completed while the process was SIGSTOPped")
        } catch {
            sawPartitionEffect = true
            print("    as expected, seed did not complete while partitioned (\(error))")
        }

        print("[8] heal: SIGCONT the process, confirm it resumes answering")
        relay.heal()
        _ = try await relay.seed(partitionedEvent)
        let healedQuery = try await relay.queryById(partitionedEvent.id)
        guard healedQuery != nil, sawPartitionEffect else {
            print("    FAIL: relay did not resume correctly after heal")
            try await relay.kill()
            exit(1)
        }
        print("    SUCCESS: relay resumed and admitted a write after SIGCONT")

        try await relay.kill()
        let dbFiles = (try? FileManager.default.contentsOfDirectory(atPath: relay.dataDir.path)) ?? []
        print("    on-disk db files left behind: \(dbFiles.sorted())")

        print("\nALL STEPS PASSED (real strfry subprocess, real websocket, real LMDB persistence)")
    }
}
