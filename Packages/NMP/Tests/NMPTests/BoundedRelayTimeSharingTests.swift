import Foundation
@preconcurrency import Network
import XCTest
@testable import NMP

final class BoundedRelayTimeSharingTests: XCTestCase {
    /// Issue #598's original public-API reproduction, promoted into NMP's
    /// host Swift XCTest gate: one operator-provided app relay serves live reads
    /// and is also an honest Auto destination. A durable write must advance
    /// beyond `.awaitingRelay`, reach the relay exactly once, consume its
    /// `OK`, and feed the relay echo back into the still-live canonical
    /// query. No author route is learned implicitly by native core.
    @MainActor
    func testDurableAutoRoutedWriteProgressesPastAwaitingRelay() async throws {
        let relay = try ControlledRelayHarness()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("nmp-598-swift-host-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let storePath = root.appendingPathComponent("nmp.redb").path
        let engine = try NMPEngine(
            config: NMPConfig(
                storePath: storePath,
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
        XCTAssertEqual(relayInformation.document.name, "NMP Swift Test Relay")
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
                payload: .event(
                    kind: 1,
                    tags: [],
                    content: "NMP issue 598 Swift host qualification"
                ),
                durability: .durable,
                routing: .auto,
                identity: .explicit(pubkey: account.publicKey)
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
                if case .routed(let relays, _) = status {
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

    /// The qualification fixture itself follows NMP's edge-triggered
    /// contract: one timeout cancels and removes each exact continuation.
    /// A later mutation therefore cannot find or double-resume a stale
    /// waiter left behind by a timed-out test assertion.
    @MainActor
    func testEventDrivenWaitersWithdrawExactlyOnTimeout() async throws {
        let queryProbe = QueryProbe()
        let missingBatch: RowBatch? = await boundedWait(timeout: .milliseconds(50)) {
            await queryProbe.next { _ in false }
        }
        XCTAssertNil(missingBatch)
        let queryWaiters = await queryProbe.waiterCount()
        XCTAssertEqual(queryWaiters, 0)

        let receiptProbe = ReceiptProbe()
        let missingStatuses: [WriteStatus]? = await boundedWait(timeout: .milliseconds(50)) {
            await receiptProbe.next { _ in false }
        }
        XCTAssertNil(missingStatuses)
        let receiptWaiters = await receiptProbe.waiterCount()
        XCTAssertEqual(receiptWaiters, 0)

        let relay = try ControlledRelayHarness()
        defer { relay.stop() }
        let missingSnapshot: ControlledRelayHarness.Snapshot? = await boundedWait(
            timeout: .milliseconds(50)
        ) {
            await relay.nextSnapshot { _ in false }
        }
        XCTAssertNil(missingSnapshot)
        XCTAssertEqual(relay.snapshotWaiterCount(), 0)
    }

    @MainActor
    private func waitForBatch(
        _ probe: QueryProbe,
        timeoutSeconds: UInt64,
        matching predicate: @escaping @Sendable (RowBatch) -> Bool
    ) async -> RowBatch? {
        await boundedWait(timeout: .seconds(Int64(timeoutSeconds))) {
            await probe.next(matching: predicate)
        }
    }

    @MainActor
    private func waitForStatuses(
        _ probe: ReceiptProbe,
        timeoutSeconds: UInt64,
        matching predicate: @escaping @Sendable ([WriteStatus]) -> Bool
    ) async -> [WriteStatus]? {
        await boundedWait(timeout: .seconds(Int64(timeoutSeconds))) {
            await probe.next(matching: predicate)
        }
    }

    @MainActor
    private func waitForRelay(
        _ relay: ControlledRelayHarness,
        timeoutSeconds: UInt64,
        matching predicate: @escaping @Sendable (ControlledRelayHarness.Snapshot) -> Bool
    ) async -> ControlledRelayHarness.Snapshot? {
        await boundedWait(timeout: .seconds(Int64(timeoutSeconds))) {
            await relay.nextSnapshot(matching: predicate)
        }
    }

    /// Race one edge-triggered waiter against one real timeout task. The
    /// losing child is cancelled before this scope returns; each probe's
    /// cancellation handler removes and resumes its exact registered waiter.
    @MainActor
    private func boundedWait<Value: Sendable>(
        timeout: Duration,
        operation: @escaping @Sendable () async -> Value?
    ) async -> Value? {
        await withTaskGroup(of: Value?.self) { group in
            group.addTask {
                await operation()
            }
            group.addTask {
                do {
                    try await Task.sleep(for: timeout)
                    return nil
                } catch {
                    return nil
                }
            }
            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
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
    private struct Waiter {
        let predicate: @Sendable (RowBatch) -> Bool
        let continuation: CheckedContinuation<RowBatch?, Never>
    }

    private var batches: [RowBatch] = []
    private var failureMessage: String?
    private var waiters: [UUID: Waiter] = [:]

    func consume(_ query: NMPQuery) async {
        do {
            for try await batch in query {
                batches.append(batch)
                let matching = waiters.compactMap { id, waiter in
                    waiter.predicate(batch) ? id : nil
                }
                for id in matching {
                    waiters.removeValue(forKey: id)?.continuation.resume(
                        returning: batch
                    )
                }
            }
        } catch {
            failureMessage = String(describing: error)
        }
        finishWaiters()
    }

    func next(
        matching predicate: @escaping @Sendable (RowBatch) -> Bool
    ) async -> RowBatch? {
        if let existing = batches.first(where: predicate) {
            return existing
        }

        let id = UUID()
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                if Task.isCancelled {
                    continuation.resume(returning: nil)
                } else if let existing = batches.first(where: predicate) {
                    continuation.resume(returning: existing)
                } else {
                    waiters[id] = Waiter(
                        predicate: predicate,
                        continuation: continuation
                    )
                }
            }
        } onCancel: {
            Task {
                await self.cancelWaiter(id)
            }
        }
    }

    func snapshot() -> [RowBatch] {
        batches
    }

    func failure() -> String? {
        failureMessage
    }

    func waiterCount() -> Int {
        waiters.count
    }

    private func cancelWaiter(_ id: UUID) {
        waiters.removeValue(forKey: id)?.continuation.resume(returning: nil)
    }

    private func finishWaiters() {
        let pending = Array(waiters.values)
        waiters.removeAll()
        pending.forEach { $0.continuation.resume(returning: nil) }
    }
}

private actor ReceiptProbe {
    private struct Waiter {
        let predicate: @Sendable ([WriteStatus]) -> Bool
        let continuation: CheckedContinuation<[WriteStatus]?, Never>
    }

    private var statuses: [WriteStatus] = []
    private var failureMessage: String?
    private var waiters: [UUID: Waiter] = [:]

    func consume(_ status: ReceiptStatus) async {
        do {
            for try await value in status {
                statuses.append(value)
                let snapshot = statuses
                let matching = waiters.compactMap { id, waiter in
                    waiter.predicate(snapshot) ? id : nil
                }
                for id in matching {
                    waiters.removeValue(forKey: id)?.continuation.resume(
                        returning: snapshot
                    )
                }
            }
        } catch {
            failureMessage = String(describing: error)
        }
        finishWaiters()
    }

    func next(
        matching predicate: @escaping @Sendable ([WriteStatus]) -> Bool
    ) async -> [WriteStatus]? {
        if predicate(statuses) {
            return statuses
        }

        let id = UUID()
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                if Task.isCancelled {
                    continuation.resume(returning: nil)
                } else if predicate(statuses) {
                    continuation.resume(returning: statuses)
                } else {
                    waiters[id] = Waiter(
                        predicate: predicate,
                        continuation: continuation
                    )
                }
            }
        } onCancel: {
            Task {
                await self.cancelWaiter(id)
            }
        }
    }

    func snapshot() -> [WriteStatus] {
        statuses
    }

    func failure() -> String? {
        failureMessage
    }

    func waiterCount() -> Int {
        waiters.count
    }

    private func cancelWaiter(_ id: UUID) {
        waiters.removeValue(forKey: id)?.continuation.resume(returning: nil)
    }

    private func finishWaiters() {
        let pending = Array(waiters.values)
        waiters.removeAll()
        pending.forEach { $0.continuation.resume(returning: nil) }
    }
}
