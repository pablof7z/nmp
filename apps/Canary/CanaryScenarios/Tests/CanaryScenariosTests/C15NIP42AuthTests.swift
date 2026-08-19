// C15 (docs/internals/canary.md "Scenario status", #1887): NIP-42 AUTH,
// driven through NMP.
//
// THIS FILE FOUND AND NOW GUARDS #1889. It was committed RED, as the
// reproduction of a bootstrap deadlock against any relay that challenges in
// response to a request; #1889 deleted the deadlock and this is now the
// regression guard. The block below the scenario description keeps the
// diagnosis, because the control phase that pinned it is still here.
//
// THE POINT OF THIS FILE, stated first because C15 already has one wrong
// answer on the record. `canary.md` previously called C15 "proven live"
// on the strength of a run that drove the AUTH handshake from the LAB
// CONTROLLER. That proved strfry can be authenticated against; it proved
// nothing whatsoever about NMP. This scenario is the other half: every
// AUTH frame below is minted, signed and sent by `NMPEngine`, and the
// only thing the controller does is establish -- independently, and
// before NMP is involved at all -- that the relay genuinely refuses to
// serve this data without it.
//
// Run with `swift test` from apps/Canary/CanaryScenarios (see that
// package's README.md). macOS only, same reason as C1/C7/C9/C13/C17.
// Public `NMP` API only: plain `import NMP`, no `@testable`, no internal
// crate, no direct Redb inspection, no test-only constructor. The relay is
// reached only over a real `ws://` URL to a separate strfry process.
//
// HOW THE RELAY IS MADE TO DEMAND AUTH, and why it is not a plugin.
// `canary.md` records the mistake worth not repeating: a strfry
// `writePolicy` plugin can reject with a message that SAYS `auth-required`
// while `sendAuthChallenge()` is never called, because the plugin
// rejection path is not one of its three call sites. So this scenario uses
// a real native trigger instead -- `relay.auth.restrictedReadKinds`, which
// gates READS of the listed kinds behind a genuine challenge
// (`RelayIngester.cpp`: `sendAuthChallenge(connId, challenge)` followed by
// `CLOSED ... auth-required: requested filter requires authentication`).
//
// AND WHY `restrictReadToInvolvedPubkey` IS LEFT ON. With it enabled,
// strfry additionally filters each restricted-kind event per subscriber:
// only a connection authenticated as the event's author or its `p`-tagged
// recipient is served the row. The seeded events below are `p`-tagged to
// NMP's OWN account, so "the row arrived" is not merely "somebody
// authenticated" -- it is "NMP authenticated as exactly the account the
// demand named". Those are different claims, and only the second one is
// worth anything.
//
// WHAT THAT MAKES THE ROW MEAN. strfry validates an incoming kind:22242
// against the challenge it minted for THAT connection
// (`if (!foundChallenge) throw herr("challenge string mismatch")`) and
// against its own configured `serviceUrl` (`if (!foundCorrectRelayUrl)
// throw herr("incorrect or missing relay tag")`). A signed proof bound to
// the wrong challenge, the wrong relay, or the wrong key does not
// authenticate the connection, and the row never comes. So a delivered row
// is a proof, not a coincidence: NMP noticed the challenge, signed a
// kind:22242 event bound to that exact challenge and this exact relay, and
// the relay accepted it.
//
// THE PRECONDITIONS ARE ASSERTED, NOT ASSUMED -- C13's fourth falsifier
// is the lesson. A relay that never challenges anybody would make "NMP
// answered the challenge" vacuously green, so before NMP is constructed a
// plain, NMP-free client (`RelayHandle.probeRead`) is refused by the same
// relay with the same filter, and its verbatim challenge and refusal text
// are captured and printed. A second probe authenticates as a DIFFERENT
// key and is served nothing, which is what makes the identity half of the
// claim falsifiable at all.
//
// WHAT WAS WRONG, AND WHY THE CONTROL PHASE IS STILL HERE. Every
// precondition above passed while the scenario was red -- the relay demands
// AUTH, the challenge is real, the identity scoping is live, the row is
// retrievable by a client that authenticates. NMP then opened the
// identity-bound session, reported it under exactly the requested access
// identity, reached `awaitingAuth(awaitingChallenge)` and stayed there
// forever. It sent NOTHING: not the AUTH proof, not even the REQ. The
// installed policy was never consulted.
//
// The cause was a bootstrap deadlock, and the control phase below pinned it
// with a public-API measurement rather than a reading of NMP's source:
//
//   - NMP parked every request on a protected session until that session
//     had completed AUTH, and only created an AUTH session when an inbound
//     `["AUTH", challenge]` frame arrived.
//   - strfry only ever emits that frame IN RESPONSE to a request it wants
//     to gate -- a `restrictedReadKinds` read or a NIP-70 protected write.
//     It never challenges on bare connect. `canary.md` already records
//     both of its genuine triggers as "challenge-driven".
//
// So NMP waited for a challenge that the relay would only send once NMP
// sent the request NMP was withholding until it was challenged. Neither
// side was wrong on its own; together they never exchanged a byte.
//
// #1889 deleted the withholding: a read session sends its planned REQs
// whether or not it names an identity, and the relay's `CLOSED
// auth-required` plus `["AUTH", challenge]` is what answers it. The control
// observation stays because it is what makes a REGRESSION legible -- if the
// subject ever stalls again, the same two status histories say so. It also
// makes an assertion hazard, handled: the control shares this relay, this
// filter and (through `makeCurrent: true`) this account, so every
// negotiation assertion below is correlated to the SUBJECT session through
// the engine's own `AuthDiagnostics.authenticateAs` before it is believed
// (`subjectPolicyRequests`).
//
// Left failing rather than reshaped is what closed it, on the same
// principle C17's #1846 phase was left red: the scenario is not weakened
// until something passes. It was written as the scenario that would be
// green when the gap closed, so closing the gap is what turned it green.
// See the finding in `docs/internals/canary.md`.
//
// Every wait below is a bounded poll on a real condition with the real
// stuck value reported on timeout -- never a fixed sleep used AS the
// synchronization oracle.

import Foundation
import XCTest
import NMP
import RelayLabKit

final class C15NIP42AuthTests: XCTestCase {
    // MARK: - The app's AUTH policy

    /// An ordinary application's `NMPAuthPolicy`: it records every request
    /// NMP hands it and answers with whatever the scenario currently wants.
    ///
    /// The recording is the evidence. `NMPAuthPolicyRequest` is the only
    /// public surface carrying the RAW challenge string, so this is the one
    /// place a scenario can check that NMP is answering the challenge the
    /// relay actually minted rather than something of its own devising, and
    /// that a reconnect brings a genuinely different one.
    private final class RecordingAuthPolicy: NMPAuthPolicy, @unchecked Sendable {
        private let lock = NSLock()
        private var mode: NMPAuthPolicyOutcome
        private var requests: [NMPAuthPolicyRequest] = []
        private var cancellations: [NMPAuthPolicyRequest] = []
        private var resolveErrors: [String] = []

        init(mode: NMPAuthPolicyOutcome) {
            self.mode = mode
        }

        func setMode(_ mode: NMPAuthPolicyOutcome) {
            lock.lock()
            self.mode = mode
            lock.unlock()
        }

        func evaluate(request: NMPAuthPolicyRequest, completion: NMPAuthPolicyCompletion) {
            lock.lock()
            requests.append(request)
            let decision = mode
            lock.unlock()
            do {
                try completion.resolve(decision)
            } catch {
                lock.lock()
                resolveErrors.append("\(error)")
                lock.unlock()
            }
        }

        func onCancelled(request: NMPAuthPolicyRequest) {
            lock.lock()
            cancellations.append(request)
            lock.unlock()
        }

        var seen: [NMPAuthPolicyRequest] {
            lock.lock()
            defer { lock.unlock() }
            return requests
        }

        var cancelled: [NMPAuthPolicyRequest] {
            lock.lock()
            defer { lock.unlock() }
            return cancellations
        }

        var errors: [String] {
            lock.lock()
            defer { lock.unlock() }
            return resolveErrors
        }

        /// Every DISTINCT raw challenge NMP has asked about, in first-seen
        /// order. strfry mints a fresh challenge per connection id, so this
        /// growing by one across a relay restart is the reconnect claim.
        var distinctChallenges: [String] {
            var seenSet = Set<String>()
            var ordered: [String] = []
            for request in seen where seenSet.insert(request.challenge).inserted {
                ordered.append(request.challenge)
            }
            return ordered
        }

        func describe() -> String {
            let lines = seen.map {
                "  relay=\($0.relay) expected=\($0.expectedPublicKey.prefix(12))… "
                    + "challenge=\($0.challenge) gen=\($0.transportGeneration) "
                    + "epoch=\($0.epochSequence)"
            }
            return (["policy saw \(seen.count) request(s):"] + lines
                + ["  cancelled=\(cancelled.count) resolveErrors=\(errors)"])
                .joined(separator: "\n")
        }
    }

    // MARK: - What the observation has been delivered

    private struct ObservedState: Sendable {
        var latest: Set<String> = []
        var everSeen: Set<String> = []
        var batches = 0
        /// Newest per-source status for the lab relay, as a short label.
        var relayStatus = "(never reported)"
        /// Every distinct status label, in arrival order. This is where the
        /// AUTH negotiation shows up: `awaitingAuth(awaitingChallenge)` ->
        /// `awaitingAuth(awaitingPolicy)` -> ... -> `requesting`.
        var statusHistory: [String] = []
        /// Every distinct authenticated identity this source was reported
        /// under. A `nip42(<key>)` entry is NMP naming the identity it is
        /// holding the session as -- a public fact, not an inference.
        var accessHistory: [String] = []
        var ended: String?
    }

    private actor ObservationLedger {
        private var state = ObservedState()

        func record(_ batch: RowBatch, relayURL: String) {
            state.batches += 1
            let ids = batch.rows.map(\.id)
            state.latest = Set(ids)
            state.everSeen.formUnion(ids)
            guard let source = batch.evidence.first?.sources
                .first(where: { $0.relay == relayURL })
            else { return }
            let label = Self.label(source.status)
            state.relayStatus = label
            if state.statusHistory.last != label { state.statusHistory.append(label) }
            let access = Self.label(source.authenticateAs)
            if state.accessHistory.last != access { state.accessHistory.append(access) }
        }

        func markEnded(_ why: String) { state.ended = why }
        func current() -> ObservedState { state }

        /// Spelled out rather than `String(describing:)`, same reason as
        /// C13: the classification this scenario depends on is a decision
        /// made in the open, not one inherited from a synthesized
        /// description that could change shape underneath it.
        static func label(_ status: SourceStatus) -> String {
            switch status {
            case .requesting: return "requesting"
            case .finishedStoredEvents: return "finishedStoredEvents"
            case .awaitingRequest: return "awaitingRequest"
            case .coverageSatisfied: return "coverageSatisfied"
            case .connecting: return "connecting"
            case .disconnected: return "disconnected"
            case .awaitingAuth(let phase): return "awaitingAuth(\(label(phase)))"
            case .authDenied: return "authDenied"
            case .error: return "error"
            }
        }

        static func label(_ phase: AuthPhase) -> String {
            switch phase {
            case .awaitingChallenge: return "awaitingChallenge"
            case .awaitingPolicy: return "awaitingPolicy"
            case .awaitingSignature: return "awaitingSignature"
            case .awaitingSend: return "awaitingSend"
            case .awaitingRelayAck: return "awaitingRelayAck"
            case .ready: return "ready"
            case .denied: return "denied"
            case .error: return "error"
            }
        }

        static func label(_ authenticateAs: String?) -> String {
            guard let authenticateAs else { return "public" }
            return "nip42(\(authenticateAs))"
        }
    }

    /// `observeDiagnostics()` is PUSH-only -- there is no synchronous "what
    /// is your current snapshot" on `NMPEngine`, so an app that wants a
    /// point-in-time reading must hold the stream and cache the last value.
    /// C13 and C17 each contain these same lines; the duplication is the
    /// evidence (`canary.md`: "a little duplication is preferable to hiding
    /// evidence").
    ///
    /// This one additionally keeps the UNION of every `AuthDiagnostics` row
    /// ever pushed, because AUTH phases are transient by nature: a snapshot
    /// taken after the handshake finished shows `.ready` and nothing of how
    /// it got there, and a scenario that only ever reads the latest value
    /// cannot tell "it authenticated" from "it never had to".
    private final class LatestDiagnostics: @unchecked Sendable {
        private let lock = NSLock()
        private var value = DiagnosticsSnapshot()
        private var authRows: [AuthDiagnostics] = []

        func store(_ snapshot: DiagnosticsSnapshot) {
            lock.lock()
            value = snapshot
            for row in snapshot.authSessions where !authRows.contains(row) {
                authRows.append(row)
            }
            lock.unlock()
        }

        func current() -> DiagnosticsSnapshot {
            lock.lock()
            defer { lock.unlock() }
            return value
        }

        /// Every distinct AUTH-session row ever observed, in first-seen
        /// order.
        func allAuthRows() -> [AuthDiagnostics] {
            lock.lock()
            defer { lock.unlock() }
            return authRows
        }

        func describeAuth() -> String {
            let rows = allAuthRows().map {
                "  relay=\($0.relay) authenticateAs=\(ObservationLedger.label($0.authenticateAs)) "
                    + "gen=\($0.transportGeneration) epoch=\($0.epochSequence.map(String.init) ?? "nil") "
                    + "descriptor=\($0.challengeDescriptor ?? "nil") "
                    + "phase=\(ObservationLedger.label($0.phase)) "
                    + "policyBound=\($0.policyBound) signerBound=\($0.signerBound) "
                    + "authEventID=\($0.authEventID ?? "nil")"
            }
            return (["diagnostics saw \(rows.count) distinct AUTH row(s):"] + rows)
                .joined(separator: "\n")
        }
    }

    /// The subset of `policy.seen` that belongs to the session bound to
    /// `identityHex`.
    ///
    /// `RecordingAuthPolicy` is installed per public key, not per
    /// observation, so `policy.seen` is engine-global. In this scenario the
    /// CONTROL observation shares the subject's relay, its filter, and --
    /// because the account was added with `makeCurrent: true` -- the very
    /// key the policy is registered under, so a request provoked by the
    /// control would satisfy every field-level assertion about `relay`,
    /// `expectedPublicKey` and `challenge` while the identity-bound session
    /// was still transmitting nothing. That is a test that passes off the
    /// wrong session's work as the subject's.
    ///
    /// `AuthDiagnostics` is the surface that can tell them apart: it names
    /// the session's frozen `authenticateAs`, and both types carry the
    /// engine's `(relay, transportGeneration, epochSequence)` epoch
    /// coordinates. Correlating on those three restores "the SUBJECT
    /// negotiated" as the claim under test.
    private func subjectPolicyRequests(
        boundTo identityHex: String,
        policy: RecordingAuthPolicy,
        diagnostics: LatestDiagnostics
    ) -> [NMPAuthPolicyRequest] {
        let boundEpochs = Set(
            diagnostics.allAuthRows()
                .filter { $0.authenticateAs == identityHex }
                .compactMap { row -> String? in
                    guard let sequence = row.epochSequence else { return nil }
                    return "\(row.relay)|\(row.transportGeneration)|\(sequence)"
                }
        )
        return policy.seen.filter {
            boundEpochs.contains(
                "\($0.relay)|\($0.transportGeneration)|\($0.epochSequence)"
            )
        }
    }

    @discardableResult
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

    // MARK: - Scenario A: the round trip, and re-AUTH after a reconnect

    func testNMPAnswersTheChallengeAndReauthenticatesAfterAReconnect() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let lab = try await Lab(name: "c15a", binaryPath: binaryPath)
        defer { lab.cleanUp() }

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C15-A phase log:"] + log).joined(separator: "\n")) }

        // The engine exists first, only so its account's public key is known
        // before anything is seeded: the events below are `p`-tagged to it,
        // which is what makes "NMP authenticated as THIS account" a
        // falsifiable claim rather than "some AUTH happened".
        let engine = try NMPEngine(config: NMPConfig(storePath: lab.storePath, appRelays: []))
        defer { engine.shutdown() }
        let account = try engine.session.add(privateKey: NMPPrivateKey.generate(), makeCurrent: true)
        let accountHex = Self.hex(account.publicKey.bytes)

        let seeder = try NostrKeyPair()
        let first = try NostrSigning.sign(
            keyPair: seeder, kind: Lab.restrictedKind,
            tags: [["p", accountHex]], content: "C15 restricted note, before AUTH"
        )
        try await lab.relay.seed(first)

        // --- Phase 0: the relay genuinely demands AUTH (NMP-free) ---------
        //
        // Two plain clients, neither of which has ever heard of NMP. The
        // first is refused; the second authenticates as the WRONG key and
        // is served nothing. Without both, "the row arrived" would be
        // consistent with a relay that asks nobody for anything.

        let wireFilter: [String: Any] = ["kinds": [Lab.restrictedKind], "authors": [seeder.pubkeyHex]]

        let unauthenticated = try await lab.relay.probeRead(filter: wireFilter)
        let stranger = try NostrKeyPair()
        let wrongIdentity = try await lab.relay.probeRead(
            filter: wireFilter, authenticateAs: stranger
        )
        note(
            "qualification: unauthenticated challenge=\(unauthenticated.challenge ?? "nil") "
                + "closed=\(unauthenticated.closedMessage ?? "nil") "
                + "served=\(unauthenticated.servedEventIDs) | wrong-identity "
                + "challenge=\(wrongIdentity.challenge ?? "nil") "
                + "authAccepted=\(wrongIdentity.authAccepted.map(String.init) ?? "nil") "
                + "authMessage=\(wrongIdentity.authMessage ?? "nil") "
                + "eose=\(wrongIdentity.reachedEOSE) served=\(wrongIdentity.servedEventIDs)"
        )
        XCTAssertNotNil(
            unauthenticated.challenge,
            "PRECONDITION: an unauthenticated plain client issuing this exact filter got no "
                + "[\"AUTH\", challenge] frame from the relay. Frames observed: "
                + "\(unauthenticated.frames). Everything below would then be asserting that NMP "
                + "answered a challenge nobody ever sent."
        )
        XCTAssertTrue(
            (unauthenticated.closedMessage ?? "").contains("auth-required"),
            "PRECONDITION: the relay's refusal was \(unauthenticated.closedMessage ?? "nil"), not an "
                + "auth-required CLOSED. Frames: \(unauthenticated.frames)"
        )
        XCTAssertTrue(
            unauthenticated.servedEventIDs.isEmpty,
            "PRECONDITION: the relay served \(unauthenticated.servedEventIDs) to an "
                + "UNAUTHENTICATED client, so AUTH is not actually gating this read and NMP "
                + "receiving the row below would prove nothing."
        )
        XCTAssertEqual(
            wrongIdentity.authAccepted, true,
            "PRECONDITION: the stranger's own kind:22242 handshake was not accepted "
                + "(\(wrongIdentity.authMessage ?? "nil")), so the negative probe below is not "
                + "measuring identity scoping -- it is measuring a failed handshake. Frames: "
                + "\(wrongIdentity.frames)"
        )
        XCTAssertTrue(
            wrongIdentity.servedEventIDs.isEmpty,
            "PRECONDITION: a client authenticated as a DIFFERENT key was served "
                + "\(wrongIdentity.servedEventIDs). Then merely authenticating is enough here, and "
                + "this scenario could not tell 'NMP authenticated as its own account' from "
                + "'NMP authenticated as anything at all'."
        )

        // --- Phase 1: NMP answers the challenge itself --------------------
        //
        // One app-owned policy, installed for exactly this account, and one
        // observation naming `authenticateAs: <account>` against exactly this relay.
        // Nothing else in the app does anything: no handshake code, no
        // kind:22242 anywhere in this file, no retry loop.

        let policy = RecordingAuthPolicy(mode: .allow)
        let registration = try engine.addAuthPolicy(expectedPublicKey: accountHex, policy: policy)

        let diagnostics = LatestDiagnostics()
        let diagnosticsStream = try engine.observeDiagnostics()
        let diagnosticsPump = Task {
            do {
                for try await snapshot in diagnosticsStream { diagnostics.store(snapshot) }
            } catch {}
        }
        defer {
            diagnosticsPump.cancel()
            diagnosticsStream.cancel()
        }

        let demand = NMPDemand(
            selection: NMPFilter(
                kinds: [UInt16(Lab.restrictedKind)], authors: .literal([seeder.pubkeyHex])
            ),
            routing: .explicit([lab.relay.url]),
            authenticateAs: accountHex
        )
        let query = try engine.observe(.single(demand))
        let ledger = ObservationLedger()
        let consumer = Task {
            do {
                for try await batch in query { await ledger.record(batch, relayURL: lab.relay.url) }
                await ledger.markEnded("sequence ended")
            } catch {
                await ledger.markEnded("threw: \(error)")
            }
        }
        defer { consumer.cancel() }

        // --- Control: the SAME query with authenticateAs: nil -------------
        //
        // Identical relay, identical filter, one field different. The unbound
        // session's REQ reaches the relay and the relay answers it with a
        // challenge and a CLOSED, which NMP surfaces as a failing source --
        // an unbound session declares no identity, so it has nothing to sign
        // a kind:22242 proof as. The identity-bound observation above
        // completes the handshake and is served the row.
        //
        // Two status histories, same relay, same filter: while #1889 was open
        // that pair localized the stall to the protected session's own send
        // path rather than to the relay, the filter, the account, the policy
        // installation, or this scenario's setup. It is kept because it is
        // what would localize a REGRESSION the same way. RECORDED, NOT
        // ASSERTED -- a diagnosis printed alongside a failure, not a contract
        // (C13's `wireSubCount` finding, same treatment).
        let control = try engine.observe(
            .single(
                NMPDemand(
                    selection: NMPFilter(
                        kinds: [UInt16(Lab.restrictedKind)], authors: .literal([seeder.pubkeyHex])
                    ),
                    routing: .explicit([lab.relay.url])
                )
            )
        )
        let controlLedger = ObservationLedger()
        let controlConsumer = Task {
            do {
                for try await batch in control {
                    await controlLedger.record(batch, relayURL: lab.relay.url)
                }
                await controlLedger.markEnded("sequence ended")
            } catch {
                await controlLedger.markEnded("threw: \(error)")
            }
        }
        defer { controlConsumer.cancel() }

        let sawFirst = await waitUntil(timeout: 45) {
            await ledger.current().latest.contains(first.id)
        }
        let afterFirst = await ledger.current()
        let controlState = await controlLedger.current()
        note(
            "CONTROL (authenticateAs: nil, same relay+filter): status=\(controlState.relayStatus) "
                + "history=\(controlState.statusHistory) rows=\(controlState.latest.count) "
                + "| SUBJECT (authenticateAs: <account>): status=\(afterFirst.relayStatus) "
                + "history=\(afterFirst.statusHistory)"
        )
        control.cancel()
        controlConsumer.cancel()
        note(
            "round trip: sawFirst=\(sawFirst) status=\(afterFirst.relayStatus) "
                + "statusHistory=\(afterFirst.statusHistory) access=\(afterFirst.accessHistory) "
                + "batches=\(afterFirst.batches)"
        )
        note(policy.describe())
        note(diagnostics.describeAuth())

        // The policy was consulted BEFORE anything was signed -- and it was
        // consulted about the right relay, for the right identity, with a
        // real challenge string. This is the fact the previous
        // controller-driven run could not touch at all.
        // SCOPED TO THE SUBJECT SESSION. Everything below asserts about the
        // session the demand bound to `accountHex`, never about "some
        // session NMP happened to negotiate". `policy.seen` alone cannot
        // make that distinction here -- see `subjectPolicyRequests`.
        //
        // The correlation needs the engine's AUTH row for the epoch as well
        // as the policy request, and those arrive on two independent
        // streams, so wait for the pair rather than racing it.
        _ = await waitUntil(timeout: 15) {
            !self.subjectPolicyRequests(
                boundTo: accountHex, policy: policy, diagnostics: diagnostics
            ).isEmpty
        }
        let subjectRequests = subjectPolicyRequests(
            boundTo: accountHex, policy: policy, diagnostics: diagnostics
        )
        XCTAssertFalse(
            subjectRequests.isEmpty,
            "NMP never consulted the installed AUTH policy for the IDENTITY-BOUND session. The "
                + "relay demands AUTH for this filter (proven above by an NMP-free client), the "
                + "demand declared `authenticateAs: \(accountHex)`, and a policy was installed for "
                + "that exact key. The identity-bound observation is \(afterFirst.relayStatus) with "
                + "history \(afterFirst.statusHistory), while the CONTROL unbound observation over "
                + "the SAME relay and filter is \(controlState.relayStatus) with history "
                + "\(controlState.statusHistory) -- so if the relay is answering the control, "
                + "the protected session is the one that stopped transmitting.\n"
                + "That is the #1889 bootstrap deadlock returning: a protected session's requests "
                + "parked until AUTH completes, while AUTH only starts on an INBOUND challenge and "
                + "strfry only challenges IN RESPONSE to a request. Neither side moves first.\n"
                + "(A policy request that could NOT be correlated to an AUTH row bound to "
                + "\(accountHex) is the control session's, and does not count.)\n"
                + policy.describe() + "\n" + diagnostics.describeAuth()
        )
        let firstRequest = try XCTUnwrap(subjectRequests.first)
        XCTAssertEqual(
            firstRequest.relay, lab.relay.url,
            "the policy was consulted about \(firstRequest.relay), not the relay the demand pinned"
        )
        XCTAssertEqual(
            firstRequest.expectedPublicKey, accountHex,
            "the policy was consulted for \(firstRequest.expectedPublicKey), not the account the "
                + "demand's `authenticateAs` named"
        )
        XCTAssertFalse(
            firstRequest.challenge.isEmpty,
            "the policy was handed an EMPTY challenge -- there is nothing for a kind:22242 proof to "
                + "be bound to, and strfry would reject it with `challenge string mismatch`"
        )
        // The challenge NMP was asked about is not one this test invented,
        // and not one of the controller's either: strfry mints a fresh one
        // per connection id, so all three connections above must differ.
        XCTAssertNotEqual(
            firstRequest.challenge, unauthenticated.challenge,
            "NMP's challenge is byte-identical to the one the unauthenticated probe received on a "
                + "DIFFERENT connection. strfry mints one per connection id, so this is not a fresh "
                + "challenge and the freshness property is not what this relay is doing."
        )
        XCTAssertNotEqual(
            firstRequest.challenge, wrongIdentity.challenge,
            "NMP's challenge is byte-identical to the wrong-identity probe's, on a different "
                + "connection again"
        )

        // The row itself. Everything above is negotiation; this is the
        // relay agreeing that the proof was valid, bound to that challenge,
        // for that relay URL, under that identity.
        XCTAssertTrue(
            sawFirst,
            "NMP never received \(first.id), which this relay serves ONLY to a connection "
                + "authenticated as \(accountHex). The AUTH round trip did not close. "
                + "status=\(afterFirst.relayStatus) history=\(afterFirst.statusHistory) "
                + "access=\(afterFirst.accessHistory) ended=\(afterFirst.ended ?? "no")\n"
                + policy.describe() + "\n" + diagnostics.describeAuth()
        )
        XCTAssertEqual(
            afterFirst.latest, [first.id],
            "the observation must hold exactly the one seeded restricted event"
        )
        // NMP names the identity it holds the session as, through the
        // query's own evidence. Without this the row could in principle have
        // arrived over an unbound session.
        XCTAssertTrue(
            afterFirst.accessHistory.contains("nip42(\(accountHex))"),
            "the source that served this row was never reported under `nip42(\(accountHex))` "
                + "identity: \(afterFirst.accessHistory)"
        )

        // And the engine's own AUTH diagnostics agree, with a signed event
        // id to point at. `phase == .ready` is documented as the sole owner
        // of "the relay's OK was correlated"; `authEventID` is the id of the
        // kind:22242 event NMP actually signed.
        let readyRows = diagnostics.allAuthRows().filter {
            $0.phase == .ready && $0.authenticateAs == accountHex
        }
        XCTAssertFalse(
            readyRows.isEmpty,
            "no AUTH session BOUND TO \(accountHex) ever reached `.ready` in "
                + "`observeDiagnostics()`, although the row arrived.\n" + diagnostics.describeAuth()
        )
        let ready = try XCTUnwrap(readyRows.first)
        XCTAssertEqual(ready.relay, lab.relay.url)
        XCTAssertTrue(ready.policyBound, "the ready AUTH session reports no bound policy")
        XCTAssertTrue(ready.signerBound, "the ready AUTH session reports no bound signer")
        XCTAssertNotNil(
            ready.authEventID,
            "the ready AUTH session names no signed kind:22242 event id"
        )

        // --- Phase 2: a reconnect must produce a NEW round trip -----------
        //
        // strfry mints a fresh `challengeGenerator` per connection id
        // (`RelayIngester.cpp`), so a reconnected NMP holding its old proof
        // is an unauthenticated NMP. This is the half that a single
        // happy-path handshake cannot show: an app that authenticated once
        // and never again would pass everything above.

        let challengesBeforeOutage = policy.distinctChallenges
        try await lab.relay.kill()
        let portDead = await waitUntil(timeout: 10) { await !lab.relay.isReachable() }
        XCTAssertTrue(
            portDead,
            "PRECONDITION: \(lab.relay.url) still accepted a TCP connection after SIGKILL -- there "
                + "was no reconnect for a fresh challenge to be minted on."
        )

        // A second event, written while the app's relay is provably dead,
        // by a sidecar process over the SAME LMDB directory on its own port
        // (C13's mechanism). The app has never been told this port exists,
        // so it cannot have been served this event before the reconnect --
        // which is what makes "it came back and re-authenticated" different
        // from "it still had the first row".
        let second = try NostrSigning.sign(
            keyPair: seeder, kind: Lab.restrictedKind,
            tags: [["p", accountHex]], content: "C15 restricted note, written during the outage"
        )
        let sidecar = try await RelayHandle(
            name: "c15a-sidecar", workDir: lab.sidecarDir, binaryPath: binaryPath,
            dataDir: lab.relay.dataDir
        )
        try sidecar.overrideConfig(lab.config(port: sidecar.port))
        try await sidecar.start()
        let seeded = try await sidecar.seed(second)
        try await sidecar.kill()
        let stillDead = await !lab.relay.isReachable()
        let sawSecondEarly = await ledger.current().everSeen.contains(second.id)
        XCTAssertTrue(seeded, "the sidecar relay did not accept the outage-window event")
        XCTAssertTrue(
            stillDead,
            "PRECONDITION: \(lab.relay.url) became reachable while the sidecar was writing"
        )
        XCTAssertFalse(
            sawSecondEarly,
            "PRECONDITION: NMP already held \(second.id) before the reconnect"
        )

        try await lab.relay.restart()
        let relayBack = await lab.relay.isReachable(timeout: 5)
        XCTAssertTrue(relayBack, "the relay did not come back up on \(lab.relay.url)")

        let sawSecond = await waitUntil(timeout: 60) {
            await ledger.current().latest.contains(second.id)
        }
        let afterReconnect = await ledger.current()
        let challengesAfter = policy.distinctChallenges
        note(
            "reconnect: relayBack=\(relayBack) sawSecond=\(sawSecond) "
                + "challenges \(challengesBeforeOutage.count) -> \(challengesAfter.count) "
                + "status=\(afterReconnect.relayStatus) history=\(afterReconnect.statusHistory) "
                + "access=\(afterReconnect.accessHistory)"
        )
        note(policy.describe())
        note(diagnostics.describeAuth())

        XCTAssertGreaterThan(
            challengesAfter.count, challengesBeforeOutage.count,
            "after the relay restarted, NMP consulted the AUTH policy about no NEW challenge "
                + "(distinct challenges: \(challengesBeforeOutage.count) before, "
                + "\(challengesAfter.count) after). strfry mints a fresh challenge per connection, "
                + "so re-using the pre-outage proof cannot authenticate the new connection.\n"
                + policy.describe()
        )
        XCTAssertTrue(
            sawSecond,
            "60s after the relay came back with \(second.id) durably in its store, NMP holds "
                + "\(afterReconnect.latest). This relay serves that event only to a connection "
                + "authenticated as \(accountHex), so the re-AUTH did not close. "
                + "status=\(afterReconnect.relayStatus) history=\(afterReconnect.statusHistory) "
                + "ended=\(afterReconnect.ended ?? "no")\n"
                + policy.describe() + "\n" + diagnostics.describeAuth()
        )
        XCTAssertEqual(
            afterReconnect.latest, [first.id, second.id],
            "after the reconnect the observation must hold exactly the two seeded ids"
        )
        // The two AUTH sessions are genuinely distinct sessions, not one row
        // re-reported: strfry's per-connection challenge means the engine's
        // own challenge descriptors must differ too.
        let descriptors = Set(diagnostics.allAuthRows().compactMap(\.challengeDescriptor))
        XCTAssertGreaterThanOrEqual(
            descriptors.count, 2,
            "the engine reported \(descriptors.count) distinct challenge descriptor(s) across the "
                + "reconnect. Two connections, two challenges, two descriptors.\n"
                + diagnostics.describeAuth()
        )

        XCTAssertTrue(
            policy.errors.isEmpty,
            "the app's policy failed to resolve a completion: \(policy.errors)"
        )
        _ = try engine.removeAuthPolicy(registration)
        consumer.cancel()
        try await lab.relay.kill()
    }

    // MARK: - Scenario B: the app says no, and recovers

    func testNMPSurfacesAPolicyDenialAndRecoversOnAFreshSession() async throws {
        let binaryPath = try Self.locateStrfryBinary()
        let lab = try await Lab(name: "c15b", binaryPath: binaryPath)
        defer { lab.cleanUp() }

        var log: [String] = []
        func note(_ line: String) { log.append(line) }
        defer { print((["", "C15-B phase log:"] + log).joined(separator: "\n")) }

        let engine = try NMPEngine(config: NMPConfig(storePath: lab.storePath, appRelays: []))
        defer { engine.shutdown() }
        let account = try engine.session.add(privateKey: NMPPrivateKey.generate(), makeCurrent: true)
        let accountHex = Self.hex(account.publicKey.bytes)

        let seeder = try NostrKeyPair()
        let secret = try NostrSigning.sign(
            keyPair: seeder, kind: Lab.restrictedKind,
            tags: [["p", accountHex]], content: "C15 restricted note behind a denied policy"
        )
        try await lab.relay.seed(secret)

        // The same NMP-free qualification as scenario A, in short: this
        // relay refuses this filter without AUTH. Without it, "the app
        // denied and no rows arrived" is exactly what an EMPTY relay looks
        // like, and the denial would be proving nothing.
        let wireFilter: [String: Any] = ["kinds": [Lab.restrictedKind], "authors": [seeder.pubkeyHex]]
        let unauthenticated = try await lab.relay.probeRead(filter: wireFilter)
        note(
            "qualification: challenge=\(unauthenticated.challenge ?? "nil") "
                + "closed=\(unauthenticated.closedMessage ?? "nil") "
                + "served=\(unauthenticated.servedEventIDs)"
        )
        XCTAssertNotNil(
            unauthenticated.challenge,
            "PRECONDITION: no AUTH challenge for this filter. Frames: \(unauthenticated.frames)"
        )
        XCTAssertTrue(
            unauthenticated.servedEventIDs.isEmpty,
            "PRECONDITION: the relay served \(unauthenticated.servedEventIDs) unauthenticated"
        )
        // ... and it IS there, so "no rows" below is a denial and not an
        // empty relay. Proven the only NMP-free way available: a client
        // that completes the handshake as the involved pubkey. This test
        // does not have NMP's private key, so it re-seeds the same content
        // under the SEEDER's key and reads it back as the seeder -- strfry
        // serves a restricted-kind row to its own author too
        // (`shouldSendToSubscriber`).
        let authorProbe = try await lab.relay.probeRead(filter: wireFilter, authenticateAs: seeder)
        note(
            "presence: authorProbe authAccepted=\(authorProbe.authAccepted.map(String.init) ?? "nil") "
                + "served=\(authorProbe.servedEventIDs)"
        )
        XCTAssertTrue(
            authorProbe.servedEventIDs.contains(secret.id),
            "PRECONDITION: an authenticated client that IS the event's author was served "
                + "\(authorProbe.servedEventIDs), not \(secret.id). The row is not actually "
                + "retrievable, so NMP receiving nothing below would say nothing about the denial. "
                + "Frames: \(authorProbe.frames)"
        )

        // --- The app refuses ----------------------------------------------

        let policy = RecordingAuthPolicy(mode: .deny(reason: "C15: the app declined this relay"))
        let registration = try engine.addAuthPolicy(expectedPublicKey: accountHex, policy: policy)

        let diagnostics = LatestDiagnostics()
        let diagnosticsStream = try engine.observeDiagnostics()
        let diagnosticsPump = Task {
            do {
                for try await snapshot in diagnosticsStream { diagnostics.store(snapshot) }
            } catch {}
        }
        defer {
            diagnosticsPump.cancel()
            diagnosticsStream.cancel()
        }

        let demand = NMPDemand(
            selection: NMPFilter(
                kinds: [UInt16(Lab.restrictedKind)], authors: .literal([seeder.pubkeyHex])
            ),
            routing: .explicit([lab.relay.url]),
            authenticateAs: accountHex
        )
        let query = try engine.observe(.single(demand))
        let ledger = ObservationLedger()
        let consumer = Task {
            do {
                for try await batch in query { await ledger.record(batch, relayURL: lab.relay.url) }
                await ledger.markEnded("sequence ended")
            } catch {
                await ledger.markEnded("threw: \(error)")
            }
        }
        defer { consumer.cancel() }

        let reportedDenied = await waitUntil(timeout: 45) {
            await ledger.current().statusHistory.contains("authDenied")
        }
        let denied = await ledger.current()
        note(
            "denied: reported=\(reportedDenied) status=\(denied.relayStatus) "
                + "history=\(denied.statusHistory) rows=\(denied.latest.count) "
                + "batches=\(denied.batches)"
        )
        note(policy.describe())
        note(diagnostics.describeAuth())

        XCTAssertFalse(
            policy.seen.isEmpty,
            "NMP never consulted the policy, so there was nothing to deny -- the same bootstrap "
                + "deadlock #1889 closed, reached from the other direction. An app cannot refuse "
                + "a relay it was never asked about. Query "
                + "history=\(denied.statusHistory)\n" + policy.describe()
        )
        XCTAssertTrue(
            reportedDenied,
            "the app denied the AUTH request and the query never reported `authDenied` through its "
                + "own acquisition evidence. An app that refuses a relay must be able to SEE that "
                + "it refused it, or the refusal is indistinguishable from a broken relay. "
                + "history=\(denied.statusHistory)\n" + diagnostics.describeAuth()
        )
        XCTAssertTrue(
            denied.latest.isEmpty,
            "the app denied AUTH and NMP delivered \(denied.latest) anyway, from a relay that "
                + "serves this filter only to an authenticated involved pubkey"
        )
        let deniedRows = diagnostics.allAuthRows().filter { $0.phase == .denied }
        XCTAssertFalse(
            deniedRows.isEmpty,
            "no AUTH session reached `.denied` in the engine's own diagnostics.\n"
                + diagnostics.describeAuth()
        )

        // --- ... and then changes its mind --------------------------------
        //
        // A denial is a decision about ONE bounded AUTH session, not a
        // permanent verdict on the relay. A fresh connection mints a fresh
        // challenge, and an app whose policy now allows must be able to get
        // in without rebuilding the engine, reopening the query, or
        // restarting anything of its own.

        policy.setMode(.allow)
        let challengesBefore = policy.distinctChallenges
        try await lab.relay.kill()
        let portDead = await waitUntil(timeout: 10) { await !lab.relay.isReachable() }
        XCTAssertTrue(portDead, "PRECONDITION: the relay port stayed alive after SIGKILL")
        try await lab.relay.restart()
        let relayBack = await lab.relay.isReachable(timeout: 5)
        XCTAssertTrue(relayBack, "the relay did not come back up on \(lab.relay.url)")

        let recovered = await waitUntil(timeout: 60) {
            await ledger.current().latest.contains(secret.id)
        }
        let afterRecovery = await ledger.current()
        note(
            "recovered: \(recovered) status=\(afterRecovery.relayStatus) "
                + "history=\(afterRecovery.statusHistory) access=\(afterRecovery.accessHistory) "
                + "challenges \(challengesBefore.count) -> \(policy.distinctChallenges.count)"
        )
        note(policy.describe())
        note(diagnostics.describeAuth())

        XCTAssertGreaterThan(
            policy.distinctChallenges.count, challengesBefore.count,
            "after the denial and a reconnect, the policy was never consulted about a NEW "
                + "challenge, so a denial is terminal for the whole engine rather than for one "
                + "bounded AUTH session.\n" + policy.describe()
        )
        XCTAssertTrue(
            recovered,
            "the app's policy now allows and the relay has restarted, but NMP still holds "
                + "\(afterRecovery.latest) after 60s. A denial cannot be recovered from through "
                + "the public API. status=\(afterRecovery.relayStatus) "
                + "history=\(afterRecovery.statusHistory) ended=\(afterRecovery.ended ?? "no")\n"
                + policy.describe() + "\n" + diagnostics.describeAuth()
        )
        XCTAssertTrue(
            afterRecovery.accessHistory.contains("nip42(\(accountHex))"),
            "the recovered source was never reported under `nip42(\(accountHex))`: "
                + "\(afterRecovery.accessHistory)"
        )
        XCTAssertTrue(
            policy.errors.isEmpty,
            "the app's policy failed to resolve a completion: \(policy.errors)"
        )

        _ = try engine.removeAuthPolicy(registration)
        consumer.cancel()
        try await lab.relay.kill()
    }

    // MARK: - The lab, configured to demand AUTH

    /// One relay process configured for NIP-42, plus the scratch
    /// directories the scenario needs. `serviceUrl` is not optional
    /// decoration: strfry refuses to process any `AUTH` message at all
    /// without it (`ingesterProcessAuth`: "relay needs serviceUrl to be
    /// configured before AUTH can work"), and it validates the incoming
    /// proof's `relay` tag against it -- which is exactly the binding this
    /// scenario wants checked.
    private struct Lab {
        static let restrictedKind = 1

        let root: URL
        let relay: RelayHandle
        let sidecarDir: URL
        let storePath: String

        init(name: String, binaryPath: URL) async throws {
            root = FileManager.default.temporaryDirectory
                .appendingPathComponent("canary-\(name)-\(UUID().uuidString)")
            let relayDir = root.appendingPathComponent("relay")
            sidecarDir = root.appendingPathComponent("sidecar")
            let storeDir = root.appendingPathComponent("store")
            for dir in [relayDir, sidecarDir, storeDir] {
                try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            }
            storePath = storeDir.appendingPathComponent("nmp.redb").path
            relay = try await RelayHandle(
                name: "\(name)-relay", workDir: relayDir, binaryPath: binaryPath
            )
            try relay.overrideConfig(Self.config(dataDir: relay.dataDir, port: relay.port))
            try await relay.start()
        }

        /// The sidecar shares the relay's LMDB directory but listens on its
        /// own port, so it needs its own `serviceUrl`. It only ever writes
        /// (writes are not restricted), but a mismatched config would be a
        /// silent difference between the two processes.
        func config(port: UInt16) -> String {
            Self.config(dataDir: relay.dataDir, port: port)
        }

        static func config(dataDir: URL, port: UInt16) -> String {
            """
            db = "\(dataDir.path)/"
            relay {
                bind = "127.0.0.1"
                port = \(port)
                info {
                    name = "canary-c15-auth-relay"
                    description = "NIP-42 restricted-read lab relay"
                }
                # Every inbound frame, in the relay's own log. NOTHING in
                # this file reads it: no assertion, no oracle. It exists
                # because the first question anyone debugging a NIP-42
                # negotiation asks is "what did NMP actually send", and
                # while #1889 was open the answer was "nothing at all".
                # Set CANARY_KEEP_LOGS=1 to keep the work directory and
                # read it.
                logging {
                    dumpInAll = true
                }
                auth {
                    enabled = true
                    serviceUrl = "ws://127.0.0.1:\(port)"
                    restrictedReadKinds = "\(restrictedKind)"
                    restrictReadToInvolvedPubkey = true
                }
            }
            """
        }

        func cleanUp() {
            if ProcessInfo.processInfo.environment["CANARY_KEEP_LOGS"] != nil {
                print("C15 kept relay work dir at \(root.path)")
                return
            }
            try? FileManager.default.removeItem(at: root)
        }
    }

    private static func hex(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }

    /// `setup-strfry.sh`'s own default cache location. Same shape as every
    /// other scenario in this package: skip by name rather than crash.
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
