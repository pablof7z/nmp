package com.nmp.qualification.consumer

import android.content.Context
import android.content.Intent
import android.util.Log
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
import com.nmp.sdk.NMPSourceAuthority
import com.nmp.sdk.SourceStatus
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeFalse
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

@RunWith(AndroidJUnit4::class)
class NMPRuntimeQualificationTest {
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
    fun publicFacadeObservesCancelsClosesAndReopensAndroidStorage() = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val context = ApplicationProvider.getApplicationContext<Context>()
        val store = File(context.filesDir, "nmp-runtime-qualification.redb")
        resetIfPresent(store)
        val activity =
            InstrumentationRegistry.getInstrumentation().startActivitySync(
                Intent(context, QualificationActivity::class.java)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            )
        assertTrue(activity is QualificationActivity)
        activity.finish()
        Log.i(TAG, "NMP_ANDROID_ACTIVITY_LAUNCHED true")
        val config =
            NMPConfig(
                storePath = store.absolutePath,
                allowedLocalRelayHosts = listOf("10.0.2.2"),
                maxRelays = 2u,
            )

        val engine = NMPEngine(config)
        val online =
            withTimeout(20_000) {
                engine.observe(demand).first { batch ->
                    batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT } &&
                        batch.evidence.sources.any { sameRelay(it.relay, relayUrl) }
                }
            }
        assertTrue(online.rows.any { it.content == CONTROLLED_EVENT_CONTENT })
        val source = online.evidence.sources.singleOrNull { sameRelay(it.relay, relayUrl) }
        assertNotNull("the controlled relay must own scoped evidence", source)
        Log.i(TAG, "NMP_ANDROID_OBSERVED rows=${online.rows.size} relay=$relayUrl")

        // `first` cancels collection, so Query.kt's finally block cancels the
        // native handle. Explicit close then owns engine teardown; the second
        // call proves the public close operation is idempotent.
        engine.close()
        engine.close()
        assertEngineClosed(engine)
        Log.i(TAG, "NMP_ANDROID_CLOSED first_engine=true")

        val reopened = NMPEngine(config)
        val cached =
            withTimeout(10_000) {
                reopened.observe(demand.copy(freshness = NMPFreshness.CacheOnly))
                    .first { batch ->
                        batch.rows.any { it.content == CONTROLLED_EVENT_CONTENT }
                    }
            }
        assertEquals(
            CONTROLLED_EVENT_CONTENT,
            cached.rows.single { it.content == CONTROLLED_EVENT_CONTENT }.content,
        )
        reopened.close()
        assertEngineClosed(reopened)
        Log.i(TAG, "NMP_ANDROID_REOPENED cached_rows=${cached.rows.size}")
    }

    @Test
    fun cancellationBeforeAnyRequiredFrameIsBounded() = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val engine = NMPEngine(NMPConfig(allowedLocalRelayHosts = listOf("10.0.2.2")))
        val unavailable = demandFor("ws://10.0.2.2:47392")
        val collection =
            launch(start = CoroutineStart.UNDISPATCHED) {
                engine.observe(unavailable).collect()
            }
        withTimeout(5_000) {
            collection.cancelAndJoin()
        }
        engine.close()
        assertEngineClosed(engine)
        Log.i(TAG, "NMP_ANDROID_CANCELLED bounded=true")
    }

    @Test
    fun unavailableRelaySurfacesScopedFailureEvidence() = runBlocking {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val engine = NMPEngine(NMPConfig(allowedLocalRelayHosts = listOf("10.0.2.2")))
        val unavailableRelay = "ws://10.0.2.2:47392"
        val failed =
            withTimeout(20_000) {
                engine.observe(demandFor(unavailableRelay)).first { batch ->
                    batch.evidence.sources.any { source ->
                        sameRelay(source.relay, unavailableRelay) &&
                            (source.status is SourceStatus.Disconnected ||
                                source.status is SourceStatus.Error)
                    }
                }
            }
        assertTrue(failed.rows.isEmpty())
        assertTrue(failed.evidence.sources.any { sameRelay(it.relay, unavailableRelay) })
        engine.close()
        Log.i(TAG, "NMP_ANDROID_UNAVAILABLE scoped_failure=true")
    }

    @Test
    fun missingEmulatorAbiFailsAtNativeConstruction() {
        assumeFalse(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        val failure = runCatching { NMPEngine(NMPConfig()) }.exceptionOrNull()
        assertNotNull("wrong-ABI AAR unexpectedly constructed NMPEngine", failure)
        assertTrue(
            "wrong-ABI failure must be a native load/link refusal, got $failure",
            generateSequence(failure) { it.cause }.any { it is UnsatisfiedLinkError },
        )
        Log.i(TAG, "NMP_ANDROID_WRONG_ABI_REFUSED type=${failure!!::class.java.name}")
    }

    private fun demandFor(relay: String): NMPDemand =
        demand.copy(source = NMPSourceAuthority.Pinned(setOf(relay)))

    private suspend fun assertEngineClosed(engine: NMPEngine) {
        val failure =
            runCatching {
                withTimeout(5_000) {
                    engine.observe(demand).first()
                }
            }.exceptionOrNull()
        assertTrue("closed engine still admitted an observation: $failure", failure is NMPError.EngineClosed)
    }

    private fun resetIfPresent(store: File) {
        if (store.exists()) {
            NMPEngine.resetPersistentStore(store.absolutePath)
        }
    }

    private fun sameRelay(left: String, right: String): Boolean =
        left.trimEnd('/') == right.trimEnd('/')

    private companion object {
        const val TAG = "NMPQualification"
        const val CONTROLLED_EVENT_CONTENT = "nmp-android-controlled-relay"
    }
}
