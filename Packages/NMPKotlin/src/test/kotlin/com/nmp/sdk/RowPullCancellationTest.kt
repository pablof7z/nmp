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
import kotlinx.coroutines.flow.collect
import uniffi.nmp_ffi.FfiAcquisitionEvidence
import uniffi.nmp_ffi.FfiBlobDescriptor
import uniffi.nmp_ffi.FfiAuthPolicyCallback
import uniffi.nmp_ffi.FfiAuthPolicyRegistration
import uniffi.nmp_ffi.FfiCancelWriteOutcome
import uniffi.nmp_ffi.FfiFrame
import uniffi.nmp_ffi.FfiLiveQuery
import uniffi.nmp_ffi.FfiPublishQueueEntry
import uniffi.nmp_ffi.FfiPrivateKey
import uniffi.nmp_ffi.FfiPublicKey
import uniffi.nmp_ffi.FfiReceiptReattachment
import uniffi.nmp_ffi.FfiRelayInformation
import uniffi.nmp_ffi.FfiRelayInformationCachePolicy
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiRowDelta
import uniffi.nmp_ffi.FfiRowSignature
import uniffi.nmp_ffi.FfiRowPullException
import uniffi.nmp_ffi.FfiSessionAccount
import uniffi.nmp_ffi.FfiSessionPayload
import uniffi.nmp_ffi.FfiSessionSnapshot
import uniffi.nmp_ffi.FfiSignEventRequest
import uniffi.nmp_ffi.FfiWindow
import uniffi.nmp_ffi.FfiWriteIntent
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.NmpDiagnosticsStream
import uniffi.nmp_ffi.NmpFollowStream
import uniffi.nmp_ffi.NmpReceiptStream
import uniffi.nmp_ffi.NmpRowPull
import uniffi.nmp_ffi.NmpRowStream
import uniffi.nmp_ffi.NmpSignEventHandle
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
     * commits (acknowledges) a frame before returning it, but `Query.kt`'s
     * private pull loop, reached here through the public [observeQuery]
     * door, still applies/folds the delta and `emit`s it afterward. If the
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
                    signature = FfiRowSignature.Signed("sig"),
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
            val engine = ScriptedRowFlowEngine(stream)
            val dispatcher = ManualDispatcher()
            val collected = mutableListOf<RowBatch>()

            lateinit var job: Job
            job =
                launch(dispatcher) {
                    try {
                        // The public entry point, not the private pull loop
                        // directly: `Query.kt`'s `rowFlow` is deliberately
                        // not exposed even as `internal` -- touching it for
                        // testability would land inside the governed
                        // `kotlin_sources` component path for no real surface
                        // reason. Driving through `observeQuery` exercises
                        // the exact same commit-then-fold-then-emit loop
                        // through the one door an app actually uses.
                        observeQuery(
                            engine,
                            NMPLiveQuery.single(
                                NMPDemand(
                                    selection = NMPFilter(),
                                )
                            ),
                        ).collect { collected.add(it) }
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

    /** A minimal [NmpEngineInterface] whose only live behaviour is
     * `observe`, so [cancellingAfterCommitButBeforeEmitWithdrawsTheWholeObservation]
     * can drive the real public [observeQuery] door instead of reaching into
     * `Query.kt`'s private pull loop. Every other member is unreachable from
     * that one collection and fails loudly if that ever stops being true. */
    private class ScriptedRowFlowEngine(
        private val stream: NmpRowStream,
    ) : NmpEngineInterface {
        override fun `addGroupToList`(
            `groupId`: String,
            `hostRelay`: String,
            `name`: String?,
        ): NmpReceiptStream = unusedByThisFalsifier()

        override fun `addPrivateKeyAccount`(
            `privateKey`: FfiPrivateKey,
            `makeCurrent`: Boolean,
        ): FfiSessionAccount = unusedByThisFalsifier()

        override fun `addPublicKeyAccount`(
            `publicKey`: FfiPublicKey,
            `makeCurrent`: Boolean,
        ): FfiSessionAccount = unusedByThisFalsifier()

        override fun `addAuthPolicy`(
            `expectedPublicKey`: String,
            `callback`: FfiAuthPolicyCallback,
        ): FfiAuthPolicyRegistration = unusedByThisFalsifier()

        override fun `addRelayInUse`(`relay`: String): NmpReceiptStream = unusedByThisFalsifier()

        override fun `cancel`(`receiptId`: ULong): FfiCancelWriteOutcome = unusedByThisFalsifier()

        override fun `clearSession`(): Unit = unusedByThisFalsifier()

        override fun `exportSession`(): FfiSessionPayload = unusedByThisFalsifier()

        override fun `follow`(`target`: String): NmpReceiptStream = unusedByThisFalsifier()

        override fun `makeCurrentAccount`(`account`: FfiSessionAccount): Unit =
            unusedByThisFalsifier()

        override fun `observe`(`query`: FfiLiveQuery, `window`: FfiWindow?): NmpRowStream = stream

        override fun `observeDiagnostics`(): NmpDiagnosticsStream = unusedByThisFalsifier()

        override fun `observeFollowing`(`target`: String): NmpFollowStream = unusedByThisFalsifier()

        override fun `publish`(`intent`: FfiWriteIntent): NmpReceiptStream = unusedByThisFalsifier()

        override fun `publishQueue`(
            `afterReceiptId`: ULong?,
            `limit`: UByte,
        ): List<FfiPublishQueueEntry> = unusedByThisFalsifier()

        override fun `publishQueueForEvent`(
            `eventId`: String,
            `afterReceiptId`: ULong?,
            `limit`: UByte,
        ): List<FfiPublishQueueEntry> = unusedByThisFalsifier()

        override fun `removePublishQueueEntry`(`receiptId`: ULong): Unit = unusedByThisFalsifier()

        override fun `reattachReceipt`(`receiptId`: ULong): FfiReceiptReattachment = unusedByThisFalsifier()

        override suspend fun `relayInformation`(
            `relay`: String,
            `policy`: FfiRelayInformationCachePolicy,
        ): FfiRelayInformation = unusedByThisFalsifier()

        override suspend fun `uploadBlossom`(
            `serverUrl`: String,
            `blob`: ByteArray,
            `contentType`: String,
            `description`: String,
        ): FfiBlobDescriptor = unusedByThisFalsifier()

        override fun `removeAccount`(`account`: FfiSessionAccount): Boolean =
            unusedByThisFalsifier()

        override fun `removeGroupFromList`(
            `groupId`: String,
            `hostRelay`: String,
        ): NmpReceiptStream = unusedByThisFalsifier()

        override fun `removeAuthPolicy`(`registration`: FfiAuthPolicyRegistration): Boolean =
            unusedByThisFalsifier()

        override fun `removeRelayInUse`(`relay`: String): NmpReceiptStream = unusedByThisFalsifier()

        override fun `session`(): FfiSessionSnapshot = unusedByThisFalsifier()

        override fun `shutdown`(): Unit = unusedByThisFalsifier()

        override fun `signEvent`(`event`: FfiSignEventRequest): NmpSignEventHandle = unusedByThisFalsifier()

        override fun `unfollow`(`target`: String): NmpReceiptStream = unusedByThisFalsifier()

        private fun unusedByThisFalsifier(): Nothing =
            error("ScriptedRowFlowEngine only scripts observe(); this falsifier never reaches anything else")
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
