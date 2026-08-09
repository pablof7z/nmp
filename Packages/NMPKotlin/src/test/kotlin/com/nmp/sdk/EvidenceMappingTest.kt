package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiAccessContext
import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiAuthPhase
import uniffi.nmp_ffi.FfiAuthDenialSource
import uniffi.nmp_ffi.FfiCoverageInterval
import uniffi.nmp_ffi.FfiFilterCoverage
import uniffi.nmp_ffi.FfiException
import uniffi.nmp_ffi.FfiShortfallFact
import uniffi.nmp_ffi.FfiSourceEvidence
import uniffi.nmp_ffi.FfiSourceStatus
import uniffi.nmp_ffi.FfiNotSentReason
import uniffi.nmp_ffi.FfiRefuseReason
import uniffi.nmp_ffi.FfiRelayState
import uniffi.nmp_ffi.FfiRelayWaiting
import uniffi.nmp_ffi.FfiWriteFact
import uniffi.nmp_ffi.FfiWriteOutcome
import uniffi.nmp_ffi.FfiCancelWriteException
import uniffi.nmp_ffi.FfiReceiptReattachment
import uniffi.nmp_ffi.FfiRetryCause
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiRowDelta

class EvidenceMappingTest {
    @Test
    fun cancellationFactAndEveryRefusalRemainTyped() {
        assertEquals(
            WriteFact.Outcome(WriteOutcome.NotSent(NotSentReason.Cancelled)),
            WriteFact.from(
                FfiWriteFact.Outcome(FfiWriteOutcome.NotSent(FfiNotSentReason.CANCELLED)),
            ),
        )
        assertEquals(
            WriteFact.Outcome(WriteOutcome.NotSent(NotSentReason.Superseded)),
            WriteFact.from(
                FfiWriteFact.Outcome(FfiWriteOutcome.NotSent(FfiNotSentReason.SUPERSEDED)),
            ),
        )
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
            NMPWriteCancellationError.AlreadySuperseded(42uL),
            NMPWriteCancellationError.from(FfiCancelWriteException.AlreadySuperseded(42uL)),
        )
        assertEquals(
            NMPWriteCancellationError.AlreadyRefused(42uL),
            NMPWriteCancellationError.from(FfiCancelWriteException.AlreadyRefused(42uL)),
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
    fun liveStoreResetRefusalRemainsTypedAtTheNativeBoundary() {
        assertEquals(
            NMPError.StoreStillOpen("/canonical/nmp.redb"),
            NMPError.from(FfiException.StoreStillOpen("/canonical/nmp.redb")),
        )
    }

    /** #489: the second-owner refusal must survive to the native surface as
     * its own fact, distinct from "the store could not be opened". */
    @Test
    fun secondStoreOwnerRefusalRemainsTypedAtTheNativeBoundary() {
        val refusal = NMPError.from(FfiException.StoreAlreadyOpen("/canonical/nmp.redb"))
        assertEquals(NMPError.StoreAlreadyOpen("/canonical/nmp.redb"), refusal)
        assertTrue(refusal != NMPError.StoreOpenFailed("/canonical/nmp.redb"))
    }

    /** #920: an app deciding whether to delete a multi-gigabyte store must
     * branch on a type, never on prose. The epoch refusal arrives as its own
     * fact carrying the path; every other open refusal -- damaged bytes, a
     * refused lock -- stays [NMPError.StoreOpenFailed], where deleting the
     * file is the wrong move. */
    @Test
    fun supersededEpochRefusalRemainsTypedAndSeparableAtTheNativeBoundary() {
        val readable =
            NMPError.from(FfiException.StoreUnsupportedSchema("/canonical/nmp.redb", 13uL, 10uL))
        assertEquals(
            NMPError.StoreUnsupportedSchema("/canonical/nmp.redb", 13uL, 10uL),
            readable,
        )
        val branched = readable as? NMPError.StoreUnsupportedSchema
        assertTrue(branched != null, "the epoch refusal must be branchable without reading its text")
        assertEquals("/canonical/nmp.redb", branched!!.path)
        assertEquals(10uL, branched.found)

        // A marker this build cannot read is absent, not zero, and is still
        // the epoch refusal -- the exact shape a real 1 GB store hit.
        val unreadable =
            NMPError.from(FfiException.StoreUnsupportedSchema("/canonical/nmp.redb", 13uL, null))
        assertEquals(
            NMPError.StoreUnsupportedSchema("/canonical/nmp.redb", 13uL, null),
            unreadable,
        )
        assertTrue(
            unreadable.message!!.contains("discard and recreate this store to continue"),
        )
        assertTrue(unreadable.message!!.contains("permanently lost"))

        val damaged = NMPError.from(FfiException.StoreOpenFailed("corrupted region"))
        assertTrue(damaged !is NMPError.StoreUnsupportedSchema)
        assertTrue(!damaged.message!!.contains("discard and recreate"))
    }

    @Test
    fun finiteFactDeliveryFailuresRemainTypedAtTheNativeBoundary() {
        assertEquals(
            NMPError.FactStreamLagged(42u),
            NMPError.from(FfiException.FactStreamLagged(42u)),
        )
        assertEquals(
            NMPError.FactStreamLagged(null),
            NMPError.from(FfiException.FactStreamLagged(null)),
        )
        assertEquals(
            NMPError.ReceiptReplayUnavailable(42u),
            NMPError.from(FfiException.ReceiptReplayUnavailable(42u)),
        )
    }

    @Test
    fun everyReceiptReattachmentVariantMapsWithoutCollapsingCorruptionIntoAbsence() {
        // #680: `FfiReceiptReattachment.Attached` now carries a live
        // `NmpReceiptStream` (exercised by the integration suite); the
        // corruption-vs-absence distinction this test guards lives entirely in
        // the two non-stream terminals, which must stay DISTINCT objects --
        // retained-but-unreadable is never collapsed into not-found. The
        // `attach` mapper must not run for either terminal.
        val unusedAttach: (uniffi.nmp_ffi.NmpReceiptStream) -> Receipt =
            { error("attach must not run for a non-Attached reattachment") }

        val notFound = mapReceiptReattachment(FfiReceiptReattachment.NotFound, unusedAttach)
        val unreadable =
            mapReceiptReattachment(FfiReceiptReattachment.RetainedButUnreadable, unusedAttach)

        assertTrue(notFound === ReceiptReattachment.NotFound)
        assertTrue(unreadable === ReceiptReattachment.RetainedButUnreadable)
        assertTrue(notFound !== unreadable)
    }

    @Test
    fun everyRetryLaneRelayStateMapsWithoutLosingAttemptTruth() {
        assertEquals(
            WriteFact.Relay(
                "wss://offline.example",
                RelayState.Waiting(RelayWaiting.NotConnected),
            ),
            WriteFact.from(
                FfiWriteFact.Relay(
                    "wss://offline.example",
                    FfiRelayState.Waiting(FfiRelayWaiting.NotConnected),
                ),
            ),
        )
        assertEquals(
            WriteFact.Relay("wss://auth.example", RelayState.Waiting(RelayWaiting.NeedsAuth)),
            WriteFact.from(
                FfiWriteFact.Relay(
                    "wss://auth.example",
                    FfiRelayState.Waiting(FfiRelayWaiting.NeedsAuth),
                ),
            ),
        )
        assertEquals(
            WriteFact.Relay(
                "wss://auth.example",
                RelayState.AuthFailed(
                    "a".repeat(64),
                    AuthDenialSource.Policy,
                    "account not permitted",
                ),
            ),
            WriteFact.from(
                FfiWriteFact.Relay(
                    "wss://auth.example",
                    FfiRelayState.AuthFailed(
                        "a".repeat(64),
                        FfiAuthDenialSource.POLICY,
                        "account not permitted",
                    ),
                ),
            ),
        )
        assertEquals(
            WriteFact.Relay(
                "wss://retry.example",
                RelayState.Waiting(
                    RelayWaiting.BackingOff(
                        2uL,
                        123uL,
                        RetryCause.RelayRateLimited,
                        "rate-limited: slow down",
                    ),
                ),
            ),
            WriteFact.from(
                FfiWriteFact.Relay(
                    "wss://retry.example",
                    FfiRelayState.Waiting(
                        FfiRelayWaiting.BackingOff(
                            2uL,
                            123uL,
                            FfiRetryCause.RELAY_RATE_LIMITED,
                            "rate-limited: slow down",
                        ),
                    ),
                ),
            ),
        )
        assertEquals(
            WriteFact.Relay("wss://written.example", RelayState.Sent(4uL, 125uL)),
            WriteFact.from(
                FfiWriteFact.Relay("wss://written.example", FfiRelayState.Sent(4uL, 125uL)),
            ),
        )
    }

    /** A stalled local disk owns the lane but has emitted nothing on the
     * wire: it must never be read as the lane being finished with. */
    @Test
    fun persistenceStalledRelayStateMappingRemainsNonterminal() {
        val stalled =
            WriteFact.from(
                FfiWriteFact.Relay(
                    "wss://blocked.example",
                    FfiRelayState.Waiting(FfiRelayWaiting.PersistenceStalled("disk full")),
                ),
            )
        assertEquals(
            WriteFact.Relay(
                "wss://blocked.example",
                RelayState.Waiting(RelayWaiting.PersistenceStalled("disk full")),
            ),
            stalled,
        )
        val state = (stalled as WriteFact.Relay).state
        assertFalse(state.isTerminal)
        assertTrue(state != RelayState.GaveUp)
    }

    @Test
    fun replaceableBaseChangedPreservesBothWinnerIds() {
        val refused =
            WriteFact.from(
                FfiWriteFact.Outcome(
                    FfiWriteOutcome.Refused(
                        FfiRefuseReason.ReplaceableBaseChanged(
                            expected = "expected-event",
                            actual = "actual-event",
                        ),
                    ),
                ),
            )
        assertEquals(
            WriteFact.Outcome(
                WriteOutcome.Refused(
                    RefuseReason.ReplaceableBaseChanged("expected-event", "actual-event"),
                ),
            ),
            refused,
        )
    }

    @Test
    fun everyAcquisitionEvidenceVariantMapsWithoutARollup() {
        val raw =
            FfiAcquisitionEvidence(
                sources =
                    listOf(
                        source("wss://requesting.example", 10uL, FfiSourceStatus.Requesting),
                        source("wss://finished.example", 11uL, FfiSourceStatus.FinishedStoredEvents),
                        source("wss://awaiting.example", null, FfiSourceStatus.AwaitingRequest),
                        source("wss://satisfied.example", 12uL, FfiSourceStatus.CoverageSatisfied),
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
        assertTrue(evidence.sources[1].status === SourceStatus.FinishedStoredEvents)
        assertTrue(evidence.sources[2].status === SourceStatus.AwaitingRequest)
        assertNull(evidence.sources[2].reconciledThrough)
        assertTrue(evidence.sources[3].status === SourceStatus.CoverageSatisfied)
        assertTrue(evidence.sources[4].status === SourceStatus.Connecting)
        assertNull(evidence.sources[4].reconciledThrough)
        assertTrue(evidence.sources[5].status === SourceStatus.Disconnected)
        assertTrue(
            (evidence.sources[6].status as SourceStatus.AwaitingAuth).phase ===
                AuthPhase.AwaitingPolicy,
        )
        assertTrue(
            (evidence.sources[7].status as SourceStatus.AwaitingAuth).phase ===
                AuthPhase.AwaitingSignature,
        )
        assertTrue(evidence.sources[8].status === SourceStatus.AuthDenied)
        assertTrue(evidence.sources[9].status === SourceStatus.Error)
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
