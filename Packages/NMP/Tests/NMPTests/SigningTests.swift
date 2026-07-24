import XCTest
@testable import NMP
import NMPFFI

final class SigningTests: XCTestCase {
    private let secret = String(repeating: "0", count: 63) + "1"
    private let author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    func testSignEventReturnsExactBodyWithoutPublishingIt() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let registration = try await engine.addAccount(secretKey: secret)
        XCTAssertEqual(registration.publicKey, author)
        try engine.setActiveAccount(author)
        let request = NMPUnsignedEvent(
            createdAt: 1_723_456_789,
            kind: 27_272,
            tags: [["t", "swift-sign-only"]],
            content: "exact swift body"
        )

        // #680: `signEvent` now awaits the one-shot `NmpSignEventHandle.signed()`
        // inside a `withTaskCancellationHandler`; the verified event is returned
        // directly, with no `SignEventObserver` bridge in between.
        let signed = try await engine.signEvent(request)
        XCTAssertEqual(signed.pubkey, author)
        XCTAssertEqual(signed.createdAt, request.createdAt)
        XCTAssertEqual(signed.kind, request.kind)
        XCTAssertEqual(signed.tags, request.tags)
        XCTAssertEqual(signed.content, request.content)
        XCTAssertEqual(signed.id.count, 64)
        XCTAssertEqual(signed.signature.count, 128)

        let query = try engine.observe(NMPFilter(kinds: [request.kind]))
        var iterator = query.makeAsyncIterator()
        let first = try await iterator.next()
        XCTAssertEqual(first?.rows, [], "sign-only must not publish or store the event")
        query.cancel()
    }

    func testSignEventWithoutActiveSignerIsTyped() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)
        do {
            _ = try await engine.signEvent(
                NMPUnsignedEvent(
                    createdAt: 1,
                    kind: 1,
                    tags: [],
                    content: "body"
                )
            )
            XCTFail("missing signer must fail")
        } catch {
            XCTAssertEqual(error as? NMPError, .noActiveSigner)
        }
    }

    /// #680: cancelling the awaiting task must reach Rust through
    /// `handle.cancel()` (Swift task cancellation never interrupts the await on
    /// its own). Even when the local signer would resolve near-instantly, the
    /// call must terminate with either the signed event or a `CancellationError`
    /// -- never hang.
    func testSignEventCancellationTerminatesTheAwait() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        _ = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(author)

        let task = Task {
            try await engine.signEvent(
                NMPUnsignedEvent(createdAt: 7, kind: 1, tags: [], content: "cancel me")
            )
        }
        task.cancel()

        do {
            let signed = try await task.value
            // Won the race before the cancel landed -- an exact signed event.
            XCTAssertEqual(signed.content, "cancel me")
        } catch is CancellationError {
            // Cancellation reached Rust and unwound the await. Also correct.
        }
    }

    /// #727: the accepted-operation failure taxonomy is a closed type distinct
    /// from synchronous `FfiError` start refusals. `.Cancelled` becomes a Swift
    /// `CancellationError`, and `.AlreadyConsumed` maps to
    /// `.signEventAlreadyConsumed`.
    func testSignEventFailureMappingKeepsEveryTypedAxis() {
        XCTAssertEqual(
            mapSignEventFailure(.SignerUnavailable(reason: "no signer")) as? NMPError,
            .signerUnavailable("no signer")
        )
        XCTAssertEqual(
            mapSignEventFailure(.SignerRejected(reason: "user said no")) as? NMPError,
            .signerRejected("user said no")
        )
        XCTAssertEqual(
            mapSignEventFailure(.InvalidSignerOutput(reason: "bad sig")) as? NMPError,
            .invalidSignerOutput("bad sig")
        )
        XCTAssertTrue(mapSignEventFailure(.Cancelled) is CancellationError)
        XCTAssertEqual(
            mapSignEventFailure(.AlreadyConsumed) as? NMPError,
            .signEventAlreadyConsumed
        )
    }
}
