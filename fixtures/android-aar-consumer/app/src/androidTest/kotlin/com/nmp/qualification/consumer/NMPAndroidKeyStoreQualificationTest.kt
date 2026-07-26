package com.nmp.qualification.consumer

import android.content.Context
import android.os.Build
import android.security.keystore.KeyInfo
import android.security.keystore.KeyProperties
import android.util.Log
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import com.nmp.sdk.NMPAndroidCheckpointException
import com.nmp.sdk.NMPAndroidKeyStoreAccountStore
import com.nmp.sdk.NMPAndroidKeyStoreNip46SessionCheckpointStore
import com.nmp.sdk.NMPNip46SessionCheckpoint
import com.nmp.sdk.NMPNip46SessionOrigin
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.security.KeyStore
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.concurrent.Callable
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import javax.crypto.SecretKey
import javax.crypto.SecretKeyFactory

@RunWith(AndroidJUnit4::class)
class NMPAndroidKeyStoreQualificationTest {
    private val context = ApplicationProvider.getApplicationContext<Context>()

    @Test
    fun accountAndNip46CheckpointsAreAuthenticatedCiphertext() {
        val account = NMPAndroidKeyStoreAccountStore(context, "ciphertext-account")
        val session =
            NMPAndroidKeyStoreNip46SessionCheckpointStore(
                context,
                "ciphertext-nip46",
            )
        account.clear()
        session.clear()

        try {
            val accountSecret = randomHex(32)
            val checkpoint =
                NMPNip46SessionCheckpoint(
                    clientSecretKey = randomHex(32),
                    userPublicKey = randomHex(32),
                    remoteSignerPublicKey = randomHex(32),
                    relays = listOf(BuildConfig.NMP_QUALIFICATION_RELAY),
                    origin = NMPNip46SessionOrigin.Bunker,
                )
            account.saveSecretKey(accountSecret)
            session.saveCheckpoint(checkpoint)

            assertEquals(accountSecret, account.loadSecretKey())
            assertEquals(checkpoint, session.loadCheckpoint())
            val accountFile = ciphertextFile(account.wrappingKeyAlias)
            val sessionFile = ciphertextFile(session.wrappingKeyAlias)
            assertTrue(accountFile.isFile)
            assertTrue(sessionFile.isFile)
            assertNeedlesAbsent(
                context.dataDir,
                listOf(
                    accountSecret.toByteArray(),
                    checkpoint.clientSecretKey.toByteArray(),
                    checkpoint.serialize(),
                ),
            )

            val accountSecurity = wrappingKeySecurity(account.wrappingKeyAlias)
            val sessionSecurity = wrappingKeySecurity(session.wrappingKeyAlias)
            Log.i(
                TAG,
                "NMP_ANDROID_KEYSTORE_CIPHERTEXT account=$accountSecurity " +
                    "nip46=$sessionSecurity secp256k1=engine-memory plaintext=absent",
            )
        } finally {
            account.clear()
            session.clear()
        }
    }

    @Test
    fun tamperedCiphertextFailsBeforeAccountRegistration() {
        val store = NMPAndroidKeyStoreAccountStore(context, "tamper-account")
        store.clear()
        try {
            val secret = randomHex(32)
            store.saveSecretKey(secret)
            val file = ciphertextFile(store.wrappingKeyAlias)
            val bytes = file.readBytes()
            bytes[bytes.lastIndex] = (bytes.last().toInt() xor 0x01).toByte()
            file.writeBytes(bytes)

            expectThrows<NMPAndroidCheckpointException.CorruptCiphertext> {
                store.loadSecretKey()
            }
            Log.i(TAG, "NMP_ANDROID_KEYSTORE_TAMPER refused=corrupt-ciphertext")
        } finally {
            store.clear()
        }
    }

    @Test
    fun retainedCiphertextNeverRegeneratesDeletedWrappingKey() {
        val store = NMPAndroidKeyStoreAccountStore(context, "missing-key-account")
        store.clear()
        try {
            store.saveSecretKey(randomHex(32))
            assertTrue(ciphertextFile(store.wrappingKeyAlias).isFile)
            androidKeyStore().deleteEntry(store.wrappingKeyAlias)

            val failure =
                expectThrows<NMPAndroidCheckpointException.WrappingKeyMissing> {
                    store.loadSecretKey()
                }
            assertEquals(store.wrappingKeyAlias, failure.alias)
            assertFalse(androidKeyStore().containsAlias(store.wrappingKeyAlias))
            Log.i(TAG, "NMP_ANDROID_KEYSTORE_INVALIDATED refused=missing-key")
        } finally {
            store.clear()
        }
    }

    @Test
    fun concurrentSaveClearAndRestoreLinearize() {
        val first = NMPAndroidKeyStoreAccountStore(context, "concurrent-account")
        val second = NMPAndroidKeyStoreAccountStore(context, "concurrent-account")
        val secretOne = randomHex(32)
        val secretTwo = randomHex(32)
        val executor = Executors.newFixedThreadPool(4)
        first.clear()

        try {
            repeat(16) {
                first.clear()
                val start = CountDownLatch(1)
                val futures =
                    listOf(
                        executor.submit(
                            Callable {
                                start.await()
                                first.saveSecretKey(secretOne)
                            },
                        ),
                        executor.submit(
                            Callable {
                                start.await()
                                second.saveSecretKey(secretTwo)
                            },
                        ),
                        executor.submit(
                            Callable {
                                start.await()
                                first.clear()
                            },
                        ),
                        executor.submit(
                            Callable {
                                start.await()
                                second.loadSecretKey()
                            },
                        ),
                    )
                start.countDown()
                futures.forEach { it.get(10, TimeUnit.SECONDS) }
                assertTrue(first.loadSecretKey() in setOf(null, secretOne, secretTwo))
            }
            Log.i(TAG, "NMP_ANDROID_KEYSTORE_CONCURRENT rounds=16 partial=0")
        } finally {
            first.clear()
            executor.shutdownNow()
            assertTrue(executor.awaitTermination(10, TimeUnit.SECONDS))
        }
        assertNull(first.loadSecretKey())
    }

    private fun ciphertextFile(alias: String): File =
        File(
            File(context.noBackupFilesDir, "nmp-checkpoints"),
            "${sha256Hex(alias)}.checkpoint",
        )

    private fun wrappingKeySecurity(alias: String): String {
        val key = androidKeyStore().getKey(alias, null) as SecretKey
        val factory = SecretKeyFactory.getInstance(key.algorithm, "AndroidKeyStore")
        val info = factory.getKeySpec(key, KeyInfo::class.java) as KeyInfo
        return if (Build.VERSION.SDK_INT >= 31) {
            when (info.securityLevel) {
                KeyProperties.SECURITY_LEVEL_SOFTWARE -> "software"
                KeyProperties.SECURITY_LEVEL_TRUSTED_ENVIRONMENT -> "trusted-environment"
                KeyProperties.SECURITY_LEVEL_STRONGBOX -> "strongbox"
                KeyProperties.SECURITY_LEVEL_UNKNOWN_SECURE -> "unknown-secure"
                else -> "unknown-${info.securityLevel}"
            }
        } else if (info.isInsideSecureHardware) {
            "inside-secure-hardware"
        } else {
            "software"
        }
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

    private inline fun <reified T : Throwable> expectThrows(block: () -> Unit): T {
        val failure = runCatching(block).exceptionOrNull()
        assertTrue(
            "expected ${T::class.java.name}, got ${failure?.javaClass?.name}",
            failure is T,
        )
        return failure as T
    }

    private fun randomHex(bytes: Int): String =
        ByteArray(bytes)
            .also(SecureRandom()::nextBytes)
            .joinToString("") { "%02x".format(it) }

    private fun sha256Hex(value: String): String =
        MessageDigest.getInstance("SHA-256")
            .digest(value.toByteArray())
            .joinToString("") { "%02x".format(it) }

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
        const val MAX_SCAN_BYTES = 16L * 1024 * 1024
    }
}
