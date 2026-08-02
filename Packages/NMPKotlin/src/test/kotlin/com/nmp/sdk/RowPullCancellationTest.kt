package com.nmp.sdk

import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger
import kotlin.coroutines.CoroutineContext
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiFrame
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiRowDelta
import uniffi.nmp_ffi.FfiRowPullException
import uniffi.nmp_ffi.NmpRowPull
import uniffi.nmp_ffi.NmpRowStream
import uniffi.nmp_ffi.NoPointer
import uniffi.nmp_ffi.UNIFFI_RUST_FUTURE_POLL_READY
import uniffi.nmp_ffi.UniffiRustFutureContinuationCallback
import uniffi.nmp_ffi.uniffiRustCallAsync

class RowPullCancellationTest {
    /**
     * #762: drive the generated UniFFI 0.29.5 async helper through its exact
     * READY-before-complete seam. READY queues the cancellable continuation;
     * cancellation then wins before generated completion can retrieve the
     * result. Generated code must free the Rust future, and the SDK wrapper
     * must still own and abort its synchronously-created pull ticket.
     *
     * Real native replay, successor composition, repeated cancellation
     * bounds, and commit/abort/cancel races are covered in
     * `nmp-ffi/tests/row_pull_cancellation.rs`; this test is the generated
     * Kotlin side of the same cross-boundary falsifier.
     */
    @Test
    fun generatedReadyThenCancellationFreesFutureAndAbortsTicketBeforeCompletion() =
        runBlocking<Unit> {
            val generated = GeneratedReadySeam()
            val pull = FakePull(generated)
            val stream = FakeStream(pull)
            val dispatcher = ManualDispatcher()
            val escapedFrame = CompletableDeferred<FfiFrame?>()
            val job =
                launch(dispatcher) {
                    escapedFrame.complete(nextCommittedRowFrame(stream))
                }

            // Enter generated `uniffiRustCallAsync` and suspend in its poll.
            assertTrue(dispatcher.runNext(), "pull coroutine entered generated receive")
            assertTrue(job.isActive)
            assertEquals(1, stream.beginCount.get(), "ticket existed before the cancellable await")
            assertTrue(generated.hasPendingPoll(), "generated poll continuation is parked")

            // Rust reports READY. Resumption is queued, so generated complete
            // has not yet retrieved the result and free has not run.
            generated.reportReady()
            assertEquals(1, dispatcher.queuedCount())
            assertEquals(0, generated.completeCount.get())
            assertEquals(0, generated.freeCount.get())
            assertEquals(0, pull.commitCount.get())
            assertEquals(0, pull.abortCount.get())
            assertFalse(escapedFrame.isCompleted)

            // Prompt cancellation wins when the queued READY continuation
            // runs: generated finally frees first, then the SDK finally aborts.
            job.cancel()
            dispatcher.drain()
            withTimeout(5_000) { job.join() }

            assertTrue(job.isCancelled)
            assertFalse(escapedFrame.isCompleted)
            assertEquals(0, generated.completeCount.get(), "completion never retrieved the frame")
            assertEquals(1, generated.freeCount.get(), "generated cancellation freed the Rust future")
            assertEquals(0, pull.commitCount.get(), "an unseen frame was never committed")
            assertEquals(1, pull.abortCount.get(), "the still-owned ticket was rolled back exactly once")
            assertEquals(
                listOf("free", "abort"),
                generated.terminalOrder,
                "generated free completed before the SDK settled its ticket",
            )
        }

    /**
     * #1192: the second half of #762's guarantee. [nextCommittedRowFrame]
     * commits (acknowledges) a frame before returning it, but [rowFlow]'s own
     * pull loop still applies/folds the delta and `emit`s it afterward. If the
     * collecting coroutine is cancelled in that window --
     * after commit, before emit -- the acknowledged transition must not just
     * disappear: `finally { handle.cancel() }` must withdraw the whole
     * observation, so no later pull ever continues from a step the collector
     * never received.
     *
     * This is not a timing race. [AcknowledgeThenCancelPull.commit] cancels
     * the collecting [Job] itself, synchronously, from inside the exact call
     * that acknowledges the row -- deterministic ordering, not a scheduling
     * claim. [ManualDispatcher] guarantees the [Job] reference is captured
     * before any of its body runs, so there is no window in which `commit()`
     * could observe a not-yet-stored job.
     */
    @Test
    fun cancellingAfterCommitButBeforeEmitWithdrawsTheWholeObservation() =
        runBlocking<Unit> {
            val row =
                FfiRow(
                    id = "row-1",
                    pubkey = "pk",
                    createdAt = 1uL,
                    kind = 1u,
                    tags = emptyList(),
                    content = "ticketed",
                    sig = "sig",
                    sources = listOf("wss://cancel-window.example"),
                )
            val frame =
                FfiFrame(
                    deltas = listOf(FfiRowDelta.Added(row)),
                    window = null,
                    evidence = listOf(FfiAcquisitionEvidence(emptyList(), emptyList())),
                )
            val pull = AcknowledgeThenCancelPull(frame)
            val stream = AcknowledgeThenCancelStream(pull)
            val dispatcher = ManualDispatcher()
            val collected = mutableListOf<RowBatch>()

            lateinit var job: Job
            job =
                launch(dispatcher) {
                    try {
                        rowFlow { stream }.collect { collected.add(it) }
                    } catch (_: CancellationException) {
                        // Expected: cancellation lands after acknowledgement,
                        // before this collector ever sees the row.
                    }
                }
            pull.onCommit = { job.cancel() }

            dispatcher.drain()
            withTimeout(5_000) { job.join() }

            assertTrue(job.isCancelled, "the collecting coroutine was cancelled")
            assertEquals(
                1,
                pull.commitCount.get(),
                "the frame was acknowledged before cancellation could be observed",
            )
            assertEquals(
                1,
                stream.cancelCount.get(),
                "cancellation after acknowledgement withdrew the whole observation exactly once",
            )
            assertTrue(
                collected.isEmpty(),
                "the acknowledged-but-unapplied frame was never delivered to the collector",
            )
            assertEquals(
                1,
                stream.beginCount.get(),
                "no later pull began on the withdrawn observation",
            )
        }

    /** Always acknowledges [frame], then runs [onCommit] -- the SDK's own
     * commit-before-anything-else ordering, with a hook for the test to
     * trigger cancellation from inside the acknowledgement itself. */
    private class AcknowledgeThenCancelPull(
        private val frame: FfiFrame,
    ) : NmpRowPull(NoPointer) {
        val commitCount = AtomicInteger()
        val abortCount = AtomicInteger()
        var onCommit: () -> Unit = {}

        override suspend fun receive(): FfiFrame? = frame

        override fun commit() {
            commitCount.incrementAndGet()
            onCommit()
        }

        override fun abort() {
            abortCount.incrementAndGet()
        }
    }

    private class AcknowledgeThenCancelStream(
        private val pull: NmpRowPull,
    ) : NmpRowStream(NoPointer) {
        val beginCount = AtomicInteger()
        val cancelCount = AtomicInteger()

        override fun beginNext(): NmpRowPull {
            beginCount.incrementAndGet()
            return pull
        }

        override fun cancel() {
            cancelCount.incrementAndGet()
        }

        override fun requestRows(atLeast: ULong) = Unit
    }

    private class GeneratedReadySeam {
        val completeCount = AtomicInteger()
        val freeCount = AtomicInteger()
        val terminalOrder = CopyOnWriteArrayList<String>()

        private lateinit var callback: UniffiRustFutureContinuationCallback
        private var continuation: Long? = null

        fun hasPendingPoll(): Boolean = continuation != null

        fun reportReady() {
            val pending = checkNotNull(continuation)
            continuation = null
            callback.callback(pending, UNIFFI_RUST_FUTURE_POLL_READY)
        }

        suspend fun receive(): FfiFrame? =
            uniffiRustCallAsync(
                rustFuture = 762L,
                pollFunc = { _, registeredCallback, registeredContinuation ->
                    callback = registeredCallback
                    continuation = registeredContinuation
                },
                completeFunc = { _, _ ->
                    completeCount.incrementAndGet()
                    Unit
                },
                freeFunc = {
                    freeCount.incrementAndGet()
                    terminalOrder.add("free")
                },
                liftFunc = { null },
                errorHandler = FfiRowPullException.ErrorHandler,
            )
    }

    private class FakePull(
        private val generated: GeneratedReadySeam,
    ) : NmpRowPull(NoPointer) {
        val commitCount = AtomicInteger()
        val abortCount = AtomicInteger()

        override suspend fun receive(): FfiFrame? = generated.receive()

        override fun commit() {
            commitCount.incrementAndGet()
        }

        override fun abort() {
            abortCount.incrementAndGet()
            generated.terminalOrder.add("abort")
        }
    }

    private class FakeStream(
        private val pull: NmpRowPull,
    ) : NmpRowStream(NoPointer) {
        val beginCount = AtomicInteger()

        override fun beginNext(): NmpRowPull {
            beginCount.incrementAndGet()
            return pull
        }

        override fun cancel() = Unit

        override fun requestRows(atLeast: ULong) = Unit
    }

    private class ManualDispatcher : CoroutineDispatcher() {
        private val queued = ConcurrentLinkedQueue<Runnable>()

        override fun dispatch(context: CoroutineContext, block: Runnable) {
            queued.add(block)
        }

        fun queuedCount(): Int = queued.size

        fun runNext(): Boolean {
            val next = queued.poll() ?: return false
            next.run()
            return true
        }

        fun drain() {
            while (runNext()) {
                // Drain every cancellation/completion continuation.
            }
        }
    }
}
