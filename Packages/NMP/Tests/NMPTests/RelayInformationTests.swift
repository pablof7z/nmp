import Foundation
@preconcurrency import Network
import XCTest
@testable import NMP

private final class LocalNIP11Server: @unchecked Sendable {
    private let listener: NWListener
    private let queue = DispatchQueue(label: "nmp.swift.nip11.fixture")
    private let accepted = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private let body: Data
    private let responseDelay: DispatchTimeInterval
    private let responseGate: DispatchSemaphore?
    private var responseTime: UInt64?

    private(set) var relayURL = ""

    init(
        body: String,
        responseDelay: DispatchTimeInterval = .milliseconds(0),
        gated: Bool = false
    ) throws {
        listener = try NWListener(using: .tcp, on: .any)
        self.body = Data(body.utf8)
        self.responseDelay = responseDelay
        responseGate = gated ? DispatchSemaphore(value: 0) : nil

        let ready = DispatchSemaphore(value: 0)
        listener.stateUpdateHandler = { state in
            if case .ready = state {
                ready.signal()
            }
        }
        listener.newConnectionHandler = { [weak self] connection in
            self?.serve(connection, received: Data())
        }
        listener.start(queue: queue)
        guard ready.wait(timeout: .now() + 2) == .success, let port = listener.port else {
            listener.cancel()
            throw FixtureError.listenerDidNotStart
        }
        relayURL = "ws://localhost:\(port.rawValue)"
    }

    deinit {
        responseGate?.signal()
        listener.cancel()
    }

    func waitUntilAccepted() -> Bool {
        accepted.wait(timeout: .now() + 2) == .success
    }

    func respondedAt() -> UInt64? {
        lock.lock()
        defer { lock.unlock() }
        return responseTime
    }

    func releaseResponse() {
        responseGate?.signal()
    }

    private func serve(_ connection: NWConnection, received: Data) {
        connection.start(queue: queue)
        receiveHeaders(connection, received: received)
    }

    private func receiveHeaders(_ connection: NWConnection, received: Data) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, _, error in
            guard let self else { return }
            var received = received
            if let data {
                received.append(data)
            }
            if received.range(of: Data("\r\n\r\n".utf8)) == nil, error == nil {
                self.receiveHeaders(connection, received: received)
                return
            }

            self.accepted.signal()
            self.responseGate?.wait()
            self.queue.asyncAfter(deadline: .now() + self.responseDelay) {
                let headers = Data(
                    ("HTTP/1.1 200 OK\r\n" +
                        "Content-Type: application/nostr+json\r\n" +
                        "Content-Length: \(self.body.count)\r\n" +
                        "Connection: close\r\n\r\n").utf8
                )
                self.lock.lock()
                self.responseTime = DispatchTime.now().uptimeNanoseconds
                self.lock.unlock()
                connection.send(content: headers + self.body, completion: .contentProcessed { _ in
                    connection.cancel()
                })
            }
        }
    }

    private enum FixtureError: Error {
        case listenerDidNotStart
    }
}

final class RelayInformationTests: XCTestCase {
    @MainActor
    func testPublicAsyncCallSuspendsMainActorAndDeliversSuccess() async throws {
        let server = try LocalNIP11Server(
            body: #"{"name":"Local","supported_nips":[11,77],"limitation":{"max_limit":500,"auth_required":true}}"#,
            responseDelay: .milliseconds(500)
        )
        let engine = try NMPEngine(config: NMPConfig(allowedLocalRelayHosts: ["localhost"]))
        defer { engine.shutdown() }

        let request = Task { @MainActor in
            try await engine.relayInformation(for: server.relayURL, policy: .refresh)
        }
        await Task.yield()
        XCTAssertTrue(server.waitUntilAccepted(), "the generated async call must start HTTP")
        let mainActorProgress = DispatchTime.now().uptimeNanoseconds
        let value = try await request.value

        XCTAssertEqual(value.document.name, "Local")
        XCTAssertEqual(value.document.supportedNips, [11, 77])
        XCTAssertEqual(value.documentRevision.count, 64)
        XCTAssertEqual(value.document.limitation.maxLimit, 500)
        XCTAssertEqual(value.document.limitation.authRequired, true)
        guard let respondedAt = server.respondedAt() else {
            return XCTFail("fixture never sent its delayed response")
        }
        XCTAssertLessThan(
            mainActorProgress,
            respondedAt,
            "the MainActor must resume while Rust is still waiting for HTTP"
        )
    }

    func testPublicAsyncCallDeliversTypedAcquisitionError() async throws {
        let server = try LocalNIP11Server(body: "not-json")
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        do {
            _ = try await engine.relayInformation(for: server.relayURL, policy: .refresh)
            XCTFail("malformed NIP-11 must fail without an invented empty document")
        } catch let error as NMPError {
            guard case .relayInformationUnavailable = error else {
                return XCTFail("unexpected typed error: \(error)")
            }
        }
    }

    func testRelayInformationExecutorSaturationRemainsTypedThroughWrapper() async throws {
        let engine = try NMPEngine(config: NMPConfig(maxNativeTasks: 1))
        defer { engine.shutdown() }
        let held = try engine.observeDiagnostics()
        defer { held.cancel() }
        XCTAssertEqual(engine.nativeTaskCensus().admitted, 1)

        do {
            _ = try await engine.relayInformation(
                for: "ws://localhost:9",
                policy: .refresh
            )
            XCTFail("a full shared executor must refuse before HTTP")
        } catch let error as NMPError {
            XCTAssertEqual(
                error,
                .executorSaturated(component: "NIP-11 acquisition", capacity: 1)
            )
        }
    }

    func testRelayInformationWaiterSaturationRemainsTypedThroughWrapper() async throws {
        let server = try LocalNIP11Server(
            body: #"{"name":"Shared"}"#,
            gated: true
        )
        defer { server.releaseResponse() }
        let engine = try NMPEngine(config: NMPConfig(allowedLocalRelayHosts: ["localhost"]))
        defer { engine.shutdown() }

        enum Outcome: Sendable {
            case value
            case waiters(UInt64)
            case failure(String)
        }

        let outcomes = await withTaskGroup(of: Outcome.self) { group in
            for _ in 0..<65 {
                group.addTask {
                    do {
                        _ = try await engine.relayInformation(
                            for: server.relayURL,
                            policy: .refresh
                        )
                        return .value
                    } catch let error as NMPError {
                        if case .relayInformationWaitersSaturated(let capacity) = error {
                            return .waiters(capacity)
                        }
                        return .failure(String(describing: error))
                    } catch {
                        return .failure(String(describing: error))
                    }
                }
            }

            var outcomes: [Outcome] = []
            if let first = await group.next() {
                outcomes.append(first)
                server.releaseResponse()
            }
            for await outcome in group {
                outcomes.append(outcome)
            }
            return outcomes
        }

        XCTAssertEqual(outcomes.count, 65)
        XCTAssertEqual(outcomes.filter { if case .value = $0 { true } else { false } }.count, 64)
        XCTAssertEqual(
            outcomes.compactMap { if case .waiters(let capacity) = $0 { capacity } else { nil } },
            [UInt64(64)]
        )
        XCTAssertTrue(
            outcomes.allSatisfy { if case .failure = $0 { false } else { true } },
            "only the typed waiter refusal is allowed"
        )
    }
}
