package com.nmp.sdk

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import java.util.concurrent.atomic.AtomicInteger
import kotlin.system.measureTimeMillis
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertTrue

class DiagnosticsConcurrencyTest {
    private companion object {
        // The well-known low secret key used by nmp-ffi's own oracles;
        // activating it roots identity so the initial current-state frame is
        // delivered deterministically.
        const val TEST_SECRET_KEY =
            "0000000000000000000000000000000000000000000000000000000000000001"
    }

    // MARK: - Item 3: cancel while a pull is genuinely PARKED

    /**
     * #680 Item 3 (explicit teardown path): a live windowed observation with a
     * genuinely PARKED pull (initial `Idle` snapshot already delivered, no data
     * flowing) must be woken by the EXPLICIT `NMPQuery.cancel()`. Proves:
     *   (a) `cancel()` wakes the parked `next()` IMMEDIATELY (join bounded well
     *       under 1s -- a Rust-side wake, not a wrapper timeout);
     *   (b) NO post-cancel frame is delivered (the frame count is frozen after
     *       cancel);
     *   (c) `cancel()` is idempotent (two more calls are safe no-ops);
     *   (d) the wake is via the EXPLICIT `cancel()`, NOT the JVM `Cleaner`/GC --
     *       the query is held live for the whole test.
     */
    @Test
    fun explicitCancelWakesParkedNextWithinBoundNoPostCancelFrameIdempotent() =
        runBlocking<Unit> {
            val engine = NMPEngine(NMPConfig())
            try {
                val registration = engine.addAccount(TEST_SECRET_KEY)
                engine.setActiveAccount(registration.publicKey)
                val demand =
                    NMPDemand(
                        selection = NMPFilter(kinds = listOf(8_811u)),
                        source = NMPSourceAuthority.Public,
                    )
                val query = engine.observe(demand, Window.Expandable(initial = 1uL, max = 2uL))

                val frameCount = AtomicInteger(0)
                val firstBatch = CompletableDeferred<RowBatch>()
                val collection =
                    launch {
                        query.frames.collect { batch ->
                            frameCount.incrementAndGet()
                            if (!firstBatch.isCompleted) firstBatch.complete(batch)
                        }
                    }

                // The initial authoritative snapshot: an empty, idle window.
                val initial = withTimeout(5_000) { firstBatch.await() }
                assertEquals(emptyList(), initial.rows)
                assertEquals(WindowLoad.Idle, initial.load)

                // Let the pull genuinely park on its next() (no data, no growth).
                delay(100)
                val framesAtPark = frameCount.get()

                // (d) EXPLICIT teardown (not the Cleaner). (a) bounded wake: the
                // withTimeout throws loudly if the parked next() is never woken.
                val elapsedMs =
                    measureTimeMillis {
                        query.cancel()
                        withTimeout(1_000) { collection.join() }
                    }
                assertTrue(
                    elapsedMs < 1_000,
                    "explicit cancel woke the parked next() immediately (Rust-side): ${elapsedMs}ms",
                )
                // (b) no frame delivered after cancel.
                assertEquals(
                    framesAtPark,
                    frameCount.get(),
                    "no post-cancel frame may be delivered",
                )
                // (c) idempotent.
                query.cancel()
                query.cancel()
            } finally {
                engine.shutdown()
            }
        }

    /**
     * #680 Item 3 (coroutine-cancellation path): cancelling the COLLECTING
     * coroutine of a cold `observeQuery` flow drops the in-flight Rust `next()`
     * future and runs the flow's `finally { handle.cancel() }` -- deterministic
     * teardown that does NOT rely on the JVM `Cleaner`/GC. `cancelAndJoin()`
     * must complete promptly (a bounded, Rust-side wake), and no frame may be
     * delivered after cancellation.
     */
    @Test
    fun cancellingCollectingCoroutineRunsFinallyTeardownWithoutGc() =
        runBlocking<Unit> {
            val engine = NMPEngine(NMPConfig())
            try {
                val registration = engine.addAccount(TEST_SECRET_KEY)
                engine.setActiveAccount(registration.publicKey)

                val frameCount = AtomicInteger(0)
                val firstBatch = CompletableDeferred<Unit>()
                // Cold unbounded flow: `engine.observe(filter)` opens its own
                // handle inside `flow { }` per collect; teardown is the flow's
                // `finally`, run on coroutine cancellation -- not the Cleaner.
                val job =
                    launch {
                        engine.observe(NMPFilter(kinds = listOf(8_812u))).collect {
                            frameCount.incrementAndGet()
                            if (!firstBatch.isCompleted) firstBatch.complete(Unit)
                            // Then the pull parks on next() until we cancel.
                        }
                    }

                withTimeout(5_000) { firstBatch.await() }
                delay(100)
                val framesAtPark = frameCount.get()

                // (d)+(a): cancelAndJoin drops the parked next() future and runs
                // the finally -> handle.cancel(); it must complete within bound.
                val elapsedMs = measureTimeMillis { withTimeout(1_000) { job.cancelAndJoin() } }
                assertTrue(
                    elapsedMs < 1_000,
                    "cancelAndJoin ran the finally teardown promptly: ${elapsedMs}ms",
                )
                // (b) no frame delivered after cancellation.
                delay(100)
                assertEquals(
                    framesAtPark,
                    frameCount.get(),
                    "no post-cancel frame may be delivered",
                )
            } finally {
                engine.shutdown()
            }
        }

    // MARK: - Item 4: multi-consumer contract (the core Kotlin requirement)

    /**
     * #680 Item 4 (core): open ONE cold `Flow` from `observeQuery(...)` and
     * launch TWO CONCURRENT collectors on it. Because the flow is COLD, each
     * `collect` runs `engine.observe(...)` inside `flow { }` and gets its OWN
     * fresh single-consumer Rust handle -- two INDEPENDENT handles, never a
     * shared one. So both collectors receive their initial current-state batch
     * and NEITHER throws `ConcurrentNext` (there is no contention to trigger
     * it). This is the recommended Kotlin multi-consumer pattern.
     */
    @Test
    fun twoConcurrentCollectorsOnOneColdFlowBothDeliverNeitherThrowsConcurrentNext() =
        runBlocking<Unit> {
            val engine = NMPEngine(NMPConfig())
            try {
                val registration = engine.addAccount(TEST_SECRET_KEY)
                engine.setActiveAccount(registration.publicKey)

                // ONE Flow value, collected twice concurrently.
                val flow = engine.observe(NMPFilter(kinds = listOf(8_821u)))

                val collectorA = async { runCatching { flow.first() } }
                val collectorB = async { runCatching { flow.first() } }

                val resultA = withTimeout(10_000) { collectorA.await() }
                val resultB = withTimeout(10_000) { collectorB.await() }

                assertTrue(
                    resultA.isSuccess,
                    "collector A must deliver, not throw: ${resultA.exceptionOrNull()}",
                )
                assertTrue(
                    resultB.isSuccess,
                    "collector B must deliver, not throw: ${resultB.exceptionOrNull()}",
                )
                assertNotNull(resultA.getOrNull(), "collector A received its initial batch")
                assertNotNull(resultB.getOrNull(), "collector B received its initial batch")
                // Explicit: neither surfaced the single-consumer error, because
                // cold-flow collectors hold two INDEPENDENT handles.
                assertFalse(resultA.exceptionOrNull() is NMPError.ConcurrentNext)
                assertFalse(resultB.exceptionOrNull() is NMPError.ConcurrentNext)
            } finally {
                engine.shutdown()
            }
        }

    /**
     * #680 falsifier: many simultaneous observation `Flow` collections
     * coexist on ONE engine with NO capacity config and NO capacity error,
     * and cancelling them all tears every Rust handle down cleanly. Under the
     * deleted one-thread-per-observer / `maxNativeTasks` design, opening the
     * ~13th unrelated observation refused with `ExecutorSaturated`; under the
     * pull-based mailbox design there is no admission ceiling to hit -- the
     * only proof this test can even express is that no such config or error
     * exists (`NMPConfig()` takes no `maxNativeTasks`, and nothing here
     * catches a capacity refusal).
     */
    @Test
    fun manySimultaneousObservationsNeitherRefuseNorLeak() = runBlocking<Unit> {
        val engine = NMPEngine(NMPConfig())
        try {
            val collectors = 128
            val firstSnapshots = List(collectors) { CompletableDeferred<Unit>() }
            val jobs = mutableListOf<Job>()

            repeat(collectors) { index ->
                jobs +=
                    launch {
                        engine.observeDiagnostics().collect {
                            firstSnapshots[index].complete(Unit)
                            // Hold the observation open (idle) until cancelled --
                            // hundreds of idle handles must coexist.
                            awaitCancellation()
                        }
                    }
            }

            // Every collector received its first snapshot: none was refused
            // for a native-task ceiling (there is none).
            withTimeout(20_000) {
                firstSnapshots.forEach { it.await() }
            }

            // Cancelling the collecting coroutines drops each in-flight Rust
            // `next()` future and runs `handle.cancel()` in the flow's
            // `finally` -- deterministic teardown, no bricked handles.
            withTimeout(20_000) {
                jobs.forEach { it.cancelAndJoin() }
            }

            // The engine is still healthy after mass open+cancel: a fresh
            // observation still yields its current snapshot.
            val revived = withTimeout(20_000) { engine.observeDiagnostics().first() }
            assertNotNull(revived)
        } finally {
            engine.shutdown()
        }
    }
}
