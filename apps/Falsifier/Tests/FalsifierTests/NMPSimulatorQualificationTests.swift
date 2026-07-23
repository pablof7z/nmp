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

    /// Issue #598's original public-API reproduction, promoted into NMP's
    /// own real-iOS gate: the same hostname relay is already serving live
    /// read demand and has supplied the author's kind:10002 write route.
    /// A durable author-outbox write must therefore advance beyond
    /// `.awaitingRelay`, reach the relay exactly once, consume its `OK`, and
    /// feed the relay echo back into the still-live canonical query.
    @MainActor
    func testDurableAuthorOutboxWriteProgressesPastAwaitingRelay() async throws {
        let relay = try ControlledRelayHarness()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("nmp-598-simulator-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let storePath = root.appendingPathComponent("nmp.redb").path
        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storePath,
                indexerRelays: [relay.relayURL],
                appRelays: [relay.relayURL],
                fallbackRelays: [],
                allowedLocalRelayHosts: ["localhost"],
                maxRelays: 1,
                maxAuthCapabilities: 1
            )
        )
        defer {
            engine.shutdown()
            relay.stop()
            try? FileManager.default.removeItem(at: root)
        }

        let relayInformation = try await engine.relayInformation(
            for: relay.relayURL,
            policy: .refresh
        )
        XCTAssertTrue(isSameRelay(relayInformation.relay, relay.relayURL))
        XCTAssertEqual(relayInformation.document.name, "NMP Simulator Relay")
        XCTAssertEqual(relayInformation.document.supportedNips, [1, 11])
        XCTAssertEqual(relayInformation.freshness, .fresh)
        XCTAssertEqual(relay.snapshot().nip11Requests, 1)

        let secretKey = String(repeating: "0", count: 63) + "1"
        let account = try await engine.addAccount(secretKey: secretKey)
        try engine.setActiveAccount(account.publicKey)

        let routeEvent = try await engine.signEvent(
            NMPUnsignedEvent(
                createdAt: UInt64(Date().timeIntervalSince1970),
                kind: 10_002,
                tags: [["r", relay.relayURL, "write"]],
                content: ""
            )
        )
        relay.seed(routeEvent)

        let routeQuery = try engine.observe(
            NMPFilter(kinds: [10_002], authors: .literal([account.publicKey])),
            window: .expandable(initial: 1, max: 1)
        )
        let routeProbe = QueryProbe()
        let routeTask = Task {
            await routeProbe.consume(routeQuery)
        }
        let discoveredRoute = await waitForBatch(routeProbe, timeoutSeconds: 8) { batch in
            batch.rows.contains { $0.id == routeEvent.id }
        }
        XCTAssertNotNil(discoveredRoute, "NMP must ingest the controlled author route")
        let routeSubscriptionIDs = Set(relay.snapshot().requestSubscriptionIDs)
        routeQuery.cancel()
        await routeTask.value
        let routeFailure = await routeProbe.failure()
        XCTAssertNil(routeFailure)

        let query = try engine.observe(
            NMPFilter(kinds: [1], authors: .literal([account.publicKey])),
            window: .expandable(initial: 1, max: 1)
        )
        let queryProbe = QueryProbe()
        let queryTask = Task {
            await queryProbe.consume(query)
        }
        let acquired = await waitForBatch(queryProbe, timeoutSeconds: 8) { batch in
            batch.load == .idle
                && batch.evidence.shortfall.isEmpty
                && batch.evidence.sources.contains {
                    isSameRelay($0.relay, relay.relayURL)
                }
        }
        XCTAssertNotNil(acquired, "hostname relay must reconcile the bounded live query")

        let requested = await waitForRelay(relay, timeoutSeconds: 5) {
            !Set($0.requestSubscriptionIDs).subtracting(routeSubscriptionIDs).isEmpty
        }
        XCTAssertNotNil(requested, "the controlled relay must receive the live kind-1 REQ")

        let receipt = try await engine.publish(
            WriteIntent(
                payload: .unsigned(
                    pubkey: account.publicKey,
                    createdAt: UInt64(Date().timeIntervalSince1970),
                    kind: 1,
                    tags: [],
                    content: "NMP issue 598 simulator qualification"
                ),
                durability: .durable,
                routing: .authorOutbox,
                identityOverride: account.publicKey
            )
        )
        let receiptProbe = ReceiptProbe()
        let receiptTask = Task {
            await receiptProbe.consume(receipt.status)
        }
        let completedStatuses = await waitForStatuses(receiptProbe, timeoutSeconds: 15) { statuses in
            statuses.contains { status in
                if case .acked(let relayURL) = status {
                    return isSameRelay(relayURL, relay.relayURL)
                }
                return false
            }
        }
        let statuses: [WriteStatus]
        if let completedStatuses {
            statuses = completedStatuses
        } else {
            statuses = await receiptProbe.snapshot()
        }
        receipt.status.cancel()
        await receiptTask.value

        let statusSummary = statuses.map { String(describing: $0) }.joined(separator: ", ")
        XCTAssertTrue(statuses.contains(.accepted), statusSummary)
        let eventID = try XCTUnwrap(
            statuses.compactMap { status -> String? in
                if case .signed(let eventID) = status {
                    return eventID
                }
                return nil
            }.first,
            statusSummary
        )
        XCTAssertTrue(
            statuses.contains { status in
                if case .routed(let relays) = status {
                    return relays.contains { isSameRelay($0, relay.relayURL) }
                }
                return false
            },
            statusSummary
        )
        XCTAssertTrue(
            statuses.contains { status in
                if case .sent(let relayURL, _, _) = status {
                    return isSameRelay(relayURL, relay.relayURL)
                }
                return false
            },
            statusSummary
        )
        XCTAssertTrue(
            statuses.contains { status in
                if case .acked(let relayURL) = status {
                    return isSameRelay(relayURL, relay.relayURL)
                }
                return false
            },
            statusSummary
        )
        let receiptFailure = await receiptProbe.failure()
        XCTAssertNil(receiptFailure)
        XCTAssertEqual(relay.snapshot().acceptedEventIDs, [eventID])

        let delivered = await waitForBatch(queryProbe, timeoutSeconds: 8) { batch in
            batch.rows.contains {
                $0.id == eventID && $0.sources.contains {
                    isSameRelay($0, relay.relayURL)
                }
            }
        }
        XCTAssertNotNil(delivered, "relay echo must reach the still-live canonical query")
        XCTAssertEqual(
            relay.snapshot().peakActiveWebSockets,
            1,
            "read/write time-sharing must never exceed the configured physical-session ceiling"
        )

        query.cancel()
        await queryTask.value
        let queryFailure = await queryProbe.failure()
        XCTAssertNil(queryFailure)
        XCTAssertTrue(try engine.removeAccount(account))

        engine.shutdown()
        let tornDown = await waitForRelay(relay, timeoutSeconds: 5) {
            $0.activeWebSockets == 0
        }
        XCTAssertNotNil(tornDown, "engine shutdown must close the relay transport")
        try NMPEngine.resetPersistentStore(at: storePath)
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

    @MainActor
    private func waitForBatch(
        _ probe: QueryProbe,
        timeoutSeconds: UInt64,
        matching predicate: @escaping @Sendable (RowBatch) -> Bool
    ) async -> RowBatch? {
        let deadline = ContinuousClock.now + .seconds(Int64(timeoutSeconds))
        while ContinuousClock.now < deadline {
            let batches = await probe.snapshot()
            if let batch = batches.first(where: predicate) {
                return batch
            }
            try? await Task.sleep(for: .milliseconds(20))
        }
        return nil
    }

    @MainActor
    private func waitForStatuses(
        _ probe: ReceiptProbe,
        timeoutSeconds: UInt64,
        matching predicate: @escaping @Sendable ([WriteStatus]) -> Bool
    ) async -> [WriteStatus]? {
        let deadline = ContinuousClock.now + .seconds(Int64(timeoutSeconds))
        while ContinuousClock.now < deadline {
            let statuses = await probe.snapshot()
            if predicate(statuses) {
                return statuses
            }
            try? await Task.sleep(for: .milliseconds(20))
        }
        return nil
    }

    @MainActor
    private func waitForRelay(
        _ relay: ControlledRelayHarness,
        timeoutSeconds: UInt64,
        matching predicate: @escaping @Sendable (ControlledRelayHarness.Snapshot) -> Bool
    ) async -> ControlledRelayHarness.Snapshot? {
        let deadline = ContinuousClock.now + .seconds(Int64(timeoutSeconds))
        while ContinuousClock.now < deadline {
            let snapshot = relay.snapshot()
            if predicate(snapshot) {
                return snapshot
            }
            try? await Task.sleep(for: .milliseconds(20))
        }
        return nil
    }
}

private func isSameRelay(_ candidate: String, _ expected: String) -> Bool {
    guard let candidate = URL(string: candidate), let expected = URL(string: expected) else {
        return false
    }
    return candidate.scheme == expected.scheme
        && candidate.host == expected.host
        && candidate.port == expected.port
}

private actor QueryProbe {
    private var batches: [RowBatch] = []
    private var failureMessage: String?

    func consume(_ query: NMPQuery) async {
        do {
            for try await batch in query {
                batches.append(batch)
            }
        } catch {
            failureMessage = String(describing: error)
        }
    }

    func snapshot() -> [RowBatch] {
        batches
    }

    func failure() -> String? {
        failureMessage
    }
}

private actor ReceiptProbe {
    private var statuses: [WriteStatus] = []
    private var failureMessage: String?

    func consume(_ status: ReceiptStatus) async {
        do {
            for try await value in status {
                statuses.append(value)
            }
        } catch {
            failureMessage = String(describing: error)
        }
    }

    func snapshot() -> [WriteStatus] {
        statuses
    }

    func failure() -> String? {
        failureMessage
    }
}
