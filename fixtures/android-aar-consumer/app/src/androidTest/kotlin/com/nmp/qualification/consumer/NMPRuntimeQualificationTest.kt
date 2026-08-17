package com.nmp.qualification.consumer

import android.content.Context
import android.content.Intent
import android.os.Debug
import android.os.Process
import android.util.Log
import android.view.Choreographer
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.nmp.sdk.NMPAccessContext
import com.nmp.sdk.NMPCacheMode
import com.nmp.sdk.NMPConfig
import com.nmp.sdk.NMPDemand
import com.nmp.sdk.NMPEngine
import com.nmp.sdk.NMPError
import com.nmp.sdk.NMPFilter
import com.nmp.sdk.NMPFreshness
import com.nmp.sdk.NMPReadRouting
import com.nmp.sdk.RowBatch
import com.nmp.sdk.SourceStatus
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeFalse
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import kotlin.coroutines.resume
import kotlin.math.ceil
import kotlin.system.measureNanoTime

@RunWith(AndroidJUnit4::class)
class NMPRuntimeQualificationTest {
    private val successRelay = BuildConfig.NMP_QUALIFICATION_RELAY
    private val recoveryRelay = BuildConfig.NMP_QUALIFICATION_RECOVERY_RELAY
    private val offlineRelay = BuildConfig.NMP_QUALIFICATION_OFFLINE_RELAY

    @Test
    fun coldSeedUsesPublicFacadeAndAndroidStorage(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val context = ApplicationProvider.getApplicationContext<Context>()
        val store = persistentStore(context)
        if (store.exists()) NMPEngine.resetPersistentStore(store.absolutePath)

        val activity =
            InstrumentationRegistry.getInstrumentation().startActivitySync(
                Intent(context, QualificationActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        assertTrue(activity is QualificationActivity)
        activity.finish()

        lateinit var engine: NMPEngine
        lateinit var online: RowBatch
        var collector: Job? = null
        val matchingBatches = Channel<RowBatch>(1)
        val coldNanos =
            try {
                measureNanoTime {
                    engine = NMPEngine(config(store))
                    collector =
                        launch(Dispatchers.Default) {
                            engine.observe(demand(successRelay)).collect { batch ->
                                if (batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT } &&
                                    batch.sources().any { sameRelay(it.relay, successRelay) }
                                ) {
                                    matchingBatches.trySend(batch)
                                }
                            }
                        }
                    online = withTimeout(COLD_LIVE_LIMIT_MS) { matchingBatches.receive() }
                }
            } finally {
                withTimeout(COLLECTOR_CANCEL_LIMIT_MS) { collector?.cancelAndJoin() }
            }
        val coldMs = coldNanos / 1_000_000
        assertTrue("cold live first row took ${coldMs}ms", coldMs <= COLD_LIVE_LIMIT_MS)
        assertTrue(online.rows.any { it.content == CONTROLLED_EVENT_CONTENT })
        // Cancellation withdraws demand synchronously from the Kotlin Flow,
        // while the engine-owned transport writes CLOSE on its worker. Keep
        // the engine alive for one bounded flush window; the host relay
        // transcript independently requires the exact CLOSE for this run.
        delay(WIRE_WITHDRAWAL_FLUSH_MS)
        engine.close()
        engine.close()
        assertEngineClosedFromKotlin(engine)
        assertTrue("Java did not receive typed EngineClosed", QualificationJava.postCloseIsEngineClosed(engine))

        Log.i(
            TAG,
            "NMP_ANDROID_COLD_SEED pid=${Process.myPid()} cold_live_ms=$coldMs " +
                "rows=${online.rows.size} native_backed_call=controlled_row " +
                "store=${store.absolutePath}",
        )
    }

    @Test
    fun freshProcessReopensCacheAndMeetsCacheLatency(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val context = ApplicationProvider.getApplicationContext<Context>()
        val store = persistentStore(context)
        assertTrue("seed store missing before process restart", store.exists())
        val engine = NMPEngine(config(store))
        val timings =
            (0 until CACHE_SAMPLES).map {
                var cached: RowBatch? = null
                val nanos =
                    measureNanoTime {
                        cached =
                            withTimeout(2_000) {
                                engine.observe(
                                    demand(successRelay).copy(freshness = NMPFreshness.CacheOnly),
                                ).first { batch ->
                                    batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }
                                }
                            }
                    }
                assertEquals(
                    CONTROLLED_EVENT_CONTENT,
                    cached!!.rows.single { it.content == CONTROLLED_EVENT_CONTENT }.content,
                )
                nanos / 1_000_000.0
            }
        val p95 = percentile(timings, 0.95)
        engine.close()
        assertTrue("cache-only p95 was ${p95}ms", p95 <= CACHE_P95_LIMIT_MS)
        Log.i(
            TAG,
            "NMP_ANDROID_REOPENED pid=${Process.myPid()} cache_samples=$CACHE_SAMPLES " +
                "cache_p95_ms=${"%.3f".format(p95)}",
        )
    }

    @Test
    fun preconnectFailureRecoversToRealRow(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val engine = NMPEngine(config())
        var sawError = false
        val recovered =
            withTimeout(30_000) {
                engine.observe(demand(recoveryRelay)).first { batch ->
                    val status = batch.statusFor(recoveryRelay)
                    if (status is SourceStatus.Error) sawError = true
                    sawError && batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }
                }
            }
        assertTrue("recovery row arrived without scoped pre-connect Error", sawError)
        assertTrue(recovered.rows.any { it.content == CONTROLLED_EVENT_CONTENT })
        engine.close()
        Log.i(TAG, "NMP_ANDROID_RECOVERED pid=${Process.myPid()} error_before_row=true")
    }

    @Test
    fun offlineFailureIsScopedAndJavaReadable(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val engine = NMPEngine(config())
        val failed =
            withTimeout(20_000) {
                engine.observe(demand(offlineRelay)).first { batch ->
                    batch.statusFor(offlineRelay) is SourceStatus.Error
                }
            }
        assertTrue(failed.rows.isEmpty())
        assertTrue("Java facade could not recognize SourceStatus.Error", QualificationJava.recognizesScopedError(failed))
        engine.close()
        Log.i(TAG, "NMP_ANDROID_OFFLINE pid=${Process.myPid()} scoped_error=true")
    }

    @Test
    fun cancellationBeforeRequiredRowIsBounded(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val engine = NMPEngine(config())
        val collection =
            launch(Dispatchers.Default, start = CoroutineStart.UNDISPATCHED) {
                engine.observe(demand(offlineRelay)).first { batch ->
                    assertTrue(batch.rows.isEmpty())
                    false
                }
            }
        assertTrue("collector completed before explicit cancellation", collection.isActive)
        withTimeout(5_000) { collection.cancelAndJoin() }
        engine.close()
        assertEngineClosedFromKotlin(engine)
        Log.i(TAG, "NMP_ANDROID_CANCELLED pid=${Process.myPid()} bounded=true")
    }

    @Test
    fun collectorIdleAndTeardownPerformanceContract(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val baselineThreads = taskCount()
        val baselineHeap = Debug.getNativeHeapAllocatedSize()
        val engine = NMPEngine(config())
        val ready = Channel<Unit>(COLLECTOR_COUNT)
        val jobs = mutableListOf<Job>()

        jobs += persistentCollector(engine, ready)
        withTimeout(5_000) { ready.receive() }
        val oneCollectorThreads = taskCount()
        val oneCollectorHeap = Debug.getNativeHeapAllocatedSize()
        repeat(COLLECTOR_COUNT - 1) { jobs += persistentCollector(engine, ready) }
        withTimeout(15_000) { repeat(COLLECTOR_COUNT - 1) { ready.receive() } }
        val manyCollectorThreads = taskCount()
        val manyCollectorHeap = Debug.getNativeHeapAllocatedSize()
        val threadDelta = manyCollectorThreads - oneCollectorThreads
        val heapDelta = manyCollectorHeap - oneCollectorHeap
        assertTrue("64 collectors added $threadDelta threads", threadDelta <= COLLECTOR_THREAD_DELTA)
        assertTrue("64 collectors added $heapDelta native bytes", heapDelta <= NATIVE_HEAP_DELTA_BYTES)

        val cpuBeforeMs = Process.getElapsedCpuTime()
        val frameIntervalsMs = collectFrameIntervals(FRAME_COUNT)
        val cpuMs = Process.getElapsedCpuTime() - cpuBeforeMs
        val frameP99Ms = percentile(frameIntervalsMs, 0.99)
        assertTrue("idle engine consumed ${cpuMs}ms process CPU", cpuMs <= IDLE_CPU_LIMIT_MS)
        assertTrue("main dispatch p99 was ${frameP99Ms}ms", frameP99Ms < DISPATCH_P99_LIMIT_MS)

        jobs.forEach { it.cancel() }
        withTimeout(10_000) { jobs.forEach { it.join() } }
        engine.close()
        repeat(TEARDOWN_CYCLES) {
            val cycleEngine = NMPEngine(config())
            val cycleReady = Channel<Unit>(1)
            val cycleCollector = persistentCollector(cycleEngine, cycleReady)
            withTimeout(5_000) { cycleReady.receive() }
            withTimeout(5_000) { cycleCollector.cancelAndJoin() }
            cycleEngine.close()
        }
        val teardown = awaitTeardown(baselineThreads, baselineHeap)
        assertTrue("teardown left ${teardown.first - baselineThreads} threads", teardown.first <= baselineThreads + TEARDOWN_THREAD_DELTA)
        assertTrue("teardown left ${teardown.second - baselineHeap} native bytes", teardown.second <= baselineHeap + NATIVE_HEAP_DELTA_BYTES)

        Log.i(
            TAG,
            "NMP_ANDROID_PERFORMANCE pid=${Process.myPid()} collectors=$COLLECTOR_COUNT " +
                "thread_delta=$threadDelta native_heap_delta_bytes=$heapDelta " +
                "idle_cpu_ms=$cpuMs dispatch_p99_ms=${"%.3f".format(frameP99Ms)} " +
                "teardown_cycles=$TEARDOWN_CYCLES teardown_threads=${teardown.first} " +
                "teardown_heap_bytes=${teardown.second}",
        )
    }

    @Test
    fun missingEmulatorAbiFailsAtNativeConstruction() {
        assumeFalse(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val failure = runCatching { NMPEngine(NMPConfig()) }.exceptionOrNull()
        assertNotNull("missing-x86_64 AAR unexpectedly constructed NMPEngine", failure)
        assertTrue(
            "wrong-ABI failure must be a native load/link refusal, got $failure",
            generateSequence(failure) { it.cause }.any { it is UnsatisfiedLinkError },
        )
        Log.i(TAG, "NMP_ANDROID_WRONG_ABI_REFUSED type=${failure!!::class.java.name}")
    }

    private fun persistentCollector(
        engine: NMPEngine,
        ready: Channel<Unit>,
    ): Job =
        kotlinx.coroutines.CoroutineScope(Dispatchers.Default).launch {
            var announced = false
            engine.observe(demand(offlineRelay)).collect {
                if (!announced) {
                    announced = true
                    ready.send(Unit)
                }
            }
        }

    private suspend fun collectFrameIntervals(count: Int): List<Double> =
        withContext(Dispatchers.Main) {
            suspendCancellableCoroutine { continuation ->
                val times = ArrayList<Long>(count)
                val choreographer = Choreographer.getInstance()
                val callback =
                    object : Choreographer.FrameCallback {
                        override fun doFrame(frameTimeNanos: Long) {
                            times += frameTimeNanos
                            if (times.size == count) {
                                continuation.resume(
                                    times.zipWithNext { left, right ->
                                        (right - left) / 1_000_000.0
                                    },
                                )
                            } else {
                                choreographer.postFrameCallback(this)
                            }
                        }
                    }
                continuation.invokeOnCancellation { choreographer.removeFrameCallback(callback) }
                choreographer.postFrameCallback(callback)
            }
        }

    private suspend fun awaitTeardown(
        baselineThreads: Int,
        baselineHeap: Long,
    ): Pair<Int, Long> {
        var current = taskCount() to Debug.getNativeHeapAllocatedSize()
        repeat(100) {
            if (current.first <= baselineThreads + TEARDOWN_THREAD_DELTA &&
                current.second <= baselineHeap + NATIVE_HEAP_DELTA_BYTES
            ) {
                return current
            }
            delay(50)
            current = taskCount() to Debug.getNativeHeapAllocatedSize()
        }
        return current
    }

    private fun taskCount(): Int = File("/proc/self/task").list()?.size ?: error("no /proc/self/task")

    private fun config(store: File? = null): NMPConfig =
        NMPConfig(
            storePath = store?.absolutePath,
            maxRelays = 2u,
        )

    private fun demand(relay: String): NMPDemand =
        NMPDemand(
            selection = NMPFilter(kinds = listOf(1u.toUShort())),
            routing = NMPReadRouting.Explicit(listOf(relay)),
            access = NMPAccessContext.Public,
            cache = NMPCacheMode.Strict,
            freshness = NMPFreshness.Live,
        )

    private fun RowBatch.sources() = evidence.flatMap { it.sources }

    private fun RowBatch.statusFor(relay: String): SourceStatus? =
        sources().singleOrNull { sameRelay(it.relay, relay) }?.status

    private fun persistentStore(context: Context) =
        File(context.noBackupFilesDir, "nmp-runtime-qualification.redb")

    private fun assertEngineClosedFromKotlin(engine: NMPEngine) {
        val failure = runCatching { engine.session.current }.exceptionOrNull()
        assertTrue("closed engine still accepted a synchronous verb: $failure", failure is NMPError.EngineClosed)
    }

    private fun percentile(values: List<Double>, quantile: Double): Double {
        require(values.isNotEmpty())
        val sorted = values.sorted()
        val index = (ceil(sorted.size * quantile).toInt() - 1).coerceIn(sorted.indices)
        return sorted[index]
    }

    private fun sameRelay(left: String, right: String): Boolean =
        left.trimEnd('/') == right.trimEnd('/')

    private companion object {
        const val TAG = "NMPQualification"
        const val CONTROLLED_EVENT_CONTENT = "nmp-android-controlled-relay"
        const val COLD_LIVE_LIMIT_MS = 3_000L
        const val COLLECTOR_CANCEL_LIMIT_MS = 5_000L
        const val WIRE_WITHDRAWAL_FLUSH_MS = 500L
        const val CACHE_SAMPLES = 50
        const val CACHE_P95_LIMIT_MS = 100.0
        const val COLLECTOR_COUNT = 64
        const val COLLECTOR_THREAD_DELTA = 4
        const val FRAME_COUNT = 120
        const val IDLE_CPU_LIMIT_MS = 500L
        const val DISPATCH_P99_LIMIT_MS = 250.0
        const val TEARDOWN_THREAD_DELTA = 4
        const val TEARDOWN_CYCLES = 10
        const val NATIVE_HEAP_DELTA_BYTES = 16L * 1024L * 1024L
    }
}
