package com.nmp.sdk

import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.Semaphore
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

/**
 * #607: checkpoint bytes and SDK checkpoint metadata are one serialized
 * mutation domain. The fixture holds a destructive store operation open
 * while a successful account add tries to replace it, deterministically
 * exercising the old lost-checkpoint interleaving.
 */
class CheckpointMutationConcurrencyTest {
    private class BlockingClearCheckpoint(initialSecretKey: String? = null) :
        NMPLocalAccountCheckpoint {
        @Volatile
        private var secretKey: String? = initialSecretKey
        private val shouldBlockNextClear = AtomicBoolean(true)
        private val clearEntered = CountDownLatch(1)
        private val clearRelease = CountDownLatch(1)
        private val saveEntered = Semaphore(0)

        override fun loadSecretKey(): String? = secretKey

        override fun saveSecretKey(secretKey: String) {
            saveEntered.release()
            this.secretKey = secretKey
        }

        override fun clear() {
            if (shouldBlockNextClear.compareAndSet(true, false)) {
                clearEntered.countDown()
                check(clearRelease.await(5, TimeUnit.SECONDS)) {
                    "test did not release the blocked checkpoint clear"
                }
            }
            secretKey = null
        }

        fun awaitClearEntry(): Boolean = clearEntered.await(5, TimeUnit.SECONDS)

        fun awaitConcurrentSave(): Boolean = saveEntered.tryAcquire(1, TimeUnit.SECONDS)

        fun releaseClear() {
            clearRelease.countDown()
        }
    }

    private val secretOne = "0".repeat(63) + "1"
    private val secretTwo = "0".repeat(63) + "2"

    private fun assertRestartRestoresAndDetaches(
        publicKey: String,
        checkpoint: BlockingClearCheckpoint,
    ) {
        NMPEngine(NMPConfig(), checkpoint).use { restarted ->
            assertEquals(publicKey, restarted.activeAccount())
            assertTrue(restarted.detachPersistedAccount())
            assertNull(
                checkpoint.loadSecretKey(),
                "restart metadata must describe the same checkpoint material that was restored",
            )
        }
    }

    @Test
    fun addWaitsForConcurrentCheckpointClearAndPersistsTheNewAccount() {
        val checkpoint = BlockingClearCheckpoint(secretOne)
        val executor = Executors.newFixedThreadPool(2)
        try {
            NMPEngine(NMPConfig(), checkpoint).use { engine ->
                val clear = executor.submit<Unit> {
                    engine.clearPersistedAccount()
                }
                assertTrue(checkpoint.awaitClearEntry())

                val addStarted = CountDownLatch(1)
                val add = executor.submit<NMPAccountRegistration> {
                    addStarted.countDown()
                    engine.addAccount(secretTwo)
                }
                assertTrue(addStarted.await(5, TimeUnit.SECONDS))
                assertFalse(
                    checkpoint.awaitConcurrentSave(),
                    "save must not enter the checkpoint store while clear owns the mutation domain",
                )

                checkpoint.releaseClear()
                clear.get(5, TimeUnit.SECONDS)
                val registration = add.get(5, TimeUnit.SECONDS)

                assertEquals(secretTwo, checkpoint.loadSecretKey())
                engine.shutdown()
                assertRestartRestoresAndDetaches(registration.publicKey, checkpoint)
            }
        } finally {
            checkpoint.releaseClear()
            executor.shutdownNow()
        }
    }

    @Test
    fun addWaitsForConcurrentAccountRemovalAndPersistsTheNewAccount() {
        val checkpoint = BlockingClearCheckpoint()
        val executor = Executors.newFixedThreadPool(2)
        try {
            NMPEngine(NMPConfig(), checkpoint).use { engine ->
                val original = engine.addAccount(secretOne)
                assertTrue(checkpoint.awaitConcurrentSave())

                val remove = executor.submit<Boolean> {
                    engine.removeAccount(original)
                }
                assertTrue(checkpoint.awaitClearEntry())

                val addStarted = CountDownLatch(1)
                val add = executor.submit<NMPAccountRegistration> {
                    addStarted.countDown()
                    engine.addAccount(secretTwo)
                }
                assertTrue(addStarted.await(5, TimeUnit.SECONDS))
                assertFalse(
                    checkpoint.awaitConcurrentSave(),
                    "save must not enter the checkpoint store while remove owns the mutation domain",
                )

                checkpoint.releaseClear()
                assertTrue(remove.get(5, TimeUnit.SECONDS))
                val registration = add.get(5, TimeUnit.SECONDS)

                assertEquals(secretTwo, checkpoint.loadSecretKey())
                engine.shutdown()
                assertRestartRestoresAndDetaches(registration.publicKey, checkpoint)
            }
        } finally {
            checkpoint.releaseClear()
            executor.shutdownNow()
        }
    }
}
