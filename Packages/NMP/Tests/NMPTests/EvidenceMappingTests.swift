import XCTest
@testable import NMP
import NMPFFI

final class EvidenceMappingTests: XCTestCase {
    func testReceiptCorrelationExhaustionRemainsTypedAtTheNativeBoundary() {
        XCTAssertEqual(
            NMPError(.ReceiptCorrelationIdExhausted),
            .receiptCorrelationIdExhausted
        )
    }

    func testLiveStoreResetRefusalRemainsTypedAtTheNativeBoundary() {
        XCTAssertEqual(
            NMPError(.StoreStillOpen(path: "/canonical/nmp.redb")),
            .storeStillOpen("/canonical/nmp.redb")
        )
    }

    func testEveryReceiptReattachmentVariantMapsWithoutCollapsingCorruptionIntoAbsence() {
        let stream = AsyncStream<WriteStatus> { $0.finish() }

        guard case let .attached(receipt) = mapReceiptReattachment(.attached, id: 42, status: stream) else {
            return XCTFail("attached must remain attached")
        }
        XCTAssertEqual(receipt.id, 42)

        guard case .notFound = mapReceiptReattachment(.notFound, id: 42, status: stream) else {
            return XCTFail("notFound must remain notFound")
        }
        guard case .retainedButUnreadable = mapReceiptReattachment(
            .retainedButUnreadable,
            id: 42,
            status: stream
        ) else {
            return XCTFail("retained corruption must not collapse into absence")
        }
    }

    func testOutcomeUnknownReceiptMappingRemainsDistinctFromGaveUp() {
        XCTAssertEqual(
            WriteStatus(.outcomeUnknown(relay: "wss://ambiguous.example")),
            .outcomeUnknown(relay: "wss://ambiguous.example")
        )
        XCTAssertNotEqual(
            WriteStatus(.outcomeUnknown(relay: "wss://ambiguous.example")),
            WriteStatus(.gaveUp(relay: "wss://ambiguous.example"))
        )
    }

    func testEveryRetryLaneReceiptStateMapsWithoutLosingAttemptTruth() {
        XCTAssertEqual(
            WriteStatus(.awaitingRelay(relay: "wss://offline.example")),
            .awaitingRelay(relay: "wss://offline.example")
        )
        XCTAssertEqual(
            WriteStatus(.awaitingAuth(relay: "wss://auth.example")),
            .awaitingAuth(relay: "wss://auth.example")
        )
        XCTAssertEqual(
            WriteStatus(
                .retryEligible(
                    relay: "wss://retry.example", attempt: 2, eligibleAt: 123
                )
            ),
            .retryEligible(relay: "wss://retry.example", attempt: 2, eligibleAt: 123)
        )
        XCTAssertEqual(
            WriteStatus(
                .handoffAmbiguous(
                    relay: "wss://ambiguous.example", attempt: 3, observedAt: 124
                )
            ),
            .handoffAmbiguous(
                relay: "wss://ambiguous.example", attempt: 3, observedAt: 124
            )
        )
        XCTAssertEqual(
            WriteStatus(
                .sent(relay: "wss://written.example", attempt: 4, writtenAt: 125)
            ),
            .sent(relay: "wss://written.example", attempt: 4, writtenAt: 125)
        )
    }

    func testPersistenceBlockedReceiptMappingRemainsNonterminal() {
        let blocked = WriteStatus(.persistenceBlocked(relay: "wss://blocked.example"))
        XCTAssertEqual(blocked, .persistenceBlocked(relay: "wss://blocked.example"))
        XCTAssertNotEqual(blocked, .gaveUp(relay: "wss://blocked.example"))
        XCTAssertNotEqual(blocked, .failed(reason: "persistence"))
    }

    func testRoutePersistenceBlockedDoesNotClaimDurableAttemptOwnership() {
        let blocked = WriteStatus(.routePersistenceBlocked(relay: "wss://volatile.example"))
        XCTAssertEqual(blocked, .routePersistenceBlocked(relay: "wss://volatile.example"))
        XCTAssertNotEqual(blocked, .persistenceBlocked(relay: "wss://volatile.example"))
    }

    func testEveryAcquisitionEvidenceVariantMapsWithoutARollup() {
        let raw = FfiAcquisitionEvidence(
            sources: [
                .init(relay: "wss://requesting.example", reconciledThrough: 10, status: .requesting),
                .init(relay: "wss://connecting.example", reconciledThrough: nil, status: .connecting),
                .init(relay: "wss://disconnected.example", reconciledThrough: 20, status: .disconnected),
                .init(
                    relay: "wss://policy.example",
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingPolicy)
                ),
                .init(
                    relay: "wss://signature.example",
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingSignature)
                ),
                .init(relay: "wss://denied.example", reconciledThrough: nil, status: .authDenied),
                .init(relay: "wss://error.example", reconciledThrough: nil, status: .error),
            ],
            shortfall: [
                .noPlannedSource(atom: "no-source-filter"),
                .noResolvedDemand,
                .localLimit(atom: "limited-filter"),
            ]
        )

        let evidence = AcquisitionEvidence(raw)
        XCTAssertEqual(evidence.sources.map(\.relay), raw.sources.map(\.relay))
        XCTAssertEqual(evidence.sources[0].status, .requesting)
        XCTAssertEqual(evidence.sources[0].reconciledThrough, 10)
        XCTAssertEqual(evidence.sources[1].status, .connecting)
        XCTAssertNil(evidence.sources[1].reconciledThrough)
        XCTAssertEqual(evidence.sources[2].status, .disconnected)
        XCTAssertEqual(evidence.sources[3].status, .awaitingAuth(phase: .awaitingPolicy))
        XCTAssertEqual(evidence.sources[4].status, .awaitingAuth(phase: .awaitingSignature))
        XCTAssertEqual(evidence.sources[5].status, .authDenied)
        XCTAssertEqual(evidence.sources[6].status, .error)
        XCTAssertEqual(
            evidence.shortfall,
            [
                .noPlannedSource(atom: "no-source-filter"),
                .noResolvedDemand,
                .localLimit(atom: "limited-filter"),
            ]
        )
    }

    /// #105: `SourcesGrew` must replace the row's provenance IN PLACE --
    /// never a second `Added` for the same id. Drives `RowBridge` directly
    /// (the same accumulator `NMPQuery` uses internally) since the coalescer
    /// underneath it is already proven separately by `FrameCoalescerTests`.
    func testRowBridgeSourcesGrewReplacesRowInPlaceWithoutDuplicating() async throws {
        var continuation: AsyncStream<RowBatch>.Continuation!
        let stream = AsyncStream<RowBatch> { continuation = $0 }
        let bridge = RowBridge(continuation: continuation)
        let emptyEvidence = FfiAcquisitionEvidence(sources: [], shortfall: [])

        let ffiRow = FfiRow(
            id: "abc",
            pubkey: "pk",
            createdAt: 1,
            kind: 1,
            tags: [],
            content: "hi",
            sig: "sig",
            sources: ["wss://r0.example"]
        )
        bridge.onFrame(
            frame: FfiFrame(deltas: [.added(row: ffiRow)], window: nil, evidence: emptyEvidence)
        )
        bridge.onFrame(
            frame: FfiFrame(
                deltas: [.sourcesGrew(id: "abc", sources: ["wss://r0.example", "wss://r1.example"])],
                window: nil,
                evidence: emptyEvidence
            )
        )
        bridge.onClosed()

        var lastBatch: RowBatch?
        for await batch in stream {
            lastBatch = batch
        }
        let rows = try XCTUnwrap(lastBatch).rows
        XCTAssertEqual(rows.count, 1, "SourcesGrew must never insert a second row for the same id")
        XCTAssertEqual(rows.first?.sources, ["wss://r0.example", "wss://r1.example"])
    }

    func testDiagnosticsIntervalIsDistinctFromQueryEvidence() {
        let interval = CoverageInterval(FfiCoverageInterval(from: 4, through: 9))
        XCTAssertEqual(interval.from, 4)
        XCTAssertEqual(interval.through, 9)

        let proven = FilterCoverage(
            FfiFilterCoverage(
                filter: "{\"kinds\":[9999]}",
                coverage: FfiCoverageInterval(from: 4, through: 9)
            )
        )
        XCTAssertEqual(proven.coverage, interval)

        let unproven = FilterCoverage(
            FfiFilterCoverage(filter: "{\"kinds\":[9998]}", coverage: nil)
        )
        XCTAssertNil(unproven.coverage)

        let evidence = AcquisitionEvidence(
            FfiAcquisitionEvidence(
                sources: [
                    .init(
                        relay: "wss://source.example",
                        reconciledThrough: 9,
                        status: .disconnected
                    )
                ],
                shortfall: []
            )
        )
        XCTAssertEqual(evidence.sources[0].reconciledThrough, interval.through)
        XCTAssertEqual(evidence.sources[0].status, .disconnected)
    }
}
