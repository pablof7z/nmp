import XCTest
@testable import NMP
import NMPFFI

final class SigningTests: XCTestCase {
    private let secret = String(repeating: "0", count: 63) + "1"
    private let author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    func testSignEventReturnsExactBodyWithoutPublishingIt() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        XCTAssertEqual(try await engine.addAccount(secretKey: secret), author)
        try engine.setActiveAccount(author)
        let request = NMPUnsignedEvent(
            createdAt: 1_723_456_789,
            kind: 27_272,
            tags: [["t", "swift-sign-only"]],
            content: "exact swift body"
        )

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
        let first = await iterator.next()
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

    func testSignEventCompletionBeforeCancellationHandleInstallIsSafe() async throws {
        let request = NMPUnsignedEvent(
            createdAt: 9,
            kind: 1,
            tags: [],
            content: "immediate"
        )
        var cancellationCalls = 0
        let signed = try await performSignEvent(request) { _, observer in
            observer.onSigned(
                event: FfiSignedEvent(
                    id: String(repeating: "a", count: 64),
                    pubkey: author,
                    createdAt: request.createdAt,
                    kind: request.kind,
                    tags: request.tags,
                    content: request.content,
                    sig: String(repeating: "b", count: 128)
                )
            )
            return { cancellationCalls += 1 }
        }

        XCTAssertEqual(signed.content, request.content)
        XCTAssertEqual(cancellationCalls, 0)
    }
}
