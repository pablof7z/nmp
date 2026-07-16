package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import kotlinx.coroutines.flow.emptyFlow
import uniffi.nmp_ffi.FfiAccessContext
import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiAuthPhase
import uniffi.nmp_ffi.FfiCoverageInterval
import uniffi.nmp_ffi.FfiFilterCoverage
import uniffi.nmp_ffi.FfiException
import uniffi.nmp_ffi.FfiShortfallFact
import uniffi.nmp_ffi.FfiSourceEvidence
import uniffi.nmp_ffi.FfiSourceStatus
import uniffi.nmp_ffi.FfiWriteStatus
import uniffi.nmp_ffi.FfiCancelWriteException
import uniffi.nmp_ffi.FfiReceiptReattachment
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiRowDelta

class EvidenceMappingTest {
    @Test
    fun cancellationFactAndEveryRefusalRemainTyped() {
        assertEquals(WriteStatus.Cancelled, WriteStatus.from(FfiWriteStatus.Cancelled))
        assertEquals(
            NMPWriteCancellationError.UnknownReceipt(42uL),
            NMPWriteCancellationError.from(FfiCancelWriteException.UnknownReceipt(42uL)),
        )
        assertEquals(
            NMPWriteCancellationError.AlreadySigned(42uL, "event"),
            NMPWriteCancellationError.from(FfiCancelWriteException.AlreadySigned(42uL, "event")),
        )
        assertEquals(
            NMPWriteCancellationError.AlreadyCompensated(42uL),
            NMPWriteCancellationError.from(FfiCancelWriteException.AlreadyCompensated(42uL)),
        )
        assertEquals(
            NMPWriteCancellationError.AlreadyAbandoned(42uL),
            NMPWriteCancellationError.from(FfiCancelWriteException.AlreadyAbandoned(42uL)),
        )
        assertEquals(
            NMPWriteCancellationError.PersistenceFailed(42uL, "disk"),
            NMPWriteCancellationError.from(FfiCancelWriteException.PersistenceFailed(42uL, "disk")),
        )
        assertEquals(
            NMPWriteCancellationError.EngineClosed,
            NMPWriteCancellationError.from(FfiCancelWriteException.EngineClosed()),
        )
    }

    @Test
    fun sourcesGrewReplacesRowInPlaceWithoutDuplicating() {
        // #105: `SourcesGrew` must replace the row's provenance IN PLACE --
        // never a second `Added` for the same id. Drives `applyRowDelta`
        // directly (the same accumulator step `observeQuery` uses).
        val order = mutableListOf<String>()
        val byId = mutableMapOf<String, Row>()
        val ffiRow =
            FfiRow(
                id = "abc",
                pubkey = "pk",
                createdAt = 1uL,
                kind = 1u,
                tags = emptyList(),
                content = "hi",
                sig = "sig",
                sources = listOf("wss://r0.example"),
            )

        applyRowDelta(order, byId, FfiRowDelta.Added(ffiRow))
        applyRowDelta(
            order,
            byId,
            FfiRowDelta.SourcesGrew("abc", listOf("wss://r0.example", "wss://r1.example")),
        )

        assertEquals(1, order.size, "SourcesGrew must never insert a second row for the same id")
        assertEquals(listOf("wss://r0.example", "wss://r1.example"), byId["abc"]?.sources)
    }

    @Test
    fun receiptCorrelationExhaustionRemainsTypedAtTheNativeBoundary() {
        assertTrue(
            NMPError.from(FfiException.ReceiptCorrelationIdExhausted()) ===
                NMPError.ReceiptCorrelationIdExhausted,
        )
    }

    @Test
    fun liveStoreResetRefusalRemainsTypedAtTheNativeBoundary() {
        assertEquals(
            NMPError.StoreStillOpen("/canonical/nmp.redb"),
            NMPError.from(FfiException.StoreStillOpen("/canonical/nmp.redb")),
        )
    }

    @Test
    fun everyReceiptReattachmentVariantMapsWithoutCollapsingCorruptionIntoAbsence() {
        val attached = mapReceiptReattachment(FfiReceiptReattachment.ATTACHED, 42uL, emptyFlow())
        assertEquals(42uL, (attached as ReceiptReattachment.Attached).receipt.id)
        assertTrue(
            mapReceiptReattachment(FfiReceiptReattachment.NOT_FOUND, 42uL, emptyFlow()) ===
                ReceiptReattachment.NotFound,
        )
        assertTrue(
            mapReceiptReattachment(
                FfiReceiptReattachment.RETAINED_BUT_UNREADABLE,
                42uL,
                emptyFlow(),
            ) === ReceiptReattachment.RetainedButUnreadable,
        )
    }

    @Test
    fun outcomeUnknownReceiptMappingRemainsDistinctFromGaveUp() {
        val ambiguous = WriteStatus.from(FfiWriteStatus.OutcomeUnknown("wss://ambiguous.example"))
        assertEquals(WriteStatus.OutcomeUnknown("wss://ambiguous.example"), ambiguous)
        assertTrue(ambiguous != WriteStatus.GaveUp("wss://ambiguous.example"))
    }

    @Test
    fun everyRetryLaneReceiptStateMapsWithoutLosingAttemptTruth() {
        assertEquals(
            WriteStatus.AwaitingRelay("wss://offline.example"),
            WriteStatus.from(FfiWriteStatus.AwaitingRelay("wss://offline.example")),
        )
        assertEquals(
            WriteStatus.AwaitingAuth("wss://auth.example"),
            WriteStatus.from(FfiWriteStatus.AwaitingAuth("wss://auth.example")),
        )
        assertEquals(
            WriteStatus.RetryEligible("wss://retry.example", 2uL, 123uL),
            WriteStatus.from(FfiWriteStatus.RetryEligible("wss://retry.example", 2uL, 123uL)),
        )
        assertEquals(
            WriteStatus.HandoffAmbiguous("wss://ambiguous.example", 3uL, 124uL),
            WriteStatus.from(
                FfiWriteStatus.HandoffAmbiguous("wss://ambiguous.example", 3uL, 124uL),
            ),
        )
        assertEquals(
            WriteStatus.Sent("wss://written.example", 4uL, 125uL),
            WriteStatus.from(FfiWriteStatus.Sent("wss://written.example", 4uL, 125uL)),
        )
    }

    @Test
    fun persistenceBlockedReceiptMappingRemainsNonterminal() {
        val blocked = WriteStatus.from(FfiWriteStatus.PersistenceBlocked("wss://blocked.example"))
        assertEquals(WriteStatus.PersistenceBlocked("wss://blocked.example"), blocked)
        assertTrue(blocked != WriteStatus.GaveUp("wss://blocked.example"))
        assertTrue(blocked != WriteStatus.Failed("persistence"))
    }

    @Test
    fun routePersistenceBlockedDoesNotClaimDurableAttemptOwnership() {
        val blocked = WriteStatus.from(FfiWriteStatus.RoutePersistenceBlocked("wss://volatile.example"))
        assertEquals(WriteStatus.RoutePersistenceBlocked("wss://volatile.example"), blocked)
        assertTrue(blocked != WriteStatus.PersistenceBlocked("wss://volatile.example"))
    }

    @Test
    fun replaceableConflictPreservesBothWinnerIds() {
        val conflict =
            WriteStatus.from(
                FfiWriteStatus.ReplaceableConflict(
                    expected = "expected-event",
                    actual = "actual-event",
                ),
            )
        assertEquals(
            WriteStatus.ReplaceableConflict("expected-event", "actual-event"),
            conflict,
        )
    }

    @Test
    fun everyAcquisitionEvidenceVariantMapsWithoutARollup() {
        val raw =
            FfiAcquisitionEvidence(
                sources =
                    listOf(
                        source("wss://requesting.example", 10uL, FfiSourceStatus.Requesting),
                        source("wss://connecting.example", null, FfiSourceStatus.Connecting),
                        source("wss://disconnected.example", 20uL, FfiSourceStatus.Disconnected),
                        source(
                            "wss://policy.example",
                            null,
                            FfiSourceStatus.AwaitingAuth(FfiAuthPhase.AWAITING_POLICY),
                        ),
                        source(
                            "wss://signature.example",
                            null,
                            FfiSourceStatus.AwaitingAuth(FfiAuthPhase.AWAITING_SIGNATURE),
                        ),
                        source("wss://denied.example", null, FfiSourceStatus.AuthDenied),
                        source("wss://error.example", null, FfiSourceStatus.Error),
                    ),
                shortfall =
                    listOf(
                        FfiShortfallFact.NoPlannedSource("no-source-filter"),
                        FfiShortfallFact.NoResolvedDemand,
                        FfiShortfallFact.LocalLimit("limited-filter"),
                    ),
            )

        val evidence = AcquisitionEvidence.from(raw)
        assertEquals(raw.sources.map { it.relay }, evidence.sources.map { it.relay })
        assertTrue(evidence.sources[0].status === SourceStatus.Requesting)
        assertEquals(10uL, evidence.sources[0].reconciledThrough)
        assertTrue(evidence.sources[1].status === SourceStatus.Connecting)
        assertNull(evidence.sources[1].reconciledThrough)
        assertTrue(evidence.sources[2].status === SourceStatus.Disconnected)
        assertTrue(
            (evidence.sources[3].status as SourceStatus.AwaitingAuth).phase ===
                AuthPhase.AwaitingPolicy,
        )
        assertTrue(
            (evidence.sources[4].status as SourceStatus.AwaitingAuth).phase ===
                AuthPhase.AwaitingSignature,
        )
        assertTrue(evidence.sources[5].status === SourceStatus.AuthDenied)
        assertTrue(evidence.sources[6].status === SourceStatus.Error)
        assertEquals(ShortfallFact.NoPlannedSource("no-source-filter"), evidence.shortfall[0])
        assertTrue(evidence.shortfall[1] === ShortfallFact.NoResolvedDemand)
        assertEquals(ShortfallFact.LocalLimit("limited-filter"), evidence.shortfall[2])
    }

    @Test
    fun diagnosticsIntervalIsDistinctFromQueryEvidence() {
        val interval = CoverageInterval.from(FfiCoverageInterval(4uL, 9uL))
        assertEquals(4uL, interval.from)
        assertEquals(9uL, interval.through)

        val proven =
            FilterCoverage.from(
                FfiFilterCoverage("{\"kinds\":[9999]}", FfiCoverageInterval(4uL, 9uL)),
            )
        assertEquals(interval, proven.coverage)
        assertNull(FilterCoverage.from(FfiFilterCoverage("{\"kinds\":[9998]}", null)).coverage)

        val evidence =
            AcquisitionEvidence.from(
                FfiAcquisitionEvidence(
                    listOf(source("wss://source.example", 9uL, FfiSourceStatus.Disconnected)),
                    emptyList(),
                ),
            )
        assertEquals(interval.through, evidence.sources[0].reconciledThrough)
        assertTrue(evidence.sources[0].status === SourceStatus.Disconnected)
    }

    private fun source(
        relay: String,
        reconciledThrough: ULong?,
        status: FfiSourceStatus,
        access: FfiAccessContext = FfiAccessContext.Public,
    ): FfiSourceEvidence = FfiSourceEvidence(relay, access, reconciledThrough, status)
}
