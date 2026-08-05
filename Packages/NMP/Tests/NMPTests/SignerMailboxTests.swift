// #1284: the app-supplied signer mailbox as a Swift app actually holds it.
//
// The headline falsifier here is cancellation. Every other pull handle on
// this surface may be closed when its owner's task dies, because closing one
// ends one stream. This mailbox IS the app's registered signer, so the same
// reflex would silently park every later write for that key. And doing
// nothing is not an option either: UniFFI's generated Swift parks on
// `withUnsafeContinuation`, which task cancellation does not resume, so a
// cancelled drain leaves its Rust future alive holding the mailbox's
// single-reader claim -- the mailbox is then not merely idle but permanently
// unreadable. `next()` bridges cancellation to `unpark()`, and these tests
// require both halves: the drain ends, AND the signer still signs.

import XCTest
@testable import NMP

final class SignerMailboxTests: XCTestCase {
    /// The key the app signs for. NMP is given only this, never a secret.
    private let author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
    private let appSecret = String(repeating: "0", count: 63) + "1"

    /// A stand-in for the Secure Enclave / bunker / hardware device an app
    /// would really reach: a second engine that does hold the secret and can
    /// produce a real signature over exactly the requested body.
    private func appSideSignature(
        for body: NMPSignatureRequestBody,
        using signer: NMPEngine
    ) async throws -> NMPSignedEvent {
        try await signer.signEvent(
            NMPUnsignedEvent(
                createdAt: body.createdAt,
                kind: body.kind,
                tags: body.tags,
                content: body.content
            )
        )
    }

    private func appSideSigner() async throws -> NMPEngine {
        let engine = try NMPEngine(config: NMPConfig())
        _ = try await engine.addAccount(secretKey: appSecret)
        try engine.setActiveAccount(author)
        return engine
    }

    /// What #1238 exists for, through the Swift surface: an engine holding no
    /// secret for `author` gets a real signature from the app.
    func testAnAppSuppliedSignerSignsThroughItsMailbox() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let appSigner = try await appSideSigner()
        defer { appSigner.shutdown() }

        let mailbox = try engine.addSigner(publicKey: author)
        XCTAssertEqual(mailbox.publicKey, author)
        try engine.setActiveAccount(author)

        async let signed = engine.signEvent(
            NMPUnsignedEvent(createdAt: 1, kind: 1, tags: [], content: "signed by the app")
        )

        let delivered = try await mailbox.next()
        let request = try XCTUnwrap(delivered)
        XCTAssertEqual(request.body.pubkey, author, "the author is frozen in the request")
        XCTAssertEqual(request.body.content, "signed by the app")
        try request.resolve(await appSideSignature(for: request.body, using: appSigner))

        let result = try await signed
        XCTAssertEqual(result.pubkey, author)
        XCTAssertEqual(result.content, "signed by the app")
    }

    /// The defect this file was added for. A drain task is cancelled -- the
    /// 29er-next case is a `.task(id: engineGeneration)` whose id changes --
    /// and two things must hold: the drain must finish, and the mailbox must
    /// still be the app's signer afterwards.
    ///
    /// Run red against the bare `await` this replaced: the drain task never
    /// completes, so the first `XCTAssertEqual` times out; forcing past it,
    /// the replacement `next()` fails with `.concurrentNext` because the
    /// abandoned Rust future still holds the single-reader claim.
    func testCancellingADrainEndsItAndLeavesTheSignerWorking() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let appSigner = try await appSideSigner()
        defer { appSigner.shutdown() }

        let mailbox = try engine.addSigner(publicKey: author)
        try engine.setActiveAccount(author)

        // A drain parked on a mailbox with nothing in it yet.
        let drained = Task { () -> Bool in
            let request = try await mailbox.next()
            return request == nil
        }
        try await Task.sleep(for: .milliseconds(50))
        drained.cancel()

        let endedAtNil = try await withTimeout(seconds: 5) { try await drained.value }
        XCTAssertEqual(endedAtNil, true, "a cancelled drain ends its await at nil")

        // The signer survived the drain that walked away.
        async let signed = engine.signEvent(
            NMPUnsignedEvent(createdAt: 2, kind: 1, tags: [], content: "the next generation signs")
        )
        let delivered = try await withTimeout(seconds: 5) { try await mailbox.next() }
        let request = try XCTUnwrap(delivered)
        try request.resolve(await appSideSignature(for: request.body, using: appSigner))
        let result = try await signed
        XCTAssertEqual(result.content, "the next generation signs")
    }

    /// The other ordering, and the ordinary one for a `for try await` loop:
    /// the loop comes round when the task is ALREADY cancelled, so Swift runs
    /// the cancellation handler before the operation body ever parks. A wake
    /// that only worked against an already-parked reader would hang here --
    /// which is why `unpark` arms one await rather than poking a waker.
    func testCancellingBeforeTheAwaitStillEndsTheDrain() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let mailbox = try engine.addSigner(publicKey: author)

        let drained = Task { () -> Int in
            // Reach the drain loop only after cancellation has landed, so
            // the first `next()` is entered by an already-cancelled task.
            while !Task.isCancelled {
                await Task.yield()
            }
            var seen = 0
            for try await _ in mailbox.requests {
                seen += 1
            }
            return seen
        }
        drained.cancel()

        let seen = try await withTimeout(seconds: 5) { try await drained.value }
        XCTAssertEqual(seen, 0, "the cancelled sequence ends rather than hanging")
    }

    /// `cancel()` is the destructive verb, and it must stay distinguishable
    /// from the cancellation path above: after it, the mailbox is closed for
    /// good and writes park instead of being signed.
    func testCancellingTheMailboxIsTheDestructiveOne() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let mailbox = try engine.addSigner(publicKey: author)
        mailbox.cancel()

        let ended = try await withTimeout(seconds: 5) { try await mailbox.next() }
        XCTAssertNil(ended, "a closed, drained mailbox ends its stream")
        // And stays closed -- unlike an unparked one, which delivers again.
        let stillEnded = try await withTimeout(seconds: 5) { try await mailbox.next() }
        XCTAssertNil(stillEnded)
    }

    /// A refusal is the app's own terminal answer, not a timeout.
    func testAnAppRefusalReachesTheCaller() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let mailbox = try engine.addSigner(publicKey: author)
        try engine.setActiveAccount(author)

        async let signed = engine.signEvent(
            NMPUnsignedEvent(createdAt: 3, kind: 1, tags: [], content: "the user declines")
        )
        let delivered = try await mailbox.next()
        let request = try XCTUnwrap(delivered)
        try request.reject(.rejected(reason: "user declined"))

        do {
            _ = try await signed
            XCTFail("a declined signature must not succeed")
        } catch {
            XCTAssertEqual(error as? NMPError, .signerRejected("user declined"))
        }
    }

    /// Each request carries exactly one answer, across a boundary that cannot
    /// consume a value.
    func testARequestSettlesExactlyOnce() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let appSigner = try await appSideSigner()
        defer { appSigner.shutdown() }

        let mailbox = try engine.addSigner(publicKey: author)
        try engine.setActiveAccount(author)
        async let signed = engine.signEvent(
            NMPUnsignedEvent(createdAt: 4, kind: 1, tags: [], content: "settled once")
        )

        let delivered = try await mailbox.next()
        let request = try XCTUnwrap(delivered)
        try request.resolve(await appSideSignature(for: request.body, using: appSigner))
        let result = try await signed
        XCTAssertEqual(result.content, "settled once")

        do {
            try request.reject(.unavailable)
            XCTFail("a settled request has no second answer to give")
        } catch {
            XCTAssertEqual(error as? NMPSignatureSettleError, .alreadySettled)
        }
    }

    /// Removal is exact-instance: a superseded mailbox cannot detach the
    /// registration that replaced it.
    func testRemovalIsStaleSafe() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let first = try engine.addSigner(publicKey: author)
        let replacement = try engine.addSigner(publicKey: author)

        XCTAssertFalse(try engine.removeSigner(first), "a stale mailbox detaches nothing")
        XCTAssertTrue(try engine.removeSigner(replacement))
        XCTAssertFalse(try engine.removeSigner(replacement))
    }

    /// A bare `await` that never returns is a hang, and a hang inside XCTest
    /// is a 10-minute stall rather than a failure. Every await that this file
    /// requires to *finish* goes through here so the red is fast and legible.
    private func withTimeout<T: Sendable>(
        seconds: Double,
        _ work: @escaping @Sendable () async throws -> T
    ) async throws -> T {
        try await withThrowingTaskGroup(of: T.self) { group in
            group.addTask { try await work() }
            group.addTask {
                try await Task.sleep(for: .seconds(seconds))
                throw SignerMailboxTimeout()
            }
            let first = try await group.next()!
            group.cancelAll()
            return first
        }
    }
}

private struct SignerMailboxTimeout: Error {}
