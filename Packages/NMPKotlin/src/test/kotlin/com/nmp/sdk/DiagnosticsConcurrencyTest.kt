package com.nmp.sdk

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith

class DiagnosticsConcurrencyTest {
    @Test
    fun nativeExecutorSaturationIsTypedAndCancellationReturnsExactBaseline() = runBlocking {
        val engine = NMPEngine(NMPConfig(maxNativeTasks = 1u))
        try {
            val firstSnapshot = CompletableDeferred<Unit>()
            val held = launch {
                engine.observeDiagnostics().collect {
                    firstSnapshot.complete(Unit)
                    awaitCancellation()
                }
            }
            firstSnapshot.await()
            assertEquals(1uL, engine.nativeTaskCensus().admitted)

            val refusal = assertFailsWith<NMPError.ExecutorSaturated> {
                engine.observeDiagnostics().collect {}
            }
            assertEquals("diagnostics-observer", refusal.component)
            assertEquals(1uL, refusal.capacity)

            held.cancelAndJoin()
            engine.awaitNativeTasksIdle()
            assertEquals(0uL, engine.nativeTaskCensus().admitted)
            assertEquals(0uL, engine.nativeTaskCensus().running)
        } finally {
            engine.shutdown()
        }
    }
}
