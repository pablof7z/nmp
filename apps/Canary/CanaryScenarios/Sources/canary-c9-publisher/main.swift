// canary-c9-publisher: the half of C9 that dies for real.
//
// C9CrashDuringPublicationTests.swift needs to prove crash safety against
// an actual `kill -9` of a separate OS process -- dropping an in-process
// `Engine` value and constructing a new one proves nothing, because
// ordinary Swift cleanup still runs. This executable is that process.
//
// It constructs an engine over a store path the parent hands it, adds and
// persists a local-key account (so the SAME account exists again after a
// restart -- crash recovery of a not-yet-signed write needs the same
// local signer back, exactly like the app's own session persistence),
// publishes one real signed event under a given correlation token to one
// or more real relays, prints machine-readable markers to stdout as it
// reaches each fact the parent needs to observe, then parks. It never
// retries and never polls beyond the ordinary `receipt.status` iteration
// `await-partial` mode does to learn when to print its own marker -- once
// a marker is printed, this process touches NMP again for nothing. There
// is no cleanup on purpose: the parent kills this process, not asks it to
// exit.
//
// Usage:
//   canary-c9-publisher <storePath> <sessionPayloadPath> <correlationToken> <mode> <relay1> [<relay2>]
//   mode: "plain" | "await-partial" (await-partial requires exactly 2 relays)

import Foundation
import NMP

@main
struct CanaryC9Publisher {
    static func main() async {
        do {
            try await run()
        } catch {
            FileHandle.standardError.write(Data("FAILED: \(error)\n".utf8))
            exit(1)
        }
    }

    static func run() async throws {
        setbuf(stdout, nil)
        let args = CommandLine.arguments
        guard args.count >= 6 else {
            print(
                "usage: canary-c9-publisher <storePath> <sessionPayloadPath> "
                    + "<correlationToken> <mode> <relay1> [<relay2>]"
            )
            exit(2)
        }
        let storePath = args[1]
        let sessionPayloadPath = args[2]
        let correlationToken = args[3]
        let mode = args[4]
        let relays = Array(args[5...])

        let engine = try NMPEngine(config: NMPConfig(storePath: storePath))

        // Add + persist the account BEFORE publishing. A crash between
        // acceptance and signature completion needs this same local
        // signer available again on restart -- this is the exact
        // Keychain-analogous step AppModel takes in the real app, just to
        // a plain file here since this is scenario scaffolding, not a
        // shipping app storing real secrets.
        let account = try engine.session.add(privateKey: NMPPrivateKey.generate(), makeCurrent: true)
        let payload = try engine.session.export()
        try payload.bytes.write(to: URL(fileURLWithPath: sessionPayloadPath))
        print("ACCOUNT:\(hex(account.publicKey.bytes))")

        let intent = WriteIntent(
            payload: .event(kind: 1, content: "C9 crash-during-publication"),
            routing: .explicit(relays: relays),
            correlation: correlationToken
        )
        // Reaching the next line without a thrown error IS local
        // acceptance -- same contract as C7.
        let receipt = try await engine.publish(intent)
        print("ACCEPTED:\(receipt.id)")

        if mode == "await-partial" {
            guard relays.count == 2 else {
                print("FAILED: await-partial mode needs exactly 2 relays, got \(relays.count)")
                exit(2)
            }
            for try await fact in receipt.status {
                guard case .relay(_, _, let state) = fact, case .published = state else { continue }
                // One relay reached the relay's own confirmed state. The
                // scenario's own setup guarantees the other relay is
                // partitioned (unreachable) at this moment, so a single
                // `.published` fact is already the partial-success shape
                // the parent is waiting for -- print immediately and stop
                // watching.
                print("PARTIAL:\(receipt.id)")
                break
            }
        }

        // Park. The parent kills this process; there is nothing further
        // for it to do, and doing anything further (retrying, polling)
        // would be exactly the thing this scenario must NOT exercise.
        while true {
            try await Task.sleep(nanoseconds: 1_000_000_000)
        }
    }
}

private func hex(_ data: Data) -> String {
    data.map { String(format: "%02x", $0) }.joined()
}
