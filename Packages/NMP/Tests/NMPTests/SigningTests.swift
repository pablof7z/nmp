import XCTest
@testable import NMP
import NMPFFI

final class SigningTests: XCTestCase {
    private final class LockedCounter: @unchecked Sendable {
        private let lock = NSLock()
        private var storedValue = 0

        var value: Int {
            lock.withLock { storedValue }
        }

        func increment() {
            lock.withLock { storedValue += 1 }
        }
    }

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

    func testCancellationAndCallbackBeforeHandleInstallCancelExactlyOnce() async throws {
        let request = NMPUnsignedEvent(
            createdAt: 10,
            kind: 1,
            tags: [],
            content: "cancel before handle"
        )
        let startEntered = expectation(description: "native start entered")
        let callbackFinished = expectation(description: "callback finished")
        let allowCallback = DispatchSemaphore(value: 0)
        let allowHandleReturn = DispatchSemaphore(value: 0)
        let cancellationCalls = LockedCounter()
        let callbackEvent = signedEvent(for: request)

        let task = Task {
            try await performSignEvent(request) { _, observer in
                DispatchQueue.global().async {
                    allowCallback.wait()
                    observer.onSigned(event: callbackEvent)
                    callbackFinished.fulfill()
                }
                startEntered.fulfill()
                allowHandleReturn.wait()
                return { cancellationCalls.increment() }
            }
        }

        await fulfillment(of: [startEntered], timeout: 2)
        task.cancel()
        allowCallback.signal()
        await fulfillment(of: [callbackFinished], timeout: 2)
        allowHandleReturn.signal()

        do {
            _ = try await task.value
            XCTFail("cancellation must own the terminal result")
        } catch is CancellationError {
            // Expected.
        }
        XCTAssertEqual(cancellationCalls.value, 1)
    }

    func testSimultaneousCancellationAndCompletionHaveOneTerminalOwner() async throws {
        let request = NMPUnsignedEvent(
            createdAt: 11,
            kind: 1,
            tags: [],
            content: "terminal race"
        )
        let callbackEvent = signedEvent(for: request)

        for _ in 0..<100 {
            let bridge = SignEventBridge()
            let cancellationCalls = LockedCounter()
            let gate = DispatchSemaphore(value: 0)
            let workers = DispatchGroup()

            let result: Result<NMPSignedEvent, Error>
            do {
                let signed = try await withCheckedThrowingContinuation { continuation in
                    XCTAssertTrue(bridge.installContinuation(continuation))
                    bridge.installCancellation { cancellationCalls.increment() }
                    workers.enter()
                    DispatchQueue.global().async {
                        gate.wait()
                        bridge.requestCancellation()
                        workers.leave()
                    }
                    workers.enter()
                    DispatchQueue.global().async {
                        gate.wait()
                        bridge.onSigned(event: callbackEvent)
                        workers.leave()
                    }
                    gate.signal()
                    gate.signal()
                    workers.wait()
                }
                result = .success(signed)
            } catch {
                result = .failure(error)
            }

            switch result {
            case .success(let signed):
                XCTAssertEqual(signed.content, request.content)
                XCTAssertEqual(cancellationCalls.value, 0)
            case .failure(let error):
                XCTAssertTrue(error is CancellationError)
                XCTAssertEqual(cancellationCalls.value, 1)
            }
        }
    }

    private func signedEvent(for request: NMPUnsignedEvent) -> FfiSignedEvent {
        FfiSignedEvent(
            id: String(repeating: "a", count: 64),
            pubkey: author,
            createdAt: request.createdAt,
            kind: request.kind,
            tags: request.tags,
            content: request.content,
            sig: String(repeating: "b", count: 128)
        )
    }
}
