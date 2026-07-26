package com.nmp.qualification.consumer

import android.content.Context
import android.util.Log
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import com.nmp.sdk.Durability
import com.nmp.sdk.NMPAccessContext
import com.nmp.sdk.NMPCacheMode
import com.nmp.sdk.NMPConfig
import com.nmp.sdk.NMPDemand
import com.nmp.sdk.NMPFilter
import com.nmp.sdk.NMPFreshness
import com.nmp.sdk.NMPSourceAuthority
import com.nmp.sdk.WriteIntent
import com.nmp.sdk.WritePayload
import com.nmp.sdk.WriteRouting
import com.nmp.sdk.WriteStatus
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicReference

@RunWith(AndroidJUnit4::class)
class NMPLifecycleQualificationTest {
    private val relayUrl = BuildConfig.NMP_QUALIFICATION_RELAY
    private val demand =
        NMPDemand(
            selection = NMPFilter(kinds = listOf(1u.toUShort())),
            source = NMPSourceAuthority.Pinned(setOf(relayUrl)),
            access = NMPAccessContext.Public,
            cache = NMPCacheMode.Strict,
            freshness = NMPFreshness.Live,
        )

    @Test
    fun configurationAndBackgroundRetainOneExplicitOwnerAndScreenCollection(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        awaitGlobalOwners(0)
        val application =
            ApplicationProvider.getApplicationContext<Context>() as QualificationApplication
        val owner = newOwner()
        application.installQualificationOwner(owner)
        var scenario: ActivityScenario<QualificationActivity>? = null

        try {
            assertEquals(
                QualificationLifecycleCensus(
                    engineState = QualificationEngineState.Open,
                    engineInstances = 1,
                    rowCollectors = 0,
                    diagnosticsCollectors = 0,
                    receiptCollectors = 0,
                    maxConcurrentRowCollectors = 0,
                ),
                owner.census.value,
            )
            awaitWireCount(owner, 0)
            val launched: ActivityScenario<QualificationActivity> =
                ActivityScenario.launch<QualificationActivity>(
                    QualificationActivity.lifecycleQualificationIntent(),
                )
            scenario = launched
            val firstModel = AtomicReference<QualificationScreenModel>()
            launched.onActivity { activity ->
                firstModel.set(activity.qualificationScreenModel)
            }
            val retainedModel = firstModel.get()
            assertNotNull(retainedModel)
            assertSame(owner, retainedModel.owner)

            awaitCensus(owner) {
                it.engineInstances == 1 && it.rowCollectors == 1 && it.totalCollectors == 1
            }
            withTimeout(20_000) {
                retainedModel.state.first { it.controlledEventIds.size == 1 }
            }
            awaitWireCount(owner, 1)
            QualificationEngineOwner.requireExactlyOneLiveEngineOwner()

            launched.recreate()
            val recreatedModel = AtomicReference<QualificationScreenModel>()
            launched.onActivity { activity ->
                recreatedModel.set(activity.qualificationScreenModel)
            }
            assertSame(
                "Activity recreation must retain the exact screen owner",
                retainedModel,
                recreatedModel.get(),
            )
            awaitCensus(owner) {
                it.engineInstances == 1 &&
                    it.rowCollectors == 1 &&
                    it.maxConcurrentRowCollectors == 1
            }
            assertEquals(1, retainedModel.state.value.controlledEventIds.size)
            awaitWireCount(owner, 1)

            launched.moveToState(Lifecycle.State.CREATED)
            awaitCensus(owner) { it.engineInstances == 1 && it.rowCollectors == 1 }
            awaitWireCount(owner, 1)
            launched.moveToState(Lifecycle.State.RESUMED)
            val foregroundModel = AtomicReference<QualificationScreenModel>()
            launched.onActivity { activity ->
                foregroundModel.set(activity.qualificationScreenModel)
            }
            assertSame(retainedModel, foregroundModel.get())
            awaitCensus(owner) {
                it.engineInstances == 1 &&
                    it.rowCollectors == 1 &&
                    it.maxConcurrentRowCollectors == 1
            }
            awaitWireCount(owner, 1)

            launched.close()
            scenario = null
            awaitCensus(owner) { it.rowCollectors == 0 && it.totalCollectors == 0 }
            awaitWireCount(owner, 0)
            owner.close()
            owner.close()
            awaitCensus(owner) {
                it.engineState == QualificationEngineState.Closed &&
                    it.engineInstances == 0 &&
                    it.totalCollectors == 0
            }
            awaitGlobalOwners(0)
            Log.i(
                TAG,
                "NMP_ANDROID_LIFECYCLE_RECREATED owner=${owner.instanceId} " +
                    "model=${retainedModel.instanceId} max_rows=1",
            )
        } finally {
            scenario?.close()
            try {
                owner.close()
            } finally {
                application.removeQualificationOwner(owner)
            }
        }
    }

    @Test
    fun twoColdCollectorsAreVisibleAndCancelIndependently(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        awaitGlobalOwners(0)
        val owner = newOwner()
        val accidentalSecondOwner = newOwner()
        try {
            awaitGlobalOwners(2)
            val duplicateFailure =
                runCatching {
                    QualificationEngineOwner.requireExactlyOneLiveEngineOwner()
                }.exceptionOrNull()
            assertTrue(
                "the engine-owner guard did not detect duplicate app ownership",
                duplicateFailure is IllegalStateException,
            )
        } finally {
            accidentalSecondOwner.close()
        }
        awaitGlobalOwners(1)
        QualificationEngineOwner.requireExactlyOneLiveEngineOwner()

        try {
            awaitCensus(owner) { it.totalCollectors == 0 }
            awaitWireCount(owner, 0)
            val firstRow = CompletableDeferred<Unit>()
            val secondRow = CompletableDeferred<Unit>()
            val coldFlow = owner.observe(demand)
            val first =
                launch {
                    coldFlow.collect { batch ->
                        if (batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }) {
                            firstRow.complete(Unit)
                        }
                    }
                }
            val second =
                launch {
                    coldFlow.collect { batch ->
                        if (batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }) {
                            secondRow.complete(Unit)
                        }
                    }
                }

            withTimeout(20_000) {
                firstRow.await()
                secondRow.await()
            }
            awaitCensus(owner) {
                it.rowCollectors == 2 && it.maxConcurrentRowCollectors == 2
            }
            awaitWireCount(owner, 1)

            first.cancelAndJoin()
            awaitCensus(owner) { it.rowCollectors == 1 }
            assertTrue(
                "cancelling one collector must leave its peer active",
                second.isActive,
            )
            awaitWireCount(owner, 1)

            second.cancelAndJoin()
            awaitCensus(owner) { it.rowCollectors == 0 }
            awaitWireCount(owner, 0)

            val cached =
                owner.observe(demand.copy(freshness = NMPFreshness.CacheOnly))
                    .first { batch ->
                        batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }
                    }
            assertEquals(
                1,
                cached.rows.count { it.content == CONTROLLED_EVENT_CONTENT },
            )
            awaitCensus(owner) { it.rowCollectors == 0 }
            awaitWireCount(owner, 0)
            Log.i(TAG, "NMP_ANDROID_COLD_FLOW_HANDLES max=2 surviving=1 final=0")
        } finally {
            owner.close()
        }
        awaitGlobalOwners(0)
    }

    @Test
    fun concurrentIdempotentCloseDrainsRowsDiagnosticsAndReceipt(): Unit = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        awaitGlobalOwners(0)
        val owner = newOwner()
        val callbackGate = CompletableDeferred<Unit>()
        val disposed = AtomicBoolean(false)
        val lateCallbacks = AtomicInteger(0)
        try {
            val relayAuthor =
                withTimeout(20_000) {
                    owner.observe(demand)
                        .first { batch ->
                            batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }
                        }
                        .rows
                        .single { it.content == CONTROLLED_EVENT_CONTENT }
                        .pubkey
                }
            awaitCensus(owner) { it.totalCollectors == 0 }
            awaitWireCount(owner, 0)
            val readyNativeFrame = CompletableDeferred<Unit>()
            val awaitingCapability = CompletableDeferred<Unit>()
            val rowJob =
                launch {
                    owner.observe(demand).collect { batch ->
                        if (batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }) {
                            readyNativeFrame.complete(Unit)
                            callbackGate.await()
                            if (disposed.get()) {
                                lateCallbacks.incrementAndGet()
                            }
                        }
                    }
                }
            val diagnosticsJob = launch { owner.observeDiagnostics().collect() }
            val receipt =
                owner.publish(
                    WriteIntent(
                        payload =
                            WritePayload.Unsigned(
                                pubkey = relayAuthor,
                                createdAt = (System.currentTimeMillis() / 1_000).toULong(),
                                kind = 1u.toUShort(),
                                tags = emptyList(),
                                content = "android lifecycle pending receipt",
                            ),
                        durability = Durability.Durable,
                        routing = WriteRouting.AuthorOutbox,
                        identityOverride = relayAuthor,
                    ),
                )
            val receiptJob =
                launch {
                    owner.observeReceipt(receipt).collect { status ->
                        if (status is WriteStatus.AwaitingCapability) {
                            awaitingCapability.complete(Unit)
                        }
                    }
                }

            withTimeout(20_000) {
                readyNativeFrame.await()
                awaitingCapability.await()
                owner.census.first {
                    it.rowCollectors == 1 &&
                        it.diagnosticsCollectors == 1 &&
                        it.receiptCollectors == 1
                }
            }

            // The ready row callback is suspended at a cancellable boundary.
            // Screen disposal marks app state terminal and cancels every
            // exact collection while two close callers race engine shutdown.
            disposed.set(true)
            rowJob.cancel()
            diagnosticsJob.cancel()
            receiptJob.cancel()
            val firstClose = async(Dispatchers.Default) { owner.close() }
            val secondClose = async(Dispatchers.Default) { owner.close() }
            callbackGate.complete(Unit)
            withTimeout(20_000) {
                firstClose.await()
                secondClose.await()
                rowJob.join()
                diagnosticsJob.join()
                receiptJob.join()
            }
            awaitCensus(owner) {
                it.engineState == QualificationEngineState.Closed &&
                    it.engineInstances == 0 &&
                    it.totalCollectors == 0
            }
            assertEquals(
                "a ready native frame called into disposed app state",
                0,
                lateCallbacks.get(),
            )
            owner.close()
            awaitGlobalOwners(0)
            Log.i(
                TAG,
                "NMP_ANDROID_LIFECYCLE_CLOSED collectors=3 residue=0 late_callbacks=0",
            )
        } finally {
            callbackGate.complete(Unit)
            owner.close()
            awaitGlobalOwners(0)
        }
    }

    private fun newOwner(): QualificationEngineOwner =
        QualificationEngineOwner(
            NMPConfig(
                allowedLocalRelayHosts = listOf("10.0.2.2"),
                maxRelays = 2u,
            ),
        )

    private suspend fun awaitCensus(
        owner: QualificationEngineOwner,
        predicate: (QualificationLifecycleCensus) -> Boolean,
    ): QualificationLifecycleCensus =
        withTimeout(10_000) {
            owner.census.first(predicate)
        }

    private suspend fun awaitGlobalOwners(expected: Int) {
        withTimeout(10_000) {
            QualificationEngineOwner.liveEngineOwners.first { it == expected }
        }
    }

    private suspend fun awaitWireCount(owner: QualificationEngineOwner, expected: Int) {
        withTimeout(10_000) {
            owner.rawDiagnostics().first { snapshot ->
                snapshot.relays
                    .filter { sameRelay(it.relay, relayUrl) }
                    .sumOf { it.wireSubCount.toInt() } == expected
            }
        }
    }

    private fun sameRelay(left: String, right: String): Boolean =
        left.trimEnd('/') == right.trimEnd('/')

    private companion object {
        const val TAG = "NMPQualification"
        const val CONTROLLED_EVENT_CONTENT = "nmp-android-controlled-relay"
    }
}
