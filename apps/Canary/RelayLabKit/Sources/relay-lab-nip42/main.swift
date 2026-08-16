// relay-lab-nip42: attempts the fix for the gap the Python-prototype
// investigation left open -- "write accepted after AUTH" against a real
// strfry process with `auth.enabled=true`, using a NIP-42 handshake this
// controller drives itself (see RelayLabKit/NIP42Handshake.swift) instead
// of shelling out to a third-party CLI.
//
// Usage: relay-lab-nip42 <path-to-strfry-binary>

import Foundation
import RelayLabKit

@main
struct RelayLabNIP42 {
    static func main() async {
        setbuf(stdout, nil)
        do {
            try await run()
        } catch {
            FileHandle.standardError.write(Data("FAILED: \(error)\n".utf8))
            exit(1)
        }
    }

    static func run() async throws {
        guard CommandLine.arguments.count > 1 else {
            print("usage: relay-lab-nip42 <path-to-strfry-binary>")
            exit(2)
        }
        let binaryPath = URL(fileURLWithPath: CommandLine.arguments[1])

        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-lab-nip42-\(UUID().uuidString)")
        let workDir = root.appendingPathComponent("auth-host")
        try FileManager.default.createDirectory(at: workDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        // CORRECTION vs. the earlier Python investigation: a `writePolicy`
        // plugin gating on `authed` DOES deny writes (real, verified), but
        // reading strfry's own source (RelayIngester.cpp) during this port
        // showed `sendAuthChallenge` is called from exactly THREE places --
        // NIP-70 "protected event" writes (a literal `-` tag) and the two
        // `restrictedReadKinds` query paths -- and NEVER from the generic
        // writePolicy-plugin rejection path. A plugin can reject a write
        // with a message that SAYS "auth-required:", but that alone never
        // makes strfry emit a real `["AUTH", challenge]` frame. This is
        // why the earlier Python probe's "denied without auth: True"
        // result, while a real denial, was not proof a challenge was ever
        // sent -- it wasn't (confirmed live below: an unmarked event never
        // got a challenge on this same relay, only "auth-required: event
        // marked as protected" once the plugin/tag mismatch was found).
        // The correct, native trigger is NIP-70: mark the seed event
        // itself as protected (`tags: [["-"]]`).
        let relay = try await RelayHandle(name: "auth-host", workDir: workDir, binaryPath: binaryPath)
        let serviceURL = relay.url
        let conf = """
        db = "\(relay.dataDir.path)/"
        relay {
            bind = "127.0.0.1"
            port = \(relay.port)
            auth {
                enabled = true
                serviceUrl = "\(serviceURL)"
            }
        }
        """
        try relay.overrideConfig(conf)

        print("[1] starting real strfry with auth.enabled=true")
        try await relay.start()
        print("    relay up at \(relay.url)")

        let keyPair = try NostrKeyPair()
        let event = try NostrSigning.sign(
            keyPair: keyPair, kind: 1, tags: [["-"]], content: "nip42-swift-probe (NIP-70 protected)"
        )

        print("[2] publishing on ONE connection, letting the controller drive the AUTH handshake itself")
        let result = try await NIP42Client.publishWithAuth(event: event, relayURL: relay.url, keyPair: keyPair)

        switch result {
        case .acceptedWithoutAuth:
            print("    UNEXPECTED: accepted without auth at all -- plugin gating did not engage")
            try await relay.kill()
            exit(1)
        case .acceptedAfterAuth(let challenge):
            print("    SUCCESS: write accepted after AUTH (challenge=\(challenge))")
            print("\nNIP-42 RECOVERY-AFTER-AUTH: PROVEN LIVE against real strfry")
        case .deniedAfterAuth(let challenge, let message):
            print("    FAILED: relay still denied after AUTH (challenge=\(challenge)): \(message)")
            try await relay.kill()
            print("\nNIP-42 RECOVERY-AFTER-AUTH: STILL NOT PROVEN -- see message above")
            exit(1)
        case .deniedNoChallengeOffered(let message):
            print("    FAILED: denied without ever offering a challenge: \(message)")
            try await relay.kill()
            exit(1)
        }

        try await relay.kill()
    }
}
