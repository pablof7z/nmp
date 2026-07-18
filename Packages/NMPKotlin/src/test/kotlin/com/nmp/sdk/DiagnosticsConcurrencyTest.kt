package com.nmp.sdk

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Job
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlin.test.Test
import kotlin.test.assertNotNull

class DiagnosticsConcurrencyTest {
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
