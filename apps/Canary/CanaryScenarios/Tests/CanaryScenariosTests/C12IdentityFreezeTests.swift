// C12 (docs/internals/canary.md "Scenario status"): identity freeze. A
// write belongs to the account that started it. Switch accounts while that
// write is still in flight and it must not be re-signed, re-attributed, or
// published under the new one.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as every other scenario
// here. Public `NMP` API only: plain `import NMP`, no `@testable`, no
// internal crate, no direct Redb inspection.
//
// C12 was BLOCKED, and this is the first run of it. `docs/internals/
// canary.md` records why: session identity did not survive restart, so
// there was no identity for a resumed write to stay frozen to. That
// blocker closed for C9 and C2 through `NMPSessionPayload`; C12 stayed
// unwritten. It needs no restart at all, as it turns out -- the switch
// this scenario cares about happens inside one live engine, which is also
// what an app actually does when a user taps a different account.
//
// TWO WRITES, TWO DIFFERENT MOMENTS TO BE FROZEN AT, and they fail
// differently.
//
//   1. UNSIGNED AND PARKED. The write is accepted under an account that
//      has NO signing provider, so it parks at
//      `SigningState.awaitingSigner(pubkey:)` -- accepted, durable, and
//      not yet signed by anybody. This is the one where re-resolution
//      would be invisible: `Identity.active` means "whoever is current
//      when this is ACCEPTED", and an engine that re-read the current
//      account at SIGNING time instead would produce a perfectly valid,
//      perfectly signed event by the wrong person. The signer for the
//      original account is then plugged in while the OTHER account is
//      current, and the event that goes out must carry the original key.
//
//   2. SIGNED AND UNDELIVERED. The write is signed immediately under an
//      account that does have a key, but its relay is a port that refuses
//      TCP connections, so the bytes sit in the queue. The account
//      switches, the relay comes back, and the delivered event must still
//      be the original author's.
//
// THE PRECONDITION IS THAT THE SWITCH HAPPENED MID-FLIGHT. "The write was
// published under the account that made it" is trivially true of a write
// that finished before anything was switched, and a scenario that switches
// accounts a moment too late proves nothing while looking identical. So
// both cases assert, with real captured values, that at the instant of the
// switch the write was still open -- `PublishQueueEntry.outcome == nil`,
// and for case 1 the signing state is still `awaitingSigner` -- and that
// the switch itself really took effect (`engine.session.current` is the
// new account, and the old account is still present but is not current).
//
// WHAT MAKES THE POSITIVE ASSERTION SHARP. Both cases end by asking the
// RELAY, over its own wire, for everything each of the two accounts ever
// wrote. The original account must have exactly the one event; the account
// that was current at the time must have NOTHING. A re-signed write would
// show up there as an event by the wrong author, and no amount of correct
// internal bookkeeping would hide it.
//
// Every wait is a bounded poll on a real condition with the real stuck
// values reported on timeout, never a fixed sleep used AS the oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C12IdentityFreezeTests: XCTestCase {
    // MARK: - What the receipt stream said

    private actor ReceiptLedger {
        private(set) var signingHistory: [String] = []
        private(set) var signedEventIDs: Set<String> = []
        private(set) var awaitingPubkeys: Set<String> = []
        private(set) var inFlightPubkeys: Set<String> = []
        private(set) var latestByRelay: [String: RelayState] = [:]
        private(set) var outcome: WriteOutcome?
        private(set) var ended: String?
        private(set) var factCount = 0

        func note(_ fact: WriteFact) {
            factCount += 1
            switch fact {
            case .signing(let state):
                let label = C12IdentityFreezeTests.label(state)
                if signingHistory.last != label { signingHistory.append(label) }
                switch state {
                case .signed(let eventID): signedEventIDs.insert(eventID)
                case .awaitingSigner(let pubkey): awaitingPubkeys.insert(pubkey)
                case .inFlight(let pubkey): inFlightPubkeys.insert(pubkey)
                case .refused: break
                }
            case .relay(_, let relay, let state):
                latestByRelay[relay] = state
            case .destinations:
                break
            case .outcome(let value):
                outcome = value
            }
        }

        func end(_ why: String) { ended = why }

        func isAwaiting(_ pubkey: String) -> Bool {
            awaitingPubkeys.contains(pubkey) && signedEventIDs.isEmpty
        }

        func hasPublished(_ relay: String) -> Bool {
            if case .published = latestByRelay[relay] { return true }
            return false
        }

        func snapshot() -> Snapshot {
            Snapshot(
                signingHistory: signingHistory, signedEventIDs: signedEventIDs,
                awaitingPubkeys: awaitingPubkeys, inFlightPubkeys: inFlightPubkeys,
                latestByRelay: latestByRelay, outcome: outcome, ended: ended,
                factCount: factCount
            )
        }
    }

    private struct Snapshot: Sendable {
        var signingHistory: [String]
        var signedEventIDs: Set<String>
        var awaitingPubkeys: Set<String>
        var inFlightPubkeys: Set<String>
        var latestByRelay: [String: RelayState]
        var outcome: WriteOutcome?
        var ended: String?
        var factCount: Int

        /// Every 64-hex public key the signing stage has ever named, in any
        /// state. "The write was never re-attributed" is exactly the claim
        /// that this set holds one key and it is the original one.
        var everyNamedPubkey: Set<String> { awaitingPubkeys.union(inFlightPubkeys) }
    }

    static func label(_ state: SigningState) -> String {
        switch state {
        case .awaitingSigner(let pubkey): return "awaitingSigner(\(pubkey.prefix(8)))"
        case .inFlight(let pubkey): return "inFlight(\(pubkey.prefix(8)))"
        case .signed(let eventID): return "signed(\(eventID.prefix(8)))"
        case .refused(let reason): return "refused(\(reason))"
        }
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

    /// One account's key material in both the lab's shape and NMP's. The
    /// lab generates it because the scenario needs the PUBLIC key on its
    /// own first -- an account added without a signer, so the write parks
    /// -- and hands NMP the PRIVATE key only later. `NMPPrivateKey` exposes
    /// no public-key accessor (deliberately: it renders redacted and yields
    /// no bytes), so a key generated through `NMPPrivateKey.generate()`
    /// cannot be added as a public-key-only account first.
    private struct LabAccount {
        let hex: String
        let publicKey: NMPPublicKey
        let privateKey: NMPPrivateKey

        init() throws {
            let pair = try NostrKeyPair()
            hex = pair.pubkeyHex
            publicKey = try NMPPublicKey(bytes: Data(pair.privateKey.xonly.bytes))
            privateKey = try NMPPrivateKey(bytes: pair.privateKey.dataRepresentation)
        }
    }

    // MARK: - Case 1: the write is parked, unsigned, when the account switches

    func testParkedWriteIsSignedByItsOriginalAccountNotWhicheverIsCurrentLater() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c12-case1-\(UUID().uuidString)")
        for sub in ["relay", "store"] {
            try FileManager.default.createDirectory(
                at: root.appendingPathComponent(sub), withIntermediateDirectories: true
            )
        }
        defer { try? FileManager.default.removeItem(at: root) }

        var log: [String] = []
        defer { print((["", "C12 case 1 phase log:"] + log).joined(separator: "\n")) }

        let relay = try await RelayHandle(
            name: "c12-case1-relay", workDir: root.appendingPathComponent("relay"),
            binaryPath: binaryPath
        )
        try await relay.start()

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: root.appendingPathComponent("store/nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { engine.shutdown() }

        // ALICE signs in with a public key only -- an ordinary app state: a
        // user whose signer (hardware, remote, an extension) is not
        // attached yet. She is current, so `Identity.active` resolves to
        // her.
        let alice = try LabAccount()
        let bob = try LabAccount()
        _ = try engine.session.add(publicKey: alice.publicKey, makeCurrent: true)
        let currentAtPublish = try engine.session.current?.publicKey.bytes.hexString
        let aliceSigningBefore = try engine.session.current?.signingAvailability
        log.append(
            "sign-in: alice=\(alice.hex.prefix(8)) bob=\(bob.hex.prefix(8)) "
                + "current=\(currentAtPublish?.prefix(8).description ?? "none") "
                + "aliceSigning=\(String(describing: aliceSigningBefore))"
        )
        XCTAssertEqual(currentAtPublish, alice.hex, "alice must be the current account at publish")

        let content = "C12 written by alice while bob is about to take over"
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

        // --- Precondition: the write is IN FLIGHT and unsigned -------------

        let parked = await waitUntil(timeout: 20) { await ledger.isAwaiting(alice.hex) }
        let atPark = await ledger.snapshot()
        let queuedBeforeSwitch = try engine.publishQueue(limit: 16)
        let entryBefore = queuedBeforeSwitch.first
        log.append(
            "parked: awaitingAlice=\(parked) signing=\(atPark.signingHistory) "
                + "named=\(atPark.everyNamedPubkey.map { String($0.prefix(8)) }) "
                + "signedIDs=\(atPark.signedEventIDs.count) queue=\(queuedBeforeSwitch.count) "
                + "queuePubkey=\(entryBefore?.pubkey.prefix(8).description ?? "nil") "
                + "queueSigning=\(entryBefore.map { Self.label($0.signing) } ?? "nil") "
                + "queueOutcome=\(String(describing: entryBefore?.outcome))"
        )
        XCTAssertTrue(
            parked,
            "PRECONDITION: 20s after accepting a write under an account with no signing provider, "
                + "the receipt reports \(atPark.signingHistory) rather than awaiting alice's "
                + "signer. Nothing is in flight, so there is nothing for the switch below to be "
                + "frozen against."
        )
        XCTAssertEqual(
            entryBefore?.pubkey, alice.hex,
            "PRECONDITION: the durable queue entry is frozen to "
                + "\(entryBefore?.pubkey ?? "nobody"), not to alice"
        )
        XCTAssertNil(
            entryBefore?.outcome,
            "PRECONDITION: the write already reached \(String(describing: entryBefore?.outcome)) "
                + "before the account switch, so the switch is not mid-flight"
        )

        // --- The switch, while the write is parked -------------------------

        _ = try engine.session.add(privateKey: bob.privateKey, makeCurrent: true)
        let currentAfterSwitch = try engine.session.current?.publicKey.bytes.hexString
        let accountsAfterSwitch = try engine.session.accounts.map { $0.publicKey.bytes.hexString }
        let entryAfterSwitch = try engine.publishQueue(limit: 16).first
        log.append(
            "switch: current=\(currentAfterSwitch?.prefix(8).description ?? "none") "
                + "accounts=\(accountsAfterSwitch.map { String($0.prefix(8)) }) "
                + "queuePubkey=\(entryAfterSwitch?.pubkey.prefix(8).description ?? "nil") "
                + "queueOutcome=\(String(describing: entryAfterSwitch?.outcome))"
        )
        XCTAssertEqual(
            currentAfterSwitch, bob.hex,
            "PRECONDITION: after adding bob as the current account, the current account is "
                + "\(currentAfterSwitch ?? "nobody") -- the switch did not happen, so nothing "
                + "below tests a switch"
        )
        XCTAssertTrue(
            accountsAfterSwitch.contains(alice.hex),
            "PRECONDITION: alice is no longer in the session at all, which is a removal, not the "
                + "account switch this scenario is about"
        )
        XCTAssertNil(
            entryAfterSwitch?.outcome,
            "PRECONDITION: the write settled during the switch itself, so it was not in flight "
                + "across it"
        )
        // Bob has a working signer and is current. An engine that re-read
        // the current account when it looked for a signer would sign right
        // here, and it would look completely normal.
        XCTAssertEqual(
            entryAfterSwitch?.pubkey, alice.hex,
            "the durable write is now attributed to "
                + "\(entryAfterSwitch?.pubkey ?? "nobody") after alice's write survived an "
                + "account switch to bob -- the write was RE-ATTRIBUTED"
        )

        // Bob is current, and can sign. Give the engine real time to do the
        // wrong thing: this window is where a re-resolving engine signs
        // alice's parked write with bob's key. Nothing else in the scenario
        // would notice, because the resulting event is perfectly valid.
        let signedUnderBob = await waitUntil(timeout: 5) {
            await !ledger.snapshot().signedEventIDs.isEmpty
        }
        let afterSwitch = await ledger.snapshot()
        log.append(
            "window with bob current and able to sign: anythingSigned=\(signedUnderBob) "
                + "signing=\(afterSwitch.signingHistory) "
                + "named=\(afterSwitch.everyNamedPubkey.map { String($0.prefix(8)) })"
        )
        XCTAssertFalse(
            signedUnderBob,
            "the parked write was SIGNED while bob was current and alice's signer was still "
                + "absent: \(afterSwitch.signingHistory). `Identity.active` freezes at "
                + "acceptance; this signed under a later current account."
        )
        XCTAssertEqual(
            afterSwitch.everyNamedPubkey, [alice.hex],
            "the signing stage has named \(afterSwitch.everyNamedPubkey) -- a write frozen to "
                + "alice must never name any other key"
        )

        // --- Alice's signer arrives. Bob is still the current account. ----
        //
        // The event that now goes out is the whole scenario: it must be
        // alice's, published while bob is signed in.

        _ = try engine.session.add(privateKey: alice.privateKey, makeCurrent: false)
        let currentWhileSigning = try engine.session.current?.publicKey.bytes.hexString
        let published = await waitUntil(timeout: 60) { await ledger.hasPublished(relay.url) }
        let final = await ledger.snapshot()
        let currentAtEnd = try engine.session.current?.publicKey.bytes.hexString
        log.append(
            "alice's signer arrives: currentWhileSigning="
                + "\(currentWhileSigning?.prefix(8).description ?? "none") published=\(published) "
                + "signing=\(final.signingHistory) signedIDs=\(final.signedEventIDs.count) "
                + "outcome=\(String(describing: final.outcome)) currentAtEnd="
                + "\(currentAtEnd?.prefix(8).description ?? "none")"
        )
        XCTAssertEqual(
            currentWhileSigning, bob.hex,
            "PRECONDITION: adding alice's private key made her current again "
                + "(\(currentWhileSigning ?? "nobody")). The point is that alice's write goes out "
                + "while BOB is signed in; if she is current there is no freeze being tested."
        )
        XCTAssertTrue(
            published,
            "60s after alice's signer became available, her parked write has not been published: "
                + "\(final.signingHistory), outcome \(String(describing: final.outcome)), stream "
                + "\(final.ended ?? "still open")"
        )
        XCTAssertEqual(currentAtEnd, bob.hex, "bob must still be the current account at the end")

        // --- Relay-side truth: who actually wrote what --------------------

        let aliceEvents = try await relay.queryIDsByAuthor(alice.hex, kinds: [1])
        let bobEvents = try await relay.queryIDsByAuthor(bob.hex, kinds: [1])
        let signedID = final.signedEventIDs.first ?? ""
        let stored = try await relay.queryById(signedID)
        log.append(
            "relay-side: alice has \(aliceEvents.count) event(s) \(aliceEvents.map { String($0.prefix(8)) }) "
                + "| bob has \(bobEvents.count) \(bobEvents.map { String($0.prefix(8)) }) "
                + "| stored pubkey=\(stored?["pubkey"] as? String ?? "nil") "
                + "content=\(stored?["content"] as? String ?? "nil")"
        )
        XCTAssertEqual(
            aliceEvents, [signedID],
            "the relay holds \(aliceEvents.count) event(s) by alice; her one write must be there "
                + "exactly once, under her own key"
        )
        XCTAssertEqual(
            bobEvents, [],
            "the relay holds \(bobEvents.count) event(s) by BOB, who never published anything. "
                + "Alice's in-flight write was re-signed and published under the account that "
                + "happened to be current."
        )
        XCTAssertEqual(
            stored?["pubkey"] as? String, alice.hex,
            "the published event is authored by \(stored?["pubkey"] as? String ?? "nobody") "
                + "rather than alice, who composed it"
        )
        XCTAssertEqual(stored?["content"] as? String, content)
        XCTAssertEqual(
            final.signedEventIDs.count, 1,
            "the write produced \(final.signedEventIDs.count) distinct signed events "
                + "(\(final.signedEventIDs)); one accepted intent is one event"
        )

        receiptPump.cancel()
        receipt.status.cancel()
        try await relay.kill()
    }

    // MARK: - Case 2: the write is signed but undelivered when the account switches

    func testSignedUndeliveredWriteKeepsItsAuthorAcrossAnAccountSwitch() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("canary-c12-case2-\(UUID().uuidString)")
        for sub in ["relay", "store"] {
            try FileManager.default.createDirectory(
                at: root.appendingPathComponent(sub), withIntermediateDirectories: true
            )
        }
        defer { try? FileManager.default.removeItem(at: root) }

        var log: [String] = []
        defer { print((["", "C12 case 2 phase log:"] + log).joined(separator: "\n")) }

        let relay = try await RelayHandle(
            name: "c12-case2-relay", workDir: root.appendingPathComponent("relay"),
            binaryPath: binaryPath
        )
        // Started, then killed: the destination is a real relay's port that
        // now refuses TCP connections, so the signed write cannot leave.
        try await relay.start()
        try await relay.kill()
        let downProbe = await relay.probe(timeout: 2)

        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: root.appendingPathComponent("store/nmp.redb").path,
                appRelays: [relay.url]
            )
        )
        defer { engine.shutdown() }

        let alice = try LabAccount()
        let bob = try LabAccount()
        _ = try engine.session.add(privateKey: alice.privateKey, makeCurrent: true)

        let content = "C12 signed by alice, delivered while bob is current"
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

        // --- Precondition: signed by alice, and NOT delivered -------------

        let signed = await waitUntil(timeout: 20) {
            await !ledger.snapshot().signedEventIDs.isEmpty
        }
        let atSign = await ledger.snapshot()
        let entryBefore = try engine.publishQueue(limit: 16).first
        log.append(
            "signed offline: relay=\(downProbe.outcome) signed=\(signed) "
                + "signing=\(atSign.signingHistory) queuePubkey="
                + "\(entryBefore?.pubkey.prefix(8).description ?? "nil") "
                + "queueOutcome=\(String(describing: entryBefore?.outcome)) "
                + "relayState=\(String(describing: atSign.latestByRelay[relay.url]))"
        )
        XCTAssertEqual(
            downProbe.outcome, .refused,
            "PRECONDITION: \(relay.url) is \(downProbe.outcome), so the signed write may already "
                + "have been delivered and nothing below is mid-flight"
        )
        XCTAssertTrue(signed, "the write was never signed: \(atSign.signingHistory)")
        XCTAssertEqual(entryBefore?.pubkey, alice.hex)
        XCTAssertNil(
            entryBefore?.outcome,
            "PRECONDITION: the write already reached \(String(describing: entryBefore?.outcome)) "
                + "with its only relay refusing connections"
        )

        // --- The switch, while the signed write is undelivered -------------

        _ = try engine.session.add(privateKey: bob.privateKey, makeCurrent: true)
        let currentAfterSwitch = try engine.session.current?.publicKey.bytes.hexString
        let entryAfterSwitch = try engine.publishQueue(limit: 16).first
        log.append(
            "switch: current=\(currentAfterSwitch?.prefix(8).description ?? "none") "
                + "queuePubkey=\(entryAfterSwitch?.pubkey.prefix(8).description ?? "nil") "
                + "queueOutcome=\(String(describing: entryAfterSwitch?.outcome))"
        )
        XCTAssertEqual(
            currentAfterSwitch, bob.hex,
            "PRECONDITION: the account switch did not take effect (current is "
                + "\(currentAfterSwitch ?? "nobody"))"
        )
        XCTAssertNil(
            entryAfterSwitch?.outcome,
            "PRECONDITION: the write settled during the switch, so it was not in flight across it"
        )
        XCTAssertEqual(
            entryAfterSwitch?.pubkey, alice.hex,
            "the undelivered write is now attributed to "
                + "\(entryAfterSwitch?.pubkey ?? "nobody") rather than alice"
        )

        // --- The relay returns; the write goes out under bob's session ----

        try await relay.restart()
        let published = await waitUntil(timeout: 120) { await ledger.hasPublished(relay.url) }
        let final = await ledger.snapshot()
        let currentAtEnd = try engine.session.current?.publicKey.bytes.hexString
        let aliceEvents = try await relay.queryIDsByAuthor(alice.hex, kinds: [1])
        let bobEvents = try await relay.queryIDsByAuthor(bob.hex, kinds: [1])
        let signedID = final.signedEventIDs.first ?? ""
        let stored = try await relay.queryById(signedID)
        log.append(
            "delivered: published=\(published) outcome=\(String(describing: final.outcome)) "
                + "signedIDs=\(final.signedEventIDs.count) currentAtEnd="
                + "\(currentAtEnd?.prefix(8).description ?? "none") | alice has \(aliceEvents.count) "
                + "bob has \(bobEvents.count) | stored pubkey="
                + "\(stored?["pubkey"] as? String ?? "nil")"
        )
        XCTAssertTrue(
            published,
            "120s after the relay came back, alice's signed write has not gone out: "
                + "\(final.signingHistory), relay \(String(describing: final.latestByRelay[relay.url])), "
                + "outcome \(String(describing: final.outcome))"
        )
        XCTAssertEqual(currentAtEnd, bob.hex, "bob must still be the current account at the end")
        XCTAssertEqual(
            stored?["pubkey"] as? String, alice.hex,
            "the delivered event is authored by \(stored?["pubkey"] as? String ?? "nobody") "
                + "rather than alice, who signed it before the switch"
        )
        XCTAssertEqual(stored?["content"] as? String, content)
        XCTAssertEqual(aliceEvents, [signedID], "alice's one event must be at the relay exactly once")
        XCTAssertEqual(
            bobEvents, [],
            "the relay holds \(bobEvents.count) event(s) by bob, who published nothing -- a "
                + "signed, undelivered write was re-signed under the account current at delivery"
        )
        XCTAssertEqual(
            final.signedEventIDs.count, 1,
            "the write produced \(final.signedEventIDs.count) distinct signed events; a re-signed "
                + "write is a different event, not the same one delivered later"
        )

        receiptPump.cancel()
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

private extension Data {
    var hexString: String { map { String(format: "%02x", $0) }.joined() }
}
