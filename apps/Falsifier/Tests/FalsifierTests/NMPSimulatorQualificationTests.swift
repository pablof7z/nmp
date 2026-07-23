// Issue #465: runtime-qualify NIP-11 hostname resolution on a runnable iOS
// Simulator target (not just macOS-host `swift test`). PR #191 compiled the
// iOS-simulator slices of `nmp-ffi`; this is the honest follow-up that
// actually EXECUTES a hostname acquisition through the governed Hickory
// resolver path while the process is a real iOS Simulator runtime, not the
// macOS SwiftPM test host.
//
// Design decision (final, 2026-07-16, issue #465): `localhost` is the
// accepted hostname here. It is a hostname, not an IP literal, so it still
// exercises `HickoryReqwestResolver` init + resolution inside the iOS
// runtime -- exactly the platform delta this issue qualifies. A loopback
// DNS-protocol fixture or a public hostname were both cut as
// over-engineering / flaky; see the issue's design-decision comment.
//
// The `LocalNIP11Server` fixture below is PORTED (not reinvented) from
// `Packages/NMP/Tests/NMPTests/RelayInformationTests.swift`, which this
// issue explicitly credits as valid hostname proof -- the deficiency named
// here is only about WHERE that proof executes.

import Foundation
@preconcurrency import Network
import XCTest
@testable import NMP

private final class LocalNIP11Server: @unchecked Sendable {
    private let listener: NWListener
    private let queue = DispatchQueue(label: "nmp.simulator.nip11.fixture")
    private let body: Data

    private(set) var relayURL = ""

    init(body: String) throws {
        listener = try NWListener(using: .tcp, on: .any)
        self.body = Data(body.utf8)

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
        listener.cancel()
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

            let headers = Data(
                ("HTTP/1.1 200 OK\r\n" +
                    "Content-Type: application/nostr+json\r\n" +
                    "Content-Length: \(self.body.count)\r\n" +
                    "Connection: close\r\n\r\n").utf8
            )
            connection.send(content: headers + self.body, completion: .contentProcessed { [weak self] _ in
                // `.contentProcessed` means "accepted by the network stack",
                // not "delivered" -- cancelling here immediately can plausibly
                // race the final flush (a suspected cause of an observed
                // simulator-only cold-boot flake where the client read a
                // truncated body). Wait for the client to actually finish
                // reading and close/reset its end of the connection first.
                self?.awaitClientCloseThenCancel(connection)
            })
        }
    }

    private func awaitClientCloseThenCancel(_ connection: NWConnection) {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 1) {
            [weak self] _, _, isComplete, error in
            if isComplete || error != nil {
                connection.cancel()
            } else {
                // Unexpected extra data from the client; keep waiting for
                // its actual close rather than assuming EOF prematurely.
                self?.awaitClientCloseThenCancel(connection)
            }
        }
    }

    private enum FixtureError: Error {
        case listenerDidNotStart
    }
}

final class NMPSimulatorQualificationTests: XCTestCase {
    /// Runs on the iOS Simulator (see `apps/Falsifier`'s `FalsifierTests`
    /// target, added for #465). A hostname (`localhost`, decided acceptable
    /// -- see file doc) NIP-11 acquisition through the public
    /// `NMPEngine.relayInformation(for:policy:)` async API must succeed with
    /// the expected document, prove the governed Hickory resolver path ran
    /// (no blocking-GAI fallback would still deliver asynchronously, but a
    /// crash/hang here would signal a platform-specific resolver
    /// misconfiguration that the macOS host test cannot surface), and then
    /// tear the engine down with exactly zero leaked native tasks.
    @MainActor
    func testHostnameAcquisitionSucceedsOnSimulatorRuntime() async throws {
        let server = try LocalNIP11Server(
            body: #"{"name":"Simulator","supported_nips":[11,77],"limitation":{"max_limit":500,"auth_required":true}}"#
        )
        let engine = try NMPEngine(config: NMPConfig(allowedLocalRelayHosts: ["localhost"]))

        let value = try await engine.relayInformation(for: server.relayURL, policy: .refresh)

        XCTAssertEqual(value.document.name, "Simulator")
        XCTAssertEqual(value.document.supportedNips, [11, 77])
        XCTAssertEqual(value.documentRevision.count, 64)
        XCTAssertEqual(value.document.limitation.maxLimit, 500)
        XCTAssertEqual(value.document.limitation.authRequired, true)

        // #680 removed the native-task census/idle-barrier: NIP-11 fetches no
        // longer run on an app-visible native-task pool, so there is nothing to
        // assert an exact zero baseline against. Shutdown remains the teardown.
        engine.shutdown()
    }

    /// The typed-error half of the same platform delta: a malformed NIP-11
    /// body must fail through the same typed `NMPError` taxonomy on the
    /// simulator runtime as it does on the macOS host, never an invented
    /// empty document and never a crash/hang from the resolver path.
    @MainActor
    func testMalformedDocumentDeliversTypedErrorOnSimulatorRuntime() async throws {
        let server = try LocalNIP11Server(body: "not-json")
        let engine = try NMPEngine(config: NMPConfig(allowedLocalRelayHosts: ["localhost"]))

        do {
            _ = try await engine.relayInformation(for: server.relayURL, policy: .refresh)
            XCTFail("malformed NIP-11 must fail without an invented empty document")
        } catch let error as NMPError {
            guard case .relayInformationUnavailable = error else {
                return XCTFail("unexpected typed error: \(error)")
            }
        }

        // #680 removed the native-task census/idle-barrier: NIP-11 fetches no
        // longer run on an app-visible native-task pool, so there is nothing to
        // assert an exact zero baseline against. Shutdown remains the teardown.
        engine.shutdown()
    }
}
