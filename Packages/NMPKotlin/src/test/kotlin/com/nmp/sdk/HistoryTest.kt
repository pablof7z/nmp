package com.nmp.sdk

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiHistoryBatch
import uniffi.nmp_ffi.FfiHistoryLoadException
import uniffi.nmp_ffi.FfiHistoryLoadFact
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiRowDelta
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull
import kotlin.test.assertTrue

class HistoryTest {
    private val emptyEvidence = FfiAcquisitionEvidence(emptyList(), emptyList())

    @Test
    fun reducerConsumesExactDeltasButRetainsAuthoritativeBoundedRows() {
        val a = ffiRow("a", 300uL)
        val b = ffiRow("b", 200uL)
        val c = ffiRow("c", 400uL)

        val first =
            reduceHistoryFrame(
                emptyMap(),
                ffiBatch(rows = listOf(a), deltas = listOf(FfiRowDelta.Added(a))),
                maxRows = 2uL,
            )
        val second =
            reduceHistoryFrame(
                first.rows.associateBy(Row::id),
                ffiBatch(
                    rows = listOf(a, b),
                    deltas = listOf(FfiRowDelta.Added(b)),
                    load = FfiHistoryLoadFact.Returned(1uL),
                ),
                maxRows = 2uL,
            )
        val third =
            reduceHistoryFrame(
                second.rows.associateBy(Row::id),
                ffiBatch(
                    rows = listOf(c, a),
                    deltas = listOf(FfiRowDelta.Removed("b"), FfiRowDelta.Added(c)),
                    load = FfiHistoryLoadFact.Requesting,
                ),
                maxRows = 2uL,
            )

        assertEquals(listOf("c", "a"), third.rows.map(Row::id))
        assertEquals(HistoryLoadFact.Requesting, third.load)
        assertTrue(third.rows.size <= 2)
    }

    @Test
    fun reducerRejectsAnAuthoritativeFrameAboveItsDeclaredBound() {
        val a = ffiRow("a", 300uL)
        val b = ffiRow("b", 200uL)
        assertFailsWith<IllegalStateException> {
            reduceHistoryFrame(
                emptyMap(),
                ffiBatch(
                    rows = listOf(a, b),
                    deltas = listOf(FfiRowDelta.Added(a), FfiRowDelta.Added(b)),
                ),
                maxRows = 1uL,
            )
        }
    }

    @Test
    fun bridgeRetainsOnlyTheLatestBoundedStateBeforeCollectionStarts() {
        val bridge = HistoryBridge(maxRows = 2uL)
        val a = ffiRow("a", 300uL)
        val b = ffiRow("b", 200uL)
        bridge.onBatch(ffiBatch(listOf(a), listOf(FfiRowDelta.Added(a))))
        bridge.onBatch(
            ffiBatch(
                rows = listOf(a, b),
                deltas = listOf(FfiRowDelta.Added(b)),
                load = FfiHistoryLoadFact.Returned(1uL),
            ),
        )

        val delivered = mutableListOf<HistoryBatch>()
        bridge.attach(emit = delivered::add, finish = {})

        assertEquals(1, delivered.size)
        assertEquals(listOf("a", "b"), delivered.single().rows.map(Row::id))
        assertEquals(HistoryLoadFact.Returned(1uL), delivered.single().load)
        bridge.finish()
    }

    @Test
    fun everyHistoryLoadFailureKeepsItsTypedAxis() {
        assertEquals(
            NMPHistoryLoadError.WrongVersion,
            NMPHistoryLoadError.from(FfiHistoryLoadException.WrongVersion()),
        )
        assertEquals(
            NMPHistoryLoadError.WrongEngine,
            NMPHistoryLoadError.from(FfiHistoryLoadException.WrongEngine()),
        )
        assertEquals(
            NMPHistoryLoadError.WrongSession,
            NMPHistoryLoadError.from(FfiHistoryLoadException.WrongSession()),
        )
        assertEquals(
            NMPHistoryLoadError.WrongDescriptor,
            NMPHistoryLoadError.from(FfiHistoryLoadException.WrongDescriptor()),
        )
        assertEquals(
            NMPHistoryLoadError.StaleGeneration,
            NMPHistoryLoadError.from(FfiHistoryLoadException.StaleGeneration()),
        )
        assertEquals(
            NMPHistoryLoadError.LoadInProgress,
            NMPHistoryLoadError.from(FfiHistoryLoadException.LoadInProgress()),
        )
        assertEquals(
            NMPHistoryLoadError.AtBound(2uL),
            NMPHistoryLoadError.from(FfiHistoryLoadException.AtBound(2uL)),
        )
        assertEquals(
            NMPHistoryLoadError.NoBoundary,
            NMPHistoryLoadError.from(FfiHistoryLoadException.NoBoundary()),
        )
        assertEquals(
            NMPHistoryLoadError.StoreUnavailable,
            NMPHistoryLoadError.from(FfiHistoryLoadException.StoreUnavailable()),
        )
        assertEquals(
            NMPHistoryLoadError.TransportUnavailable("offline"),
            NMPHistoryLoadError.from(FfiHistoryLoadException.TransportUnavailable("offline")),
        )
    }

    @Test
    fun validationAndExplicitCancellationReturnNativeTaskBaseline() = runBlocking {
        val engine = NMPEngine(NMPConfig(maxNativeTasks = 1u))
        try {
            val demand = historyDemand(7_779u)
            assertFailsWith<NMPError.HistoryZeroPageSize> {
                engine.observeHistory(demand, pageSize = 0uL, maxRows = 2uL)
            }
            assertFailsWith<NMPError.HistoryPageExceedsMaxRows> {
                engine.observeHistory(demand, pageSize = 3uL, maxRows = 2uL)
            }
            assertFailsWith<NMPError.HistorySelectionHasLimit> {
                engine.observeHistory(
                    demand.copy(selection = demand.selection.copy(limit = 1u)),
                    pageSize = 1uL,
                    maxRows = 2uL,
                )
            }

            val query = engine.observeHistory(demand, pageSize = 1uL, maxRows = 2uL)
            val first = CompletableDeferred<HistoryBatch>()
            val collection = launch {
                query.batches.collect { batch -> first.complete(batch) }
            }
            val batch = withTimeout(5_000) { first.await() }
            assertEquals(emptyList(), batch.rows)
            assertEquals(HistoryLoadFact.Idle, batch.load)
            assertNull(batch.continuation)
            assertEquals(1uL, engine.nativeTaskCensus().admitted)

            query.cancel()
            withTimeout(5_000) { collection.join() }
            engine.awaitNativeTasksIdle()
            assertEquals(0uL, engine.nativeTaskCensus().admitted)
            assertEquals(0uL, engine.nativeTaskCensus().running)
        } finally {
            engine.shutdown()
        }
    }

    @Test
    fun engineShutdownClosesHistoryCollectionWithinBound() = runBlocking {
        val engine = NMPEngine(NMPConfig())
        val query = engine.observeHistory(historyDemand(7_780u), pageSize = 1uL, maxRows = 2uL)
        val first = CompletableDeferred<Unit>()
        val collection = launch {
            query.batches.collect { first.complete(Unit) }
        }
        withTimeout(5_000) { first.await() }

        engine.shutdown()
        withTimeout(5_000) { collection.join() }
    }

    private fun historyDemand(kind: UShort) =
        NMPDemand(selection = NMPFilter(kinds = listOf(kind)), source = NMPSourceAuthority.Public)

    private fun ffiBatch(
        rows: List<FfiRow>,
        deltas: List<FfiRowDelta>,
        load: FfiHistoryLoadFact = FfiHistoryLoadFact.Idle,
    ) =
        FfiHistoryBatch(
            rows = rows,
            deltas = deltas,
            continuation = null,
            evidence = emptyEvidence,
            load = load,
        )

    private fun ffiRow(id: String, createdAt: ULong) =
        FfiRow(
            id = id,
            pubkey = "pk",
            createdAt = createdAt,
            kind = 7_779u,
            tags = emptyList(),
            content = id,
            sig = "sig",
            sources = listOf("wss://history.example"),
        )
}
