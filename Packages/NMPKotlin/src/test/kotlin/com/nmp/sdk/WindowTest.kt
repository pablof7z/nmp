package com.nmp.sdk

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiFrame
import uniffi.nmp_ffi.FfiRequestRowsException
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiRowDelta
import uniffi.nmp_ffi.FfiWindowContents
import uniffi.nmp_ffi.FfiWindowLoad
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertNull

class WindowTest {
    private val emptyEvidence = FfiAcquisitionEvidence(emptyList(), emptyList())

    @Test
    fun windowedFramesReplaceStateWholesaleAndCarryLoadFacts() {
        val a = ffiRow("a", 300uL)
        val b = ffiRow("b", 200uL)
        val c = ffiRow("c", 400uL)

        // A windowed frame is authoritative: `windowRowBatch` maps rows from
        // `frame.window.rows` alone, never a fold, so the row set changes
        // entirely while `deltas` stays empty -- and even a CONTRADICTORY wire
        // delta must be ignored on this arm.
        val first = windowRowBatch(windowedFrame(rows = listOf(a, b)))
        val second =
            windowRowBatch(
                windowedFrame(rows = listOf(c, a), load = FfiWindowLoad.Returned(1uL)),
            )
        val third =
            windowRowBatch(
                windowedFrame(
                    rows = listOf(c),
                    load = FfiWindowLoad.AtBound(2uL),
                    deltas = listOf(FfiRowDelta.Removed("c")),
                ),
            )

        assertEquals(listOf("a", "b"), first.rows.map(Row::id))
        assertEquals(WindowLoad.Idle, first.load)
        assertEquals(listOf("c", "a"), second.rows.map(Row::id))
        assertEquals(WindowLoad.Returned(1uL), second.load)
        assertEquals(listOf("c"), third.rows.map(Row::id))
        // AtBound arrives as a FACT in the frame, never a thrown error.
        assertEquals(WindowLoad.AtBound(2uL), third.load)
    }

    @Test
    fun unboundedObservationsStillFoldEveryDeltaInOrder() {
        // The unbounded arm of the one frame vocabulary keeps today's exact
        // fold semantics: `applyRowDelta` is the same accumulator step the
        // `observeQuery` pull loop runs per `frame.deltas` element.
        val order = mutableListOf<String>()
        val byId = mutableMapOf<String, Row>()
        val a = ffiRow("a", 300uL)
        val b = ffiRow("b", 200uL)
        val c = ffiRow("c", 400uL)

        applyRowDelta(order, byId, FfiRowDelta.Added(a))
        applyRowDelta(order, byId, FfiRowDelta.Added(b))
        applyRowDelta(order, byId, FfiRowDelta.Removed("a"))
        applyRowDelta(order, byId, FfiRowDelta.Added(c))
        applyRowDelta(
            order,
            byId,
            FfiRowDelta.SourcesGrew("b", listOf("wss://window.example", "wss://grew.example")),
        )

        assertEquals(listOf("b", "c"), order)
        assertEquals(
            listOf("wss://window.example", "wss://grew.example"),
            byId["b"]?.sources,
        )
    }

    @Test
    fun rowBatchLoadIsNullForUnboundedObservations() {
        val unbounded = RowBatch(emptyList(), AcquisitionEvidence(emptyList(), emptyList()))
        assertNull(unbounded.load)
    }

    @Test
    fun everyRequestRowsFailureKeepsItsTypedAxis() {
        assertEquals(
            NMPRequestRowsError.Unwindowed,
            NMPRequestRowsError.from(FfiRequestRowsException.Unwindowed()),
        )
        assertEquals(
            NMPRequestRowsError.EngineClosed,
            NMPRequestRowsError.from(FfiRequestRowsException.EngineClosed()),
        )
        assertEquals(
            NMPRequestRowsError.StoreUnavailable,
            NMPRequestRowsError.from(FfiRequestRowsException.StoreUnavailable()),
        )
        assertEquals(
            NMPRequestRowsError.TransportUnavailable("offline"),
            NMPRequestRowsError.from(FfiRequestRowsException.TransportUnavailable("offline")),
        )
    }

    @Test
    fun validationAndExplicitCancellationTearDownWindowedObservation() = runBlocking {
        // #680: opening a windowed observation costs no native-task admission
        // and there is no capacity config -- the only failures here are the
        // typed window-validation errors, and explicit `cancel()` deterministically
        // completes the collecting coroutine.
        val engine = NMPEngine(NMPConfig())
        try {
            val demand = windowDemand(7_779u)
            assertFailsWith<NMPError.WindowZeroRows> {
                engine.observe(demand, Window.Expandable(initial = 0uL, max = 2uL))
            }
            assertFailsWith<NMPError.WindowZeroRows> {
                engine.observe(demand, Window.Expandable(initial = 1uL, max = 0uL))
            }
            assertFailsWith<NMPError.WindowInitialExceedsMax> {
                engine.observe(demand, Window.Expandable(initial = 3uL, max = 2uL))
            }
            assertFailsWith<NMPError.WindowSelectionHasLimit> {
                engine.observe(
                    demand.copy(selection = demand.selection.copy(limit = 1u)),
                    Window.Expandable(initial = 1uL, max = 2uL),
                )
            }

            val query = engine.observe(demand, Window.Expandable(initial = 1uL, max = 2uL))
            val first = CompletableDeferred<RowBatch>()
            val collection = launch {
                query.frames.collect { batch -> first.complete(batch) }
            }
            val batch = withTimeout(5_000) { first.await() }
            assertEquals(emptyList(), batch.rows)
            assertEquals(WindowLoad.Idle, batch.load)

            // Explicit cancel wakes the parked `next()` to its terminal null,
            // completing the flow -- the collecting coroutine joins promptly.
            query.cancel()
            withTimeout(5_000) { collection.join() }
        } finally {
            engine.shutdown()
        }
    }

    @Test
    fun engineShutdownClosesWindowedCollectionWithinBound() = runBlocking {
        val engine = NMPEngine(NMPConfig())
        val query =
            engine.observe(windowDemand(7_780u), Window.Expandable(initial = 1uL, max = 2uL))
        val first = CompletableDeferred<Unit>()
        val collection = launch {
            query.frames.collect { first.complete(Unit) }
        }
        withTimeout(5_000) { first.await() }

        engine.shutdown()
        withTimeout(5_000) { collection.join() }
    }

    private fun windowDemand(kind: UShort) =
        NMPDemand(selection = NMPFilter(kinds = listOf(kind)), source = NMPSourceAuthority.Public)

    private fun windowedFrame(
        rows: List<FfiRow>,
        load: FfiWindowLoad = FfiWindowLoad.Idle,
        deltas: List<FfiRowDelta> = emptyList(),
    ) =
        FfiFrame(
            deltas = deltas,
            window = FfiWindowContents(rows = rows, load = load),
            evidence = emptyEvidence,
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
            sources = listOf("wss://window.example"),
        )
}
