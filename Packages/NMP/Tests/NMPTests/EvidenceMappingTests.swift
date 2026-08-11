import XCTest
@testable import NMP
import NMPFFI

final class EvidenceMappingTests: XCTestCase {
    func testCancellationFactAndEveryRefusalRemainTyped() {
        XCTAssertEqual(
            WriteFact(.outcome(outcome: .notSent(reason: .cancelled))),
            .outcome(.notSent(.cancelled))
        )
        XCTAssertEqual(
            WriteFact(.outcome(outcome: .notSent(reason: .superseded))),
            .outcome(.notSent(.superseded))
        )
        XCTAssertEqual(
            WriteFact(.outcome(outcome: .notSent(reason: .signerRefused))),
            .outcome(.notSent(.signerRefused))
        )
        XCTAssertEqual(
            WriteFact(.outcome(outcome: .superseded)),
            .outcome(.superseded)
        )
        XCTAssertEqual(
            NMPWriteCancellationError(.UnknownReceipt(receiptId: 42)),
            .unknownReceipt(receiptId: 42)
        )
        XCTAssertEqual(
            NMPWriteCancellationError(.AlreadySigned(receiptId: 42, eventId: "event")),
            .alreadySigned(receiptId: 42, eventId: "event")
        )
        XCTAssertEqual(
            NMPWriteCancellationError(.AlreadyCompensated(receiptId: 42)),
            .alreadyCompensated(receiptId: 42)
        )
        XCTAssertEqual(
            NMPWriteCancellationError(.AlreadySuperseded(receiptId: 42)),
            .alreadySuperseded(receiptId: 42)
        )
        XCTAssertEqual(
            NMPWriteCancellationError(.AlreadyRefused(receiptId: 42)),
            .alreadyRefused(receiptId: 42)
        )
        XCTAssertEqual(
            NMPWriteCancellationError(.PersistenceFailed(receiptId: 42, reason: "disk")),
            .persistenceFailed(receiptId: 42, reason: "disk")
        )
        XCTAssertEqual(
            NMPWriteCancellationError(.EngineClosed),
            .engineClosed
        )
    }

    /// The queue-entry removal door is the other end of custody: a write that
    /// nothing is going to move is forgotten HERE, so each of its refusals has
    /// to survive as its own fact -- "I do not know that receipt" and "that
    /// receipt still owns open delivery work, cancel it first" are different
    /// instructions to the app and must never collapse into one.
    func testEveryQueueEntryRemovalRefusalRemainsTypedAtTheNativeBoundary() {
        XCTAssertEqual(
            NMPQueueEntryRemovalError(.UnknownReceipt(receiptId: 42)),
            .unknownReceipt(receiptId: 42)
        )
        XCTAssertEqual(
            NMPQueueEntryRemovalError(.StillActive(receiptId: 42)),
            .stillActive(receiptId: 42)
        )
        XCTAssertNotEqual(
            NMPQueueEntryRemovalError(.StillActive(receiptId: 42)),
            .unknownReceipt(receiptId: 42)
        )
        XCTAssertEqual(
            NMPQueueEntryRemovalError(.PersistenceFailed(receiptId: 42, reason: "disk")),
            .persistenceFailed(receiptId: 42, reason: "disk")
        )
        XCTAssertEqual(
            NMPQueueEntryRemovalError(.EngineClosed),
            .engineClosed
        )
    }

    func testLiveStoreResetRefusalRemainsTypedAtTheNativeBoundary() {
        XCTAssertEqual(
            NMPError(.StoreStillOpen(path: "/canonical/nmp.redb")),
            .storeStillOpen("/canonical/nmp.redb")
        )
    }

    /// #489: the second-owner refusal must survive to the native surface as
    /// its own fact, distinct from "the store could not be opened".
    func testSecondStoreOwnerRefusalRemainsTypedAtTheNativeBoundary() {
        XCTAssertEqual(
            NMPError(.StoreAlreadyOpen(path: "/canonical/nmp.redb")),
            .storeAlreadyOpen("/canonical/nmp.redb")
        )
        XCTAssertNotEqual(
            NMPError(.StoreAlreadyOpen(path: "/canonical/nmp.redb")),
            .storeOpenFailed("/canonical/nmp.redb")
        )
    }

    /// #920: an app deciding whether to delete a multi-gigabyte store must
    /// branch on a type, never on prose. The epoch refusal arrives as its own
    /// fact carrying the path; every other open refusal — damaged bytes, a
    /// refused lock — stays `.storeOpenFailed`, where deleting is wrong.
    func testSupersededEpochRefusalRemainsTypedAndSeparableAtTheNativeBoundary() {
        XCTAssertEqual(
            NMPError(.StoreUnsupportedSchema(path: "/canonical/nmp.redb", expected: 13, found: 10)),
            .storeUnsupportedSchema(path: "/canonical/nmp.redb", expected: 13, found: 10)
        )
        // A marker this build cannot read is absent, not zero, and is still
        // the epoch refusal — the exact shape a real 1 GB store hit.
        let unreadable = NMPError(
            .StoreUnsupportedSchema(path: "/canonical/nmp.redb", expected: 13, found: nil)
        )
        XCTAssertEqual(
            unreadable,
            .storeUnsupportedSchema(path: "/canonical/nmp.redb", expected: 13, found: nil)
        )
        guard case .storeUnsupportedSchema(let path, _, let found) = unreadable else {
            return XCTFail("the epoch refusal must be branchable without reading its text")
        }
        XCTAssertEqual(path, "/canonical/nmp.redb")
        XCTAssertNil(found)
        XCTAssertTrue(
            unreadable.localizedDescription.contains("discard and recreate this store to continue")
        )

        let damaged = NMPError(.StoreOpenFailed(reason: "corrupted region"))
        XCTAssertNotEqual(
            damaged,
            .storeUnsupportedSchema(path: "/canonical/nmp.redb", expected: 13, found: nil)
        )
        XCTAssertFalse(damaged.localizedDescription.contains("discard and recreate"))
    }

    func testFiniteFactDeliveryFailuresRemainTypedAtTheNativeBoundary() {
        XCTAssertEqual(
            NMPError(.FactStreamLagged(receiptId: 42)),
            .factStreamLagged(receiptId: 42)
        )
        XCTAssertEqual(
            NMPError(.FactStreamLagged(receiptId: nil)),
            .factStreamLagged(receiptId: nil)
        )
        XCTAssertEqual(
            NMPError(.ReceiptReplayUnavailable(receiptId: 42)),
            .receiptReplayUnavailable(receiptId: 42)
        )
    }

    /// #680: `reattachReceipt` now maps the generated `FfiReceiptReattachment`
    /// directly -- `.attached` carries a live `NmpReceiptStream` (proven
    /// end-to-end by `WriteCancellationTests`), while an unknown id must stay
    /// distinctly `.notFound`, never collapsed with corrupt-but-retained
    /// evidence. Reattaching an id no receipt was ever issued for is the
    /// reachable `.notFound` case.
    func testUnknownReceiptReattachmentStaysNotFound() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        guard case .notFound = try engine.reattachReceipt(id: 999_999) else {
            return XCTFail("an id no receipt was issued for must remain notFound")
        }
    }

    /// A socket write that flushed is NOT the relay having taken the event.
    /// `.sent` is deliberately nonterminal and deliberately not equal to
    /// `.published`: collapsing them would let an app tell a user their note
    /// landed on the strength of its own outbound bytes.
    func testSentIsNeitherTerminalNorEqualToPublished() {
        let sent = RelayState(.sent(attempt: 4, writtenAt: 125))
        XCTAssertEqual(sent, .sent(attempt: 4, writtenAt: 125))
        XCTAssertFalse(sent.isTerminal)
        XCTAssertNotEqual(sent, .published)
        XCTAssertTrue(RelayState(.published).isTerminal)
        XCTAssertTrue(RelayState(.gaveUp).isTerminal)
    }

    func testEveryRetryLaneReceiptStateMapsWithoutLosingAttemptTruth() {
        XCTAssertEqual(
            WriteFact(.relay(relay: "wss://offline.example", state: .waiting(waiting: .notConnected))),
            .relay(relay: "wss://offline.example", state: .waiting(.notConnected))
        )
        XCTAssertEqual(
            WriteFact(.relay(relay: "wss://auth.example", state: .waiting(waiting: .needsAuth))),
            .relay(relay: "wss://auth.example", state: .waiting(.needsAuth))
        )
        XCTAssertEqual(
            WriteFact(
                .relay(
                    relay: "wss://auth.example",
                    state: .authFailed(
                        pubkey: String(repeating: "a", count: 64),
                        source: .policy,
                        reason: "account not permitted"
                    )
                )
            ),
            .relay(
                relay: "wss://auth.example",
                state: .authFailed(
                    pubkey: String(repeating: "a", count: 64),
                    source: .policy,
                    reason: "account not permitted"
                )
            )
        )
        XCTAssertEqual(
            WriteFact(
                .relay(
                    relay: "wss://retry.example",
                    state: .waiting(
                        waiting: .backingOff(
                            attempt: 2,
                            eligibleAt: 123,
                            cause: .relayRateLimited,
                            detail: "rate-limited: slow down"
                        )
                    )
                )
            ),
            .relay(
                relay: "wss://retry.example",
                state: .waiting(
                    .backingOff(
                        attempt: 2,
                        eligibleAt: 123,
                        cause: .relayRateLimited,
                        detail: "rate-limited: slow down"
                    )
                )
            )
        )
        XCTAssertEqual(
            WriteFact(
                .relay(relay: "wss://written.example", state: .sent(attempt: 4, writtenAt: 125))
            ),
            .relay(relay: "wss://written.example", state: .sent(attempt: 4, writtenAt: 125))
        )
    }

    /// A lane the local disk stalled is a lane that is still ours: it is
    /// `.waiting`, so it is nonterminal, and it is not the relay having
    /// refused us or the attempt ceiling having been reached.
    func testPersistenceStalledReceiptMappingRemainsNonterminal() {
        let blocked = RelayState(.waiting(waiting: .persistenceStalled(detail: "disk full")))
        XCTAssertEqual(blocked, .waiting(.persistenceStalled(detail: "disk full")))
        XCTAssertFalse(blocked.isTerminal)
        XCTAssertNotEqual(blocked, .gaveUp)
        XCTAssertNotEqual(blocked, .rejected(reason: "disk full"))
    }

    /// A stalled durable fact carries the WHY across intact. Two different
    /// stalls are two different facts -- the detail is the only thing that
    /// distinguishes them, so it must not be dropped or rolled up -- and
    /// neither of them claims a wire attempt: no `EVENT` was emitted, so no
    /// attempt ordinal is spent.
    func testPersistenceStalledKeepsItsDetailAndClaimsNoAttempt() {
        let route = RelayState(.waiting(waiting: .persistenceStalled(detail: "route not committed")))
        let attempt = RelayState(.waiting(waiting: .persistenceStalled(detail: "attempt not committed")))
        XCTAssertEqual(route, .waiting(.persistenceStalled(detail: "route not committed")))
        XCTAssertNotEqual(route, attempt)
        XCTAssertNotEqual(route, .sent(attempt: 1, writtenAt: 0))
    }

    func testEveryAcquisitionEvidenceVariantMapsWithoutARollup() {
        let raw = FfiAcquisitionEvidence(
            sources: [
                .init(relay: "wss://requesting.example", access: .public, reconciledThrough: 10, status: .requesting),
                .init(relay: "wss://finished.example", access: .public, reconciledThrough: 11, status: .finishedStoredEvents),
                .init(relay: "wss://awaiting.example", access: .public, reconciledThrough: nil, status: .awaitingRequest),
                .init(relay: "wss://satisfied.example", access: .public, reconciledThrough: 12, status: .coverageSatisfied),
                .init(relay: "wss://connecting.example", access: .public, reconciledThrough: nil, status: .connecting),
                .init(relay: "wss://disconnected.example", access: .public, reconciledThrough: 20, status: .disconnected),
                .init(
                    relay: "wss://challenge.example",
                    access: .nip42(publicKey: String(repeating: "a", count: 64)),
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingChallenge)
                ),
                .init(
                    relay: "wss://policy.example",
                    access: .public,
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingPolicy)
                ),
                .init(
                    relay: "wss://signature.example",
                    access: .public,
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingSignature)
                ),
                .init(
                    relay: "wss://ack.example",
                    access: .public,
                    reconciledThrough: nil,
                    status: .awaitingAuth(phase: .awaitingRelayAck)
                ),
                .init(relay: "wss://denied.example", access: .public, reconciledThrough: nil, status: .authDenied),
                .init(relay: "wss://error.example", access: .public, reconciledThrough: nil, status: .error),
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
        XCTAssertEqual(evidence.sources[1].status, .finishedStoredEvents)
        XCTAssertEqual(evidence.sources[2].status, .awaitingRequest)
        XCTAssertEqual(evidence.sources[3].status, .coverageSatisfied)
        XCTAssertEqual(evidence.sources[4].status, .connecting)
        XCTAssertNil(evidence.sources[4].reconciledThrough)
        XCTAssertEqual(evidence.sources[5].status, .disconnected)
        XCTAssertEqual(evidence.sources[6].status, .awaitingAuth(phase: .awaitingChallenge))
        XCTAssertEqual(
            evidence.sources[6].access,
            .nip42(publicKey: String(repeating: "a", count: 64))
        )
        XCTAssertEqual(evidence.sources[7].status, .awaitingAuth(phase: .awaitingPolicy))
        XCTAssertEqual(evidence.sources[8].status, .awaitingAuth(phase: .awaitingSignature))
        XCTAssertEqual(evidence.sources[9].status, .awaitingAuth(phase: .awaitingRelayAck))
        XCTAssertEqual(evidence.sources[10].status, .authDenied)
        XCTAssertEqual(evidence.sources[11].status, .error)
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
    /// never a second `Added` for the same id. Drives `RowAccumulator`
    /// directly (the exact per-frame fold `NMPQuery`'s iterator now runs,
    /// #680) since the coalescer over the pull loop is proven separately by
    /// `PullIteratorCoreTests`.
    func testRowAccumulatorSourcesGrewReplacesRowInPlaceWithoutDuplicating() throws {
        let accumulator = RowAccumulator()
        let emptyEvidence = [FfiAcquisitionEvidence(sources: [], shortfall: [])]

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
        _ = accumulator.fold(
            FfiFrame(deltas: [.added(row: ffiRow)], window: nil, evidence: emptyEvidence)
        )
        let last = accumulator.fold(
            FfiFrame(
                deltas: [.sourcesGrew(id: "abc", sources: ["wss://r0.example", "wss://r1.example"])],
                window: nil,
                evidence: emptyEvidence
            )
        )

        XCTAssertEqual(last.rows.count, 1, "SourcesGrew must never insert a second row for the same id")
        XCTAssertEqual(last.rows.first?.sources, ["wss://r0.example", "wss://r1.example"])
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
                        access: .public,
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
