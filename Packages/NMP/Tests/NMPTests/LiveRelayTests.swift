// The bounded LIVE test proves the whole Swift -> NMPFFI ->
// nmp-engine -> real-relay path end to end, using ONLY the public `NMP`
// surface (no raw websocket code in this file). Every network wait is
// bounded (~15s) so this can never hang a CI run.
//
// This core native package deliberately assembles no author-route provider.
// These live checks therefore configure explicit operator app policy and
// prove that ordinary and derived demands still work without an implicit
// discovery query or a protocol-specific router lane.
import Foundation
import XCTest
@testable import NMP
import NMPFFI

final class LiveRelayTests: XCTestCase {
    /// fiatjaf -- a known, always-active npub, used only as a read target.
    /// No secret key is used anywhere in this test: `setActiveAccount` may
    /// re-root reads onto an account this process holds no key for (read-
    /// only browsing is legal; see `NMPEngine.setActiveAccount`'s doc).
    static let fiatjafHex = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
    static let operatorRelays = ["wss://purplepag.es", "wss://relay.primal.net"]

    /// Construct the engine with two operator app relays, add a read-only
    /// account for fiatjaf, and observe the reactive follow-feed (kind:1
    /// authored by whoever his kind:3 currently names). Both the inner and
    /// projected demands use only the explicit neutral operator policy.
    func testFollowFeedUsesOperatorAppRelays() async throws {
        let engine = try NMPEngine(config: NMPConfig(appRelays: Self.operatorRelays))
        defer { engine.shutdown() }

        try engine.setActiveAccount(Self.fiatjafHex)

        let followFeed = NMPFilter(
            kinds: [1],
            authors: .derived(
                inner: NMPDemand(
                    selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                    source: .authorOutboxes
                ),
                project: .tag("p")
            ),
            limit: 50
        )
        let query = try engine.observe(followFeed)
        let rows = await Self.firstNonEmptyBatch(from: query, timeoutSeconds: 30)
        query.cancel()

        guard let rows else {
            throw XCTSkip(
                "Observed no follow-feed rows within 30s from \(Self.operatorRelays) -- "
                    + "the operator relays may be unreachable from this test environment. "
                    + "Package build + construction tests still pass independently of this "
                    + "network condition."
            )
        }

        XCTAssertGreaterThan(rows.count, 0, "expected at least one real note")
        for row in rows.prefix(5) {
            XCTAssertEqual(row.kind, 1)
            XCTAssertFalse(row.id.isEmpty)
        }
    }

    /// The diagnostic surface (M5), proven live: while the follow feed's row
    /// iterator still owns its native demand, a concurrently active
    /// diagnostics iterator must observe a CURRENT snapshot whose
    /// `eventsByKind` reports a REAL received kind:1 count > 0.
    ///
    /// Diagnostics is latest-wins current-plan state, not a historical event
    /// ledger. The harness therefore records the causal snapshot while both
    /// iterators are alive; it never assumes a later snapshot must retain a
    /// relay session that has since left the plan.
    func testDiagnosticsSnapshotShowsRealEventsByKindForTheFollowFeed() async throws {
        let engine = try NMPEngine(config: NMPConfig(appRelays: Self.operatorRelays))
        defer { engine.shutdown() }

        try engine.setActiveAccount(Self.fiatjafHex)

        let followFeed = NMPFilter(
            kinds: [1],
            authors: .derived(
                inner: NMPDemand(
                    selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                    source: .authorOutboxes
                ),
                project: .tag("p")
            ),
            limit: 50
        )
        // Open diagnostics before the row observation. `NMPQuery` demand is
        // iterator-owned (#680): returning from a one-shot `for await` loop
        // cancels the native handle even if the `NMPQuery` value remains in
        // scope. The shared harness below keeps both iterators pulling until
        // it has observed the causal pair.
        let diagnostics = try engine.observeDiagnostics()
        let outcome = try await Self.observeRowsAndCausalDiagnostics(
            diagnostics: diagnostics,
            openQuery: { try engine.observe(followFeed) },
            rowsTimeoutSeconds: 30,
            diagnosticsTimeoutSeconds: 10
        )

        guard outcome.diagnosticsStarted else {
            return XCTFail(
                "the pre-opened diagnostics iterator ended or failed before its first current snapshot"
            )
        }

        guard let rows = outcome.rows else {
            throw XCTSkip(
                "Observed no follow-feed rows within 30s from \(Self.operatorRelays) -- "
                    + "diagnostics has nothing real to report in this test environment."
            )
        }

        guard outcome.observedCausalKind1 else {
            return XCTFail(
                "expected the concurrently active diagnostics iterator to observe a current-plan "
                    + "snapshot reporting the real kind:1 event received for the follow feed"
            )
        }

        XCTAssertGreaterThan(rows.count, 0, "expected at least one real note")
    }

    /// Deterministic #1052 falsifier for the exact iterator-owned race.
    ///
    /// OLD shape: consume one row and return from the row iterator, retain the
    /// `NMPQuery` value, then open diagnostics. Iterator `deinit` has already
    /// cancelled native demand, so the fake latest-wins mailbox replaces the
    /// causal kind:1 snapshot with the post-unsubscribe current plan.
    ///
    /// Correct shape: pre-open diagnostics and keep BOTH iterators alive. The
    /// fake does not release the causal snapshot until the row iterator has
    /// entered its second parked `next()`, so a one-shot row consumer cannot
    /// make this half pass accidentally.
    func testDiagnosticsHarnessKeepsBothIteratorsAliveThroughTheCausalSnapshot() async throws {
        let failedStartState = DiagnosticsRaceState(endDiagnosticsOnOpen: true)
        let failedStartEngine = DiagnosticsRaceEngine(state: failedStartState)
        let endingDiagnostics = try NMPDiagnostics(engine: failedStartEngine)
        let failedStart = try await Self.observeRowsAndCausalDiagnostics(
            diagnostics: endingDiagnostics,
            openQuery: {
                try NMPQuery(
                    engine: failedStartEngine,
                    filter: NMPFilter(kinds: [1]).toFfi(),
                    window: nil
                )
            },
            rowsTimeoutSeconds: 1,
            diagnosticsTimeoutSeconds: 1
        )
        XCTAssertFalse(
            failedStart.diagnosticsStarted,
            "diagnostics ending before its first snapshot is not a no-row environmental skip"
        )
        XCTAssertNil(failedStart.rows)

        let oldState = DiagnosticsRaceState()
        let oldEngine = DiagnosticsRaceEngine(state: oldState)
        let retainedQuery = try NMPQuery(
            engine: oldEngine,
            filter: NMPFilter(kinds: [1]).toFfi(),
            window: nil
        )
        let oldRows = await Self.firstNonEmptyBatch(from: retainedQuery, timeoutSeconds: 1)
        XCTAssertNotNil(oldRows, "the deterministic row must arrive")

        let lateDiagnostics = try NMPDiagnostics(engine: oldEngine)
        let lateSnapshot = await Self.firstSnapshot(from: lateDiagnostics, timeoutSeconds: 1)
        XCTAssertNotNil(lateSnapshot, "late diagnostics still receives the replacement snapshot")
        XCTAssertFalse(
            lateSnapshot.map(Self.hasReceivedKind1) ?? true,
            "a late observer cannot recover the overwritten causal snapshot"
        )
        XCTAssertEqual(
            oldState.queryCancellationCount,
            1,
            "row-iterator deinit cancels demand even while the NMPQuery value is retained"
        )
        XCTAssertTrue(oldState.cancelledBeforeCausalDelivery)
        lateDiagnostics.cancel()
        retainedQuery.cancel() // retained through the late diagnostics sample; idempotent.

        let correctedState = DiagnosticsRaceState()
        let correctedEngine = DiagnosticsRaceEngine(state: correctedState)
        let preopenedDiagnostics = try NMPDiagnostics(engine: correctedEngine)
        let corrected = try await Self.observeRowsAndCausalDiagnostics(
            diagnostics: preopenedDiagnostics,
            openQuery: {
                try NMPQuery(
                    engine: correctedEngine,
                    filter: NMPFilter(kinds: [1]).toFfi(),
                    window: nil
                )
            },
            rowsTimeoutSeconds: 1,
            diagnosticsTimeoutSeconds: 1
        )

        XCTAssertNotNil(corrected.rows)
        XCTAssertTrue(corrected.diagnosticsStarted)
        XCTAssertTrue(corrected.observedCausalKind1)
        XCTAssertTrue(
            correctedState.queryReachedParkedSecondPull,
            "the row iterator must remain alive after its first non-empty batch"
        )
        XCTAssertFalse(
            correctedState.cancelledBeforeCausalDelivery,
            "query teardown may happen only after the causal diagnostics snapshot was consumed"
        )
    }

    /// The same operator-policy proof for a literal author set (no derived
    /// binding involved at all): fiatjaf's own kind:1 notes.
    func testAuthorsOwnNotesArriveThroughOperatorAppRelays() async throws {
        let engine = try NMPEngine(config: NMPConfig(appRelays: Self.operatorRelays))
        defer { engine.shutdown() }

        let notesFilter = NMPFilter(kinds: [1], authors: .literal([Self.fiatjafHex]), limit: 20)
        let query = try engine.observe(notesFilter)
        let rows = await Self.firstNonEmptyBatch(from: query, timeoutSeconds: 30)
        query.cancel()

        guard let rows else {
            throw XCTSkip(
                "Observed no kind:1 notes for fiatjaf within 30s from \(Self.operatorRelays) "
                    + "-- the operator relays may be unreachable from this test environment."
            )
        }

        XCTAssertGreaterThan(rows.count, 0, "expected at least one real note")
        for row in rows.prefix(5) {
            XCTAssertEqual(row.kind, 1)
            XCTAssertEqual(row.pubkey, Self.fiatjafHex)
            XCTAssertFalse(row.id.isEmpty)
            XCTAssertFalse(row.content.isEmpty)
        }
    }

    /// Races the query's first non-empty snapshot against a hard timeout so
    /// this test can never hang, regardless of what the live network does.
    private static func firstNonEmptyBatch(from query: NMPQuery, timeoutSeconds: UInt64) async -> [Row]? {
        await withTaskGroup(of: [Row]?.self) { group in
            group.addTask {
                do {
                    for try await batch in query {
                        if !batch.rows.isEmpty {
                            return batch.rows
                        }
                    }
                } catch {
                    return nil
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                return nil
            }

            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }

    private struct CausalDiagnosticsOutcome: Sendable {
        let diagnosticsStarted: Bool
        let rows: [Row]?
        let observedCausalKind1: Bool
    }

    /// Start diagnostics first, then keep the row and diagnostics iterators
    /// alive together until both causal facts have been observed. The
    /// coordinator retains only what THIS test observed; it does not change
    /// or reinterpret the engine's current-plan/latest-wins contract.
    private static func observeRowsAndCausalDiagnostics(
        diagnostics: NMPDiagnostics,
        openQuery: () throws -> NMPQuery,
        rowsTimeoutSeconds: UInt64,
        diagnosticsTimeoutSeconds: UInt64
    ) async throws -> CausalDiagnosticsOutcome {
        let coordinator = CausalDiagnosticsCoordinator()
        let (diagnosticsStarted, diagnosticsStartedContinuation) =
            AsyncStream<Bool>.makeStream(bufferingPolicy: .bufferingNewest(1))

        let diagnosticsTask = Task {
            do {
                var iterator = diagnostics.makeAsyncIterator()
                guard let first = try await iterator.next() else {
                    diagnosticsStartedContinuation.yield(false)
                    diagnosticsStartedContinuation.finish()
                    return
                }
                await coordinator.observe(first)
                diagnosticsStartedContinuation.yield(true)
                diagnosticsStartedContinuation.finish()

                while let snapshot = try await iterator.next() {
                    await coordinator.observe(snapshot)
                }
            } catch {
                diagnosticsStartedContinuation.yield(false)
                diagnosticsStartedContinuation.finish()
            }
        }

        guard await firstValue(from: diagnosticsStarted, timeoutSeconds: 5) == true else {
            diagnosticsTask.cancel()
            diagnostics.cancel()
            await diagnosticsTask.value
            return CausalDiagnosticsOutcome(
                diagnosticsStarted: false,
                rows: nil,
                observedCausalKind1: false
            )
        }

        let query: NMPQuery
        do {
            query = try openQuery()
        } catch {
            diagnosticsTask.cancel()
            diagnostics.cancel()
            await diagnosticsTask.value
            throw error
        }

        let queryTask = Task {
            do {
                var reportedRows = false
                for try await batch in query {
                    if !reportedRows, !batch.rows.isEmpty {
                        reportedRows = true
                        await coordinator.observe(batch.rows)
                    }
                }
            } catch {
                // The owning test reports missing evidence through its bounded
                // waits; cancellation/teardown ends collection cleanly.
            }
        }

        let rowsSignals = await coordinator.rowsSignals()
        let proofSignals = await coordinator.proofSignals()
        let rows = await firstValue(from: rowsSignals, timeoutSeconds: rowsTimeoutSeconds)
        let observedCausalKind1: Bool
        if rows == nil {
            observedCausalKind1 = false
        } else {
            observedCausalKind1 =
                await firstValue(
                    from: proofSignals,
                    timeoutSeconds: diagnosticsTimeoutSeconds
                ) != nil
        }

        queryTask.cancel()
        diagnosticsTask.cancel()
        query.cancel()
        diagnostics.cancel()
        await queryTask.value
        await diagnosticsTask.value

        return CausalDiagnosticsOutcome(
            diagnosticsStarted: true,
            rows: rows,
            observedCausalKind1: observedCausalKind1
        )
    }

    private static func firstSnapshot(
        from diagnostics: NMPDiagnostics, timeoutSeconds: UInt64
    ) async -> DiagnosticsSnapshot? {
        await withTaskGroup(of: DiagnosticsSnapshot?.self) { group in
            group.addTask {
                do {
                    for try await snapshot in diagnostics {
                        return snapshot
                    }
                } catch {
                    return nil
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                return nil
            }

            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }

    private static func firstValue<Value: Sendable>(
        from stream: AsyncStream<Value>, timeoutSeconds: UInt64
    ) async -> Value? {
        await withTaskGroup(of: Value?.self) { group in
            group.addTask {
                var iterator = stream.makeAsyncIterator()
                return await iterator.next()
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                return nil
            }

            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }

    private static func hasReceivedKind1(_ snapshot: DiagnosticsSnapshot) -> Bool {
        snapshot.relays.contains { relay in
            relay.eventsByKind.contains { $0.kind == 1 && $0.count > 0 }
        }
    }
}

/// Test-harness memory for two facts observed by two independently pulling
/// iterators. A later current-plan snapshot without kind:1 does not erase the
/// fact that this harness already observed the causal snapshot; that memory is
/// local to the test and never changes product diagnostics semantics.
private actor CausalDiagnosticsCoordinator {
    private enum ProofDelivery {
        case waiting
        case finished
    }

    private var rows: [Row]?
    private var sawReceivedKind1 = false
    private var proofDelivery = ProofDelivery.waiting
    private let rowsStream: AsyncStream<[Row]>
    private let rowsContinuation: AsyncStream<[Row]>.Continuation
    private let proofStream: AsyncStream<Void>
    private let proofContinuation: AsyncStream<Void>.Continuation

    init() {
        let rowsChannel = AsyncStream<[Row]>.makeStream(bufferingPolicy: .bufferingNewest(1))
        rowsStream = rowsChannel.stream
        rowsContinuation = rowsChannel.continuation

        let proofChannel = AsyncStream<Void>.makeStream(bufferingPolicy: .bufferingNewest(1))
        proofStream = proofChannel.stream
        proofContinuation = proofChannel.continuation
    }

    func observe(_ snapshot: DiagnosticsSnapshot) {
        if snapshot.relays.contains(where: { relay in
            relay.eventsByKind.contains { $0.kind == 1 && $0.count > 0 }
        }) {
            sawReceivedKind1 = true
        }
        finishProofIfReady()
    }

    func observe(_ observedRows: [Row]) {
        guard rows == nil else { return }
        rows = observedRows
        rowsContinuation.yield(observedRows)
        rowsContinuation.finish()
        finishProofIfReady()
    }

    func rowsSignals() -> AsyncStream<[Row]> {
        rowsStream
    }

    func proofSignals() -> AsyncStream<Void> {
        proofStream
    }

    private func finishProofIfReady() {
        guard rows != nil, sawReceivedKind1 else { return }
        guard case .waiting = proofDelivery else { return }
        proofDelivery = .finished
        proofContinuation.yield(())
        proofContinuation.finish()
    }
}

/// Deterministic latest-wins oracle for #1052. It models only the Swift
/// observation boundary involved in the bug:
///
/// 1. the first row pull emits a causal kind:1 diagnostics snapshot before
///    returning the row;
/// 2. dropping that row iterator cancels demand and replaces the pending
///    diagnostics value with the post-unsubscribe current plan;
/// 3. the causal diagnostics value is released only after the row iterator
///    has entered a second parked pull, making iterator lifetime load-bearing.
private final class DiagnosticsRaceState: @unchecked Sendable {
    private enum QueryLifecycle: Equatable {
        case absent
        case active
        case cancelled
    }

    private enum DiagnosticsLifecycle: Equatable {
        case absent
        case active
        case cancelled
    }

    private let lock = NSLock()

    private var queryLifecycle = QueryLifecycle.absent
    private var rowDelivered = false
    private var diagnosticsLifecycle = DiagnosticsLifecycle.absent
    private var pendingDiagnostics: FfiDiagnosticsSnapshot?
    private var diagnosticsVersion: UInt64 = 0
    private var diagnosticsWaiter: CheckedContinuation<Void, Never>?
    private var rowWaiter: CheckedContinuation<Void, Never>?
    private var causalDelivered = false
    private var cancellationCount = 0
    private var cancelledBeforeCausal = false
    private var reachedParkedSecondPull = false
    private let endDiagnosticsOnOpen: Bool

    init(endDiagnosticsOnOpen: Bool = false) {
        self.endDiagnosticsOnOpen = endDiagnosticsOnOpen
    }

    var queryCancellationCount: Int {
        locked { cancellationCount }
    }

    var cancelledBeforeCausalDelivery: Bool {
        locked { cancelledBeforeCausal }
    }

    var queryReachedParkedSecondPull: Bool {
        locked { reachedParkedSecondPull }
    }

    func openQuery() {
        lock.lock()
        queryLifecycle = .active
        lock.unlock()
    }

    func openDiagnostics() {
        let waiter: CheckedContinuation<Void, Never>?
        lock.lock()
        if endDiagnosticsOnOpen {
            diagnosticsLifecycle = .cancelled
            pendingDiagnostics = nil
            diagnosticsVersion &+= 1
            waiter = diagnosticsWaiter
            diagnosticsWaiter = nil
            lock.unlock()
            waiter?.resume()
            return
        }
        diagnosticsLifecycle = .active
        pendingDiagnostics = currentSnapshotLocked()
        diagnosticsVersion &+= 1
        waiter = diagnosticsWaiter
        diagnosticsWaiter = nil
        lock.unlock()
        waiter?.resume()
    }

    func nextRow() async -> FfiFrame? {
        let firstPull = locked {
            () -> (active: Bool, frame: FfiFrame?, wake: CheckedContinuation<Void, Never>?) in
            guard case .active = queryLifecycle else {
                return (false, nil, nil)
            }
            guard !rowDelivered else {
                return (true, nil, nil)
            }
            rowDelivered = true
            guard case .active = diagnosticsLifecycle else {
                return (true, Self.rowFrame, nil)
            }
            pendingDiagnostics = Self.snapshot(hasReceivedKind1: true)
            diagnosticsVersion &+= 1
            let wake = diagnosticsWaiter
            diagnosticsWaiter = nil
            return (true, Self.rowFrame, wake)
        }
        guard firstPull.active else {
            return nil
        }
        if let frame = firstPull.frame {
            firstPull.wake?.resume()
            return frame
        }

        await withCheckedContinuation { continuation in
            let registration = locked {
                () -> (resumeRow: Bool, wake: CheckedContinuation<Void, Never>?) in
                guard case .active = queryLifecycle else {
                    return (true, nil)
                }
                reachedParkedSecondPull = true
                rowWaiter = continuation
                diagnosticsVersion &+= 1
                let wake = diagnosticsWaiter
                diagnosticsWaiter = nil
                return (false, wake)
            }
            registration.wake?.resume()
            if registration.resumeRow {
                continuation.resume()
            }
        }
        return nil
    }

    func nextDiagnostics() async -> FfiDiagnosticsSnapshot? {
        while true {
            let step = locked {
                () -> (finished: Bool, snapshot: FfiDiagnosticsSnapshot?, version: UInt64) in
                if case .cancelled = diagnosticsLifecycle {
                    return (true, nil, diagnosticsVersion)
                }
                if let snapshot = pendingDiagnostics {
                    let isCausal = Self.hasReceivedKind1(snapshot)
                    if !isCausal || reachedParkedSecondPull || queryLifecycle != .active {
                        pendingDiagnostics = nil
                        if isCausal {
                            causalDelivered = true
                        }
                        return (false, snapshot, diagnosticsVersion)
                    }
                }
                return (false, nil, diagnosticsVersion)
            }
            if step.finished {
                return nil
            }
            if let snapshot = step.snapshot {
                return snapshot
            }

            await withCheckedContinuation { continuation in
                let resumeImmediately = locked {
                    if diagnosticsLifecycle == .cancelled
                        || diagnosticsVersion != step.version
                    {
                        return true
                    }
                    diagnosticsWaiter = continuation
                    return false
                }
                if resumeImmediately {
                    continuation.resume()
                }
            }
        }
    }

    func cancelQuery() {
        let rowWake: CheckedContinuation<Void, Never>?
        let diagnosticsWake: CheckedContinuation<Void, Never>?
        lock.lock()
        guard case .active = queryLifecycle else {
            lock.unlock()
            return
        }
        queryLifecycle = .cancelled
        cancellationCount += 1
        if !causalDelivered {
            cancelledBeforeCausal = true
        }
        rowWake = rowWaiter
        rowWaiter = nil
        if case .active = diagnosticsLifecycle {
            pendingDiagnostics = Self.snapshot(hasReceivedKind1: false)
            diagnosticsVersion &+= 1
            diagnosticsWake = diagnosticsWaiter
            diagnosticsWaiter = nil
        } else {
            diagnosticsWake = nil
        }
        lock.unlock()
        rowWake?.resume()
        diagnosticsWake?.resume()
    }

    func cancelDiagnostics() {
        let waiter: CheckedContinuation<Void, Never>?
        lock.lock()
        guard diagnosticsLifecycle != .cancelled else {
            lock.unlock()
            return
        }
        diagnosticsLifecycle = .cancelled
        pendingDiagnostics = nil
        diagnosticsVersion &+= 1
        waiter = diagnosticsWaiter
        diagnosticsWaiter = nil
        lock.unlock()
        waiter?.resume()
    }

    private func currentSnapshotLocked() -> FfiDiagnosticsSnapshot {
        Self.snapshot(hasReceivedKind1: queryLifecycle == .active && rowDelivered)
    }

    private func locked<Value>(_ body: () -> Value) -> Value {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    private static func hasReceivedKind1(_ snapshot: FfiDiagnosticsSnapshot) -> Bool {
        snapshot.relays.contains { relay in
            relay.eventsByKind.contains { $0.kind == 1 && $0.count > 0 }
        }
    }

    private static func snapshot(hasReceivedKind1: Bool) -> FfiDiagnosticsSnapshot {
        let relays: [FfiRelayDiagnostics]
        if hasReceivedKind1 {
            relays = [
                FfiRelayDiagnostics(
                    relay: "wss://deterministic.example",
                    access: .public,
                    wireSubCount: 1,
                    authorsServed: 1,
                    byLane: [],
                    filters: ["{\"kinds\":[1]}"],
                    eventsByKind: [FfiKindCount(kind: 1, count: 1)],
                    coverage: [],
                    nip11SupportedNips: nil,
                    nip11DocumentRevision: nil,
                    nip11Freshness: nil,
                    nip11LastError: nil,
                    nip77Advertisement: "unknown",
                    nip77Behavior: "unknown",
                    nip77Handoff: "none"
                )
            ]
        } else {
            relays = []
        }
        return FfiDiagnosticsSnapshot(
            relays: relays,
            authSessions: [],
            uncoveredAuthorCount: 0,
            droppedMergeRules: [],
            discoveredPrivateRelaysRejected: 0,
            sessionsRejectedOverCap: 0,
            transportDegraded: nil,
            stalledWrites: [],
            stalledWriteTotals: FfiStalledWriteTotals(
                unroutable: 0,
                unsignable: 0,
                undeliverable: 0,
                omittedDetails: 0,
                detailLimit: 0
            )
        )
    }

    private static let rowFrame = FfiFrame(
        deltas: [
            .added(
                row: FfiRow(
                    id: String(repeating: "1", count: 64),
                    pubkey: String(repeating: "2", count: 64),
                    createdAt: 1,
                    kind: 1,
                    tags: [],
                    content: "deterministic",
                    signature: .signed(signature: String(repeating: "3", count: 128)),
                    sources: ["wss://deterministic.example"]
                )
            )
        ],
        window: nil,
        evidence: [FfiAcquisitionEvidence(sources: [], shortfall: [])]
    )
}

private final class DiagnosticsRaceRowPull: NmpRowPull, @unchecked Sendable {
    private let state: DiagnosticsRaceState

    init(state: DiagnosticsRaceState) {
        self.state = state
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        state = DiagnosticsRaceState()
        super.init(unsafeFromRawPointer: pointer)
    }

    override func receive() async throws -> FfiFrame? {
        await state.nextRow()
    }

    override func commit() throws {}

    override func abort() {}
}

private final class DiagnosticsRaceRowStream: NmpRowStream, @unchecked Sendable {
    private let state: DiagnosticsRaceState

    init(state: DiagnosticsRaceState) {
        self.state = state
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        state = DiagnosticsRaceState()
        super.init(unsafeFromRawPointer: pointer)
    }

    override func beginNext() throws -> NmpRowPull {
        DiagnosticsRaceRowPull(state: state)
    }

    override func cancel() {
        state.cancelQuery()
    }
}

private final class DiagnosticsRaceDiagnosticsStream: NmpDiagnosticsStream, @unchecked Sendable {
    private let state: DiagnosticsRaceState

    init(state: DiagnosticsRaceState) {
        self.state = state
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        state = DiagnosticsRaceState()
        super.init(unsafeFromRawPointer: pointer)
    }

    override func next() async throws -> FfiDiagnosticsSnapshot? {
        await state.nextDiagnostics()
    }

    override func cancel() {
        state.cancelDiagnostics()
    }
}

private final class DiagnosticsRaceEngine: NmpEngine, @unchecked Sendable {
    private let state: DiagnosticsRaceState

    init(state: DiagnosticsRaceState) {
        self.state = state
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        state = DiagnosticsRaceState()
        super.init(unsafeFromRawPointer: pointer)
    }

    override func observe(query: FfiFilter, window: FfiWindow?) throws -> NmpRowStream {
        state.openQuery()
        return DiagnosticsRaceRowStream(state: state)
    }

    override func observeDiagnostics() throws -> NmpDiagnosticsStream {
        state.openDiagnostics()
        return DiagnosticsRaceDiagnosticsStream(state: state)
    }
}
