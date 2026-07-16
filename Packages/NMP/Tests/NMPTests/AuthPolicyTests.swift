import XCTest
@testable import NMP
import NMPFFI

final class AuthPolicyTests: XCTestCase {
    private let publicKey = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
    private let secretKey = String(repeating: "0", count: 63) + "1"

    private final class FakeCompletion: FfiAuthPolicyCompletion, @unchecked Sendable {
        var outcomes: [FfiAuthPolicyOutcome] = []
        var error: FfiAuthPolicyCompletionError?

        init(error: FfiAuthPolicyCompletionError? = nil) {
            self.error = error
            super.init(noPointer: .init())
        }

        required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
            super.init(unsafeFromRawPointer: pointer)
        }

        override func resolve(outcome: FfiAuthPolicyOutcome) throws {
            if let error { throw error }
            outcomes.append(outcome)
        }
    }

    private final class RecordingPolicy: NMPAuthPolicy, @unchecked Sendable {
        enum Mode {
            case inline
            case pending
            case unavailable
        }

        let mode: Mode
        var requests: [NMPAuthPolicyRequest] = []
        var retainedCompletion: NMPAuthPolicyCompletion?
        var resolutionError: Error?

        init(mode: Mode) {
            self.mode = mode
        }

        func evaluate(
            request: NMPAuthPolicyRequest,
            completion: NMPAuthPolicyCompletion
        ) {
            requests.append(request)
            switch mode {
            case .inline:
                do {
                    try completion.resolve(.allow)
                } catch {
                    resolutionError = error
                }
            case .pending:
                retainedCompletion = completion
            case .unavailable:
                break
            }
        }
    }

    private final class ReentrantCancellationPolicy: NMPAuthPolicy, @unchecked Sendable {
        let engine: NMPEngine
        var completion: NMPAuthPolicyCompletion?
        var activeAccountDuringCancellation: String?
        var reentrySucceeded = false
        var terminalError: NMPAuthPolicyCompletionError?
        var cancellations = 0

        init(engine: NMPEngine) {
            self.engine = engine
        }

        func evaluate(
            request: NMPAuthPolicyRequest,
            completion: NMPAuthPolicyCompletion
        ) {
            self.completion = completion
        }

        func onCancelled(request: NMPAuthPolicyRequest) {
            cancellations += 1
            do {
                activeAccountDuringCancellation = try engine.activeAccount()
                reentrySucceeded = true
            } catch {
                XCTFail("AUTH cancellation could not reenter the engine: \(error)")
            }
            do {
                try completion?.resolve(.allow)
            } catch let error as NMPAuthPolicyCompletionError {
                terminalError = error
            } catch {
                XCTFail("unexpected cancellation resolution error: \(error)")
            }
        }
    }

    func testCallbackProjectsEveryRequestFieldAndInlineCompletion() {
        let policy = RecordingPolicy(mode: .inline)
        let bridge = AuthPolicyBridge(policy: policy)
        let completion = FakeCompletion()

        bridge.evaluate(request: ffiRequest(), completion: completion)

        XCTAssertEqual(policy.requests, [nativeRequest()])
        XCTAssertEqual(completion.outcomes, [.allow])
        XCTAssertNil(policy.resolutionError)
    }

    func testCallbackRetainsOnlyAnExplicitPendingCompletion() throws {
        let policy = RecordingPolicy(mode: .pending)
        let bridge = AuthPolicyBridge(policy: policy)
        weak var weakCompletion: FakeCompletion?

        do {
            let completion = FakeCompletion()
            weakCompletion = completion
            bridge.evaluate(request: ffiRequest(), completion: completion)
        }

        XCTAssertNotNil(weakCompletion)
        try policy.retainedCompletion?.resolve(.deny(reason: "not for this relay"))
        XCTAssertEqual(weakCompletion?.outcomes, [.deny(reason: "not for this relay")])
        policy.retainedCompletion = nil
        XCTAssertNil(weakCompletion)
    }

    func testReturningWithoutResolvingOrRetainingIsUnavailableByConstruction() {
        let policy = RecordingPolicy(mode: .unavailable)
        let bridge = AuthPolicyBridge(policy: policy)
        weak var weakCompletion: FakeCompletion?

        do {
            let completion = FakeCompletion()
            weakCompletion = completion
            bridge.evaluate(request: ffiRequest(), completion: completion)
            XCTAssertEqual(completion.outcomes, [])
        }

        XCTAssertNil(policy.retainedCompletion)
        XCTAssertNil(weakCompletion)
    }

    func testCompletionPreservesEveryExactTerminalError() {
        let cases: [(FfiAuthPolicyCompletionError, NMPAuthPolicyCompletionError)] = [
            (.AlreadyCompleted, .alreadyCompleted),
            (.Cancelled, .cancelled),
            (.ReceiverGone, .receiverGone),
        ]

        for (ffiError, expected) in cases {
            let completion = NMPAuthPolicyCompletion(FakeCompletion(error: ffiError))
            XCTAssertThrowsError(try completion.resolve(.unavailable)) { error in
                XCTAssertEqual(error as? NMPAuthPolicyCompletionError, expected)
            }
        }
    }

    func testCancellationCallbackCanReenterEngineAndSeesCancelledTerminalOwner() throws {
        let engine = try NMPEngine(config: .init())
        defer { engine.shutdown() }
        let policy = ReentrantCancellationPolicy(engine: engine)
        let bridge = AuthPolicyBridge(policy: policy)
        let completion = FakeCompletion(error: .Cancelled)

        bridge.evaluate(request: ffiRequest(), completion: completion)
        bridge.onCancelled(request: ffiRequest())

        XCTAssertEqual(policy.cancellations, 1)
        XCTAssertTrue(policy.reentrySucceeded)
        XCTAssertNil(policy.activeAccountDuringCancellation)
        XCTAssertEqual(policy.terminalError, .cancelled)
    }

    func testPolicyRegistrationRemovalIsStaleSafeAndRepeatable() throws {
        let engine = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 1))
        defer { engine.shutdown() }
        let first = try engine.addAuthPolicy(
            expectedPublicKey: publicKey,
            policy: RecordingPolicy(mode: .inline)
        )
        let replacement = try engine.addAuthPolicy(
            expectedPublicKey: publicKey,
            policy: RecordingPolicy(mode: .pending)
        )

        XCTAssertEqual(first.publicKey, publicKey)
        XCTAssertEqual(replacement.publicKey, publicKey)
        XCTAssertFalse(try engine.removeAuthPolicy(first))
        XCTAssertTrue(try engine.removeAuthPolicy(replacement))
        XCTAssertFalse(try engine.removeAuthPolicy(replacement))
    }

    func testAuthCapacityAndZeroCapacityAreExactTypedBoundaries() async throws {
        let zero = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 0))
        defer { zero.shutdown() }
        do {
            _ = try await zero.addAccount(secretKey: secretKey)
            XCTFail("zero capacity must refuse the account")
        } catch {
            XCTAssertEqual(error as? NMPError, .authCapabilityRegistryFull(limit: 0))
        }
        XCTAssertThrowsError(
            try zero.addAuthPolicy(
                expectedPublicKey: publicKey,
                policy: RecordingPolicy(mode: .inline)
            )
        ) { error in
            XCTAssertEqual(error as? NMPError, .authCapabilityRegistryFull(limit: 0))
        }

        let one = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 1))
        defer { one.shutdown() }
        let account = try await one.addAccount(secretKey: secretKey)
        XCTAssertThrowsError(
            try one.addAuthPolicy(
                expectedPublicKey: publicKey,
                policy: RecordingPolicy(mode: .inline)
            )
        ) { error in
            XCTAssertEqual(error as? NMPError, .authCapabilityRegistryFull(limit: 1))
        }
        XCTAssertTrue(try one.removeAccount(account))
        let policy = try one.addAuthPolicy(
            expectedPublicKey: publicKey,
            policy: RecordingPolicy(mode: .inline)
        )
        XCTAssertTrue(try one.removeAuthPolicy(policy))
    }

    func testAuthCapabilityInstanceExhaustionMapsWithoutCollapse() {
        XCTAssertEqual(
            NMPError(.AuthCapabilityInstanceExhausted),
            .authCapabilityInstanceExhausted
        )
    }

    func testAuthDiagnosticsProjectsEveryExactFieldAndPhase() {
        let phases: [(FfiAuthPhase, AuthPhase)] = [
            (.awaitingChallenge, .awaitingChallenge),
            (.awaitingPolicy, .awaitingPolicy),
            (.awaitingSignature, .awaitingSignature),
            (.awaitingRelayAck, .awaitingRelayAck),
            (.ready, .ready),
            (.denied, .denied),
            (.error, .error),
        ]

        for (ffiPhase, expectedPhase) in phases {
            let projected = AuthDiagnostics(
                FfiAuthDiagnostics(
                    relay: "wss://relay.example/",
                    access: .nip42(publicKey: publicKey),
                    transportGeneration: 17,
                    epochSequence: 23,
                    challengeDescriptor: "blake3:challenge",
                    phase: ffiPhase,
                    policyBound: true,
                    signerBound: false,
                    authEventId: String(repeating: "b", count: 64),
                    sendHandoffAccepted: true,
                    relayOkAccepted: false
                )
            )

            XCTAssertEqual(projected.relay, "wss://relay.example/")
            XCTAssertEqual(projected.access, .nip42(publicKey: publicKey))
            XCTAssertEqual(projected.transportGeneration, 17)
            XCTAssertEqual(projected.epochSequence, 23)
            XCTAssertEqual(projected.challengeDescriptor, "blake3:challenge")
            XCTAssertEqual(projected.phase, expectedPhase)
            XCTAssertTrue(projected.policyBound)
            XCTAssertFalse(projected.signerBound)
            XCTAssertEqual(projected.authEventID, String(repeating: "b", count: 64))
            XCTAssertTrue(projected.sendHandoffAccepted)
            XCTAssertFalse(projected.relayOKAccepted)
        }
    }

    private func ffiRequest() -> FfiAuthPolicyRequest {
        FfiAuthPolicyRequest(
            expectedPublicKey: publicKey,
            relay: "wss://relay.example/",
            challenge: " exact challenge ",
            transportGeneration: 17,
            epochSequence: 23
        )
    }

    private func nativeRequest() -> NMPAuthPolicyRequest {
        NMPAuthPolicyRequest(
            expectedPublicKey: publicKey,
            relay: "wss://relay.example/",
            challenge: " exact challenge ",
            transportGeneration: 17,
            epochSequence: 23
        )
    }
}
