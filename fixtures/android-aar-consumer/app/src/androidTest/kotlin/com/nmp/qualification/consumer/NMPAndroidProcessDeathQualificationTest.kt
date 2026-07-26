package com.nmp.qualification.consumer

import android.content.Context
import android.util.Log
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.nmp.sdk.Durability
import com.nmp.sdk.NMPAccessContext
import com.nmp.sdk.NMPAndroidKeyStoreAccountStore
import com.nmp.sdk.NMPAndroidKeyStoreNip46SessionCheckpointStore
import com.nmp.sdk.NMPBinding
import com.nmp.sdk.NMPCacheMode
import com.nmp.sdk.NMPConfig
import com.nmp.sdk.NMPDemand
import com.nmp.sdk.NMPEngine
import com.nmp.sdk.NMPFilter
import com.nmp.sdk.NMPFreshness
import com.nmp.sdk.NMPNip46Connection
import com.nmp.sdk.NMPNip46ConnectionState
import com.nmp.sdk.NMPSourceAuthority
import com.nmp.sdk.ReceiptReattachment
import com.nmp.sdk.WriteIntent
import com.nmp.sdk.WritePayload
import com.nmp.sdk.WriteRouting
import com.nmp.sdk.WriteStatus
import com.nmp.sdk.connectNip46
import com.nmp.sdk.restoreNip46Session
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.take
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.net.URLEncoder
import java.nio.charset.StandardCharsets
import java.security.KeyStore
import kotlin.time.Duration.Companion.seconds

/**
 * Three host-orchestrated instrumentation phases.
 *
 * Each `am instrument` invocation runs in a fresh target process. The host
 * force-stops the app between phases; app-private checkpoints, the NMP redb
 * store, and non-secret receipt coordinates are the only cross-process state.
 */
@RunWith(AndroidJUnit4::class)
class NMPAndroidProcessDeathQualificationTest {
    private val context = ApplicationProvider.getApplicationContext<Context>()
    private val accountStore =
        NMPAndroidKeyStoreAccountStore(context, PROCESS_ACCOUNT_NAME)
    private val sessionStore =
        NMPAndroidKeyStoreNip46SessionCheckpointStore(context, PROCESS_SESSION_NAME)
    private val preferences =
        context.getSharedPreferences(PROCESS_COORDINATES, Context.MODE_PRIVATE)
    private val storePath =
        File(context.noBackupFilesDir, "nmp-process-death.redb").absolutePath

    @Test
    fun seedProtectedCheckpointsAndDurableReceipt(): Unit = runBlocking {
        assumePhase("seed")
        assertTrue(BuildConfig.NMP_NIP46_REMOTE_PUBKEY.isNotEmpty())
        assertTrue(BuildConfig.NMP_NIP46_PAIRING_SECRET.isNotEmpty())
        accountStore.clear()
        sessionStore.clear()
        preferences.edit().clear().commit()
        if (File(storePath).exists()) {
            NMPEngine.resetPersistentStore(storePath)
        }

        val engine = NMPEngine(config(), accountStore)
        var connection: NMPNip46Connection? = null
        try {
            val localAccount = engine.generateAccount()
            engine.setActiveAccount(localAccount.publicKey)
            val localSecret = requireNotNull(accountStore.loadSecretKey())
            val pairedConnection =
                engine.connectNip46(
                    bunkerUri(),
                    timeout = 15.seconds,
                )
            connection = pairedConnection
            val ready = awaitNip46Terminal(pairedConnection)
            assertTrue(
                "controlled NIP-46 bunker did not become ready: $ready",
                ready is NMPNip46ConnectionState.Ready,
            )
            val remoteUser = (ready as NMPNip46ConnectionState.Ready).userPublicKey
            val checkpoint = pairedConnection.checkpoint()
            assertEquals(remoteUser, checkpoint.userPublicKey)
            assertEquals(
                BuildConfig.NMP_NIP46_REMOTE_PUBKEY,
                checkpoint.remoteSignerPublicKey,
            )
            sessionStore.saveCheckpoint(checkpoint)
            pairedConnection.close()
            connection = null

            val receipt =
                engine.publish(
                    WriteIntent(
                        payload =
                            WritePayload.Unsigned(
                                pubkey = remoteUser,
                                createdAt = (System.currentTimeMillis() / 1_000).toULong(),
                                kind = 1u.toUShort(),
                                tags = emptyList(),
                                content = PROCESS_WRITE_CONTENT,
                            ),
                        durability = Durability.Durable,
                        routing = WriteRouting.AuthorOutbox,
                        identityOverride = remoteUser,
                        correlation = PROCESS_CORRELATION,
                    ),
                )
            val awaiting =
                withTimeout(20_000) {
                    receipt.status.first { it is WriteStatus.AwaitingCapability }
                }
            assertEquals(
                remoteUser,
                (awaiting as WriteStatus.AwaitingCapability).pubkey,
            )
            assertTrue(
                preferences.edit()
                    .putString(RECEIPT_ID, receipt.id.toString())
                    .putString(RECEIPT_CORRELATION, PROCESS_CORRELATION)
                    .putString(LOCAL_ACCOUNT_PUBKEY, localAccount.publicKey)
                    .putString(REMOTE_USER_PUBKEY, remoteUser)
                    .commit(),
            )

            engine.close()
            assertNeedlesAbsent(
                context.dataDir,
                listOf(
                    localSecret.toByteArray(),
                    checkpoint.clientSecretKey.toByteArray(),
                    checkpoint.serialize(),
                ),
            )
            Log.i(
                TAG,
                "NMP_ANDROID_PROCESS_SEEDED receipt=${receipt.id} " +
                    "account=protected nip46=protected",
            )
        } finally {
            connection?.close()
            engine.close()
        }
    }

    @Test
    fun restoreIdentitySessionAndExactReceipt(): Unit = runBlocking {
        assumePhase("restore")
        val receiptId = requireCoordinate(RECEIPT_ID).toULong()
        val correlation = requireCoordinate(RECEIPT_CORRELATION)
        val expectedLocalAccount = requireCoordinate(LOCAL_ACCOUNT_PUBKEY)
        val expectedRemoteUser = requireCoordinate(REMOTE_USER_PUBKEY)

        val engine = NMPEngine(config(), accountStore)
        var connection: NMPNip46Connection? = null
        try {
            assertEquals(expectedLocalAccount, engine.activeAccount())
            val protectedCheckpoint = sessionStore.loadCheckpoint()
            assertNotNull(protectedCheckpoint)
            assertEquals(expectedRemoteUser, protectedCheckpoint?.userPublicKey)

            val byId = engine.reattachReceipt(receiptId)
            assertTrue(byId is ReceiptReattachment.Attached)
            val replay =
                withTimeout(20_000) {
                    (byId as ReceiptReattachment.Attached)
                        .receipt
                        .status
                        .take(2)
                        .toList()
                }
            assertTrue(replay.first() is WriteStatus.Accepted)
            assertTrue(replay.last() is WriteStatus.AwaitingCapability)

            val byCorrelation = engine.reattachReceipt(correlation)
            assertTrue(byCorrelation is ReceiptReattachment.Attached)
            val correlatedReceipt =
                (byCorrelation as ReceiptReattachment.Attached).receipt
            assertEquals(
                receiptId,
                correlatedReceipt.id,
            )
            val terminal =
                async {
                    withTimeout(30_000) {
                        correlatedReceipt.status.first {
                            it is WriteStatus.Acked ||
                                it is WriteStatus.Failed ||
                                it is WriteStatus.Rejected ||
                                it is WriteStatus.GaveUp
                        }
                    }
                }

            val restoredConnection =
                requireNotNull(
                    engine.restoreNip46Session(
                        sessionStore,
                        timeout = 15.seconds,
                    ),
                )
            connection = restoredConnection
            val ready = awaitNip46Terminal(restoredConnection)
            assertTrue(
                "protected NIP-46 checkpoint did not restore: $ready",
                ready is NMPNip46ConnectionState.Ready,
            )
            assertEquals(
                expectedRemoteUser,
                (ready as NMPNip46ConnectionState.Ready).userPublicKey,
            )
            val terminalStatus = terminal.await()
            assertTrue(
                "restored obligation did not reach relay ACK: $terminalStatus",
                terminalStatus is WriteStatus.Acked,
            )

            assertTrue(engine.detachPersistedAccount())
            sessionStore.clear()
            assertTrue(preferences.edit().putBoolean(RESTORE_COMPLETED, true).commit())
            restoredConnection.close()
            connection = null
            engine.close()
            Log.i(
                TAG,
                "NMP_ANDROID_PROCESS_RESTORED receipt=$receiptId " +
                    "republished=0 account=exact nip46=exact",
            )
        } finally {
            connection?.close()
            engine.close()
        }
    }

    @Test
    fun clearedCredentialsStayAbsentAfterAnotherProcessDeath(): Unit = runBlocking {
        assumePhase("verify-clear")
        assertTrue(preferences.getBoolean(RESTORE_COMPLETED, false))
        assertNull(accountStore.loadSecretKey())
        assertNull(sessionStore.loadCheckpoint())
        assertFalse(androidKeyStore().containsAlias(accountStore.wrappingKeyAlias))
        assertFalse(androidKeyStore().containsAlias(sessionStore.wrappingKeyAlias))

        val receiptId = requireCoordinate(RECEIPT_ID).toULong()
        val expectedRemoteUser = requireCoordinate(REMOTE_USER_PUBKEY)
        val engine = NMPEngine(config(), accountStore)
        try {
            assertNull(engine.activeAccount())
            val reattached = engine.reattachReceipt(receiptId)
            assertTrue(reattached is ReceiptReattachment.Attached)
            val ack =
                withTimeout(20_000) {
                    (reattached as ReceiptReattachment.Attached)
                        .receipt
                        .status
                        .first { it is WriteStatus.Acked }
                }
            assertTrue(ack is WriteStatus.Acked)

            val cached =
                withTimeout(20_000) {
                    engine.observe(
                        NMPDemand(
                            selection =
                                NMPFilter(
                                    kinds = listOf(1u.toUShort()),
                                    authors = NMPBinding.Literal(setOf(expectedRemoteUser)),
                                ),
                            source =
                                NMPSourceAuthority.Pinned(
                                    setOf(BuildConfig.NMP_QUALIFICATION_RELAY),
                                ),
                            access = NMPAccessContext.Public,
                            cache = NMPCacheMode.Strict,
                            freshness = NMPFreshness.CacheOnly,
                        ),
                    ).first { batch ->
                        batch.rows.any { it.content == PROCESS_WRITE_CONTENT }
                    }
                }
            assertEquals(
                1,
                cached.rows.count { it.content == PROCESS_WRITE_CONTENT },
            )
            Log.i(
                TAG,
                "NMP_ANDROID_PROCESS_CLEARED identity=absent session=absent " +
                    "receipt=retained canonical_row=retained",
            )
        } finally {
            engine.close()
        }
    }

    private fun config(): NMPConfig =
        NMPConfig(
            storePath = storePath,
            appRelays = listOf(BuildConfig.NMP_QUALIFICATION_RELAY),
            allowedLocalRelayHosts = listOf("10.0.2.2"),
            maxRelays = 3u,
        )

    private fun bunkerUri(): String {
        val encodedRelay =
            URLEncoder.encode(
                BuildConfig.NMP_QUALIFICATION_RELAY,
                StandardCharsets.UTF_8.name(),
            )
        return "bunker://${BuildConfig.NMP_NIP46_REMOTE_PUBKEY}" +
            "?relay=$encodedRelay&secret=${BuildConfig.NMP_NIP46_PAIRING_SECRET}"
    }

    private suspend fun awaitNip46Terminal(
        connection: NMPNip46Connection,
    ): NMPNip46ConnectionState =
        withTimeout(20_000) {
            connection.states.first {
                it is NMPNip46ConnectionState.Ready ||
                    it is NMPNip46ConnectionState.Failed ||
                    it is NMPNip46ConnectionState.Closed
            }
        }

    private fun assumePhase(expected: String) {
        assumeTrue(BuildConfig.NMP_EXPECT_NATIVE_LOAD)
        assumeTrue(
            InstrumentationRegistry.getArguments()
                .getString(PROCESS_PHASE) == expected,
        )
    }

    private fun requireCoordinate(key: String): String =
        requireNotNull(preferences.getString(key, null)) {
            "missing non-secret process coordinate $key"
        }

    private fun androidKeyStore(): KeyStore =
        KeyStore.getInstance("AndroidKeyStore").also { it.load(null) }

    private fun assertNeedlesAbsent(root: File, needles: List<ByteArray>) {
        root.walkTopDown()
            .filter { it.isFile && it.length() <= MAX_SCAN_BYTES }
            .forEach { file ->
                val bytes = file.readBytes()
                for (needle in needles) {
                    assertFalse(
                        "plaintext checkpoint material appeared in ${file.relativeTo(root)}",
                        bytes.containsSubsequence(needle),
                    )
                }
            }
    }

    private fun ByteArray.containsSubsequence(needle: ByteArray): Boolean {
        if (needle.isEmpty() || needle.size > size) return false
        return indices
            .take(size - needle.size + 1)
            .any { offset ->
                needle.indices.all { index -> this[offset + index] == needle[index] }
            }
    }

    private companion object {
        const val TAG = "NMPQualification"
        const val PROCESS_PHASE = "nmpProcessPhase"
        const val PROCESS_ACCOUNT_NAME = "process-account"
        const val PROCESS_SESSION_NAME = "process-nip46"
        const val PROCESS_COORDINATES = "nmp-process-coordinates"
        const val RECEIPT_ID = "receipt-id"
        const val RECEIPT_CORRELATION = "receipt-correlation"
        const val LOCAL_ACCOUNT_PUBKEY = "local-account-pubkey"
        const val REMOTE_USER_PUBKEY = "remote-user-pubkey"
        const val RESTORE_COMPLETED = "restore-completed"
        const val PROCESS_CORRELATION = "android-process-death-receipt-v1"
        const val PROCESS_WRITE_CONTENT = "nmp-android-process-death-write"
        const val MAX_SCAN_BYTES = 16L * 1024 * 1024
    }
}
