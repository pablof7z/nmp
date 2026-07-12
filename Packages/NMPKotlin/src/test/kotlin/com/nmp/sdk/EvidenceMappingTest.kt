package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import kotlinx.coroutines.flow.emptyFlow
import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiAuthPhase
import uniffi.nmp_ffi.FfiCoverageInterval
import uniffi.nmp_ffi.FfiFilterCoverage
import uniffi.nmp_ffi.FfiException
import uniffi.nmp_ffi.FfiShortfallFact
import uniffi.nmp_ffi.FfiSourceEvidence
import uniffi.nmp_ffi.FfiSourceStatus
import uniffi.nmp_ffi.FfiWriteStatus
import uniffi.nmp_ffi.FfiReceiptReattachment

class EvidenceMappingTest {
    @Test
    fun receiptCorrelationExhaustionRemainsTypedAtTheNativeBoundary() {
        assertTrue(
            NMPError.from(FfiException.ReceiptCorrelationIdExhausted()) ===
                NMPError.ReceiptCorrelationIdExhausted,
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
    ): FfiSourceEvidence = FfiSourceEvidence(relay, reconciledThrough, status)
}
