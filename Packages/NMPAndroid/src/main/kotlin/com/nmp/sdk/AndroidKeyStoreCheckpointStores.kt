package com.nmp.sdk

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.security.keystore.UserNotAuthenticatedException
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.nio.charset.StandardCharsets
import java.nio.file.AtomicMoveNotSupportedException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.GeneralSecurityException
import java.security.KeyStore
import java.security.MessageDigest
import java.util.Arrays
import java.util.concurrent.ConcurrentHashMap
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * A typed Android platform-checkpoint failure.
 *
 * Absence is deliberately not an exception: the existing account/session
 * checkpoint contracts represent a missing checkpoint as `null`. Every
 * failure below means bytes or a wrapping-key obligation did exist but could
 * not be used safely; callers never receive a replacement identity.
 */
sealed class NMPAndroidCheckpointException(
    message: String,
    cause: Throwable? = null,
) : Exception(message, cause) {
    /** Ciphertext exists, but its exact Android Keystore wrapping key does not. */
    class WrappingKeyMissing(val alias: String) :
        NMPAndroidCheckpointException(
            "Android checkpoint wrapping key is missing for alias $alias",
        )

    /** Android permanently invalidated the exact wrapping key. */
    class WrappingKeyInvalidated(
        val alias: String,
        cause: Throwable,
    ) : NMPAndroidCheckpointException(
        "Android checkpoint wrapping key is permanently invalidated for alias $alias",
        cause,
    )

    /** The key exists but Android requires an unlocked/authenticated user. */
    class UserLocked(
        val alias: String,
        cause: Throwable,
    ) : NMPAndroidCheckpointException(
        "Android checkpoint wrapping key is unavailable while the user is locked for alias $alias",
        cause,
    )

    /** The envelope is malformed, was tampered with, or has an unreadable payload. */
    class CorruptCiphertext(
        val checkpoint: String,
        cause: Throwable? = null,
    ) : NMPAndroidCheckpointException(
        "Android $checkpoint checkpoint ciphertext is corrupt or unauthenticated",
        cause,
    )

    enum class PersistenceOperation {
        Read,
        Write,
        Clear,
    }

    /** App-private checkpoint bytes could not be read, replaced, or removed. */
    class PersistenceFailure(
        val checkpoint: String,
        val operation: PersistenceOperation,
        cause: Throwable,
    ) : NMPAndroidCheckpointException(
        "Android $checkpoint checkpoint persistence failed during $operation",
        cause,
    )

    /** Android Keystore or its AES/GCM implementation refused an operation. */
    class PlatformKeyUnavailable(
        val alias: String,
        cause: Throwable,
    ) : NMPAndroidCheckpointException(
        "Android checkpoint wrapping key is unavailable for alias $alias",
        cause,
    )
}

/**
 * Android production checkpoint for one local Nostr account.
 *
 * A 256-bit AES/GCM key is generated in `AndroidKeyStore` and is never
 * exportable. Only authenticated ciphertext is stored under the app's
 * `noBackupFilesDir`; there is no caller password and no JCEKS file. The
 * wrapping key may be software-, TEE-, or StrongBox-backed depending on the
 * device. This class does **not** claim secp256k1 signing happens in Android
 * Keystore: [loadSecretKey] necessarily decrypts the account secret for the
 * engine-owned live signer, matching [NMPLocalAccountCheckpoint]'s contract.
 */
class NMPAndroidKeyStoreAccountStore(
    context: Context,
    name: String = "default",
) : NMPLocalAccountCheckpoint {
    private val encrypted =
        AndroidEncryptedCheckpoint(
            context = context,
            logicalName = validateLogicalName(name),
            kind = AndroidCheckpointKind.Account,
        )

    /** Exact app-scoped alias, exposed for platform security inspection. */
    val wrappingKeyAlias: String
        get() = encrypted.wrappingKeyAlias

    override fun loadSecretKey(): String? {
        val plaintext = encrypted.load() ?: return null
        return try {
            String(plaintext, StandardCharsets.UTF_8)
        } finally {
            Arrays.fill(plaintext, 0)
        }
    }

    override fun saveSecretKey(secretKey: String) {
        require(secretKey.isNotEmpty()) { "local account secret must not be empty" }
        val plaintext = secretKey.toByteArray(StandardCharsets.UTF_8)
        try {
            encrypted.save(plaintext)
        } finally {
            Arrays.fill(plaintext, 0)
        }
    }

    override fun clear() = encrypted.clear()
}

/**
 * Android production checkpoint for one governed NIP-46 client session.
 *
 * The security boundary is identical to [NMPAndroidKeyStoreAccountStore]:
 * Android Keystore owns a non-exportable AES/GCM wrapping key while the
 * app-private file contains only authenticated ciphertext. The decrypted
 * NIP-46 client key exists briefly in JVM/native memory when the ordinary
 * [restoreNip46Session] door reconstructs the engine-owned live session.
 */
class NMPAndroidKeyStoreNip46SessionCheckpointStore(
    context: Context,
    name: String = "default",
) : NMPNip46SessionCheckpointStore {
    private val encrypted =
        AndroidEncryptedCheckpoint(
            context = context,
            logicalName = validateLogicalName(name),
            kind = AndroidCheckpointKind.Nip46,
        )

    /** Exact app-scoped alias, exposed for platform security inspection. */
    val wrappingKeyAlias: String
        get() = encrypted.wrappingKeyAlias

    override fun loadCheckpoint(): NMPNip46SessionCheckpoint? {
        val plaintext = encrypted.load() ?: return null
        return try {
            try {
                NMPNip46SessionCheckpoint.deserialize(plaintext)
            } catch (failure: NMPNip46SessionCheckpoint.SerializationException) {
                throw NMPAndroidCheckpointException.CorruptCiphertext(
                    AndroidCheckpointKind.Nip46.label,
                    failure,
                )
            }
        } finally {
            Arrays.fill(plaintext, 0)
        }
    }

    override fun saveCheckpoint(checkpoint: NMPNip46SessionCheckpoint) {
        val plaintext = checkpoint.serialize()
        try {
            encrypted.save(plaintext)
        } finally {
            Arrays.fill(plaintext, 0)
        }
    }

    override fun clear() = encrypted.clear()
}

private enum class AndroidCheckpointKind(
    val id: Int,
    val label: String,
) {
    Account(1, "local-account"),
    Nip46(2, "nip46-session"),
}

private class AndroidEncryptedCheckpoint(
    context: Context,
    logicalName: String,
    private val kind: AndroidCheckpointKind,
) {
    private val applicationContext = context.applicationContext
    val wrappingKeyAlias: String =
        "${applicationContext.packageName}.nmp.${kind.label}.$logicalName"
    private val directory = File(applicationContext.noBackupFilesDir, DIRECTORY)
    private val file = File(directory, "${sha256Hex(wrappingKeyAlias)}.checkpoint")
    private val lock = locks.computeIfAbsent(wrappingKeyAlias) { Any() }

    fun load(): ByteArray? =
        synchronized(lock) {
            if (!file.exists()) {
                return@synchronized null
            }
            val envelope =
                try {
                    Files.readAllBytes(file.toPath())
                } catch (failure: IOException) {
                    throw NMPAndroidCheckpointException.PersistenceFailure(
                        kind.label,
                        NMPAndroidCheckpointException.PersistenceOperation.Read,
                        failure,
                    )
                }
            try {
                decrypt(decodeEnvelope(envelope))
            } finally {
                Arrays.fill(envelope, 0)
            }
        }

    fun save(plaintext: ByteArray) {
        require(plaintext.size <= MAX_PLAINTEXT_BYTES) {
            "Android ${kind.label} checkpoint exceeds $MAX_PLAINTEXT_BYTES bytes"
        }
        synchronized(lock) {
            val key = loadOrCreateKey(ciphertextExists = file.exists())
            val cipher = newCipher()
            initCipher(cipher, Cipher.ENCRYPT_MODE, key)
            cipher.updateAAD(aad())
            val ciphertext =
                try {
                    cipher.doFinal(plaintext)
                } catch (failure: GeneralSecurityException) {
                    throw platformFailure(failure)
                } catch (failure: RuntimeException) {
                    throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                        wrappingKeyAlias,
                        failure,
                    )
                }
            val iv = cipher.iv.copyOf()
            val envelope =
                try {
                    encodeEnvelope(iv, ciphertext)
                } finally {
                    Arrays.fill(iv, 0)
                    Arrays.fill(ciphertext, 0)
                }
            try {
                writeAtomically(envelope)
            } finally {
                Arrays.fill(envelope, 0)
            }
        }
    }

    fun clear() {
        synchronized(lock) {
            try {
                Files.deleteIfExists(file.toPath())
            } catch (failure: IOException) {
                throw NMPAndroidCheckpointException.PersistenceFailure(
                    kind.label,
                    NMPAndroidCheckpointException.PersistenceOperation.Clear,
                    failure,
                )
            }
            try {
                openAndroidKeyStore().deleteEntry(wrappingKeyAlias)
            } catch (failure: GeneralSecurityException) {
                throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                    wrappingKeyAlias,
                    failure,
                )
            } catch (failure: RuntimeException) {
                throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                    wrappingKeyAlias,
                    failure,
                )
            }
        }
    }

    private fun decrypt(envelope: CipherEnvelope): ByteArray {
        try {
            val key = loadOrCreateKey(ciphertextExists = true)
            val cipher = newCipher()
            initCipher(
                cipher,
                Cipher.DECRYPT_MODE,
                key,
                GCMParameterSpec(GCM_TAG_BITS, envelope.iv),
            )
            cipher.updateAAD(aad())
            return try {
                cipher.doFinal(envelope.ciphertext)
            } catch (failure: AEADBadTagException) {
                throw NMPAndroidCheckpointException.CorruptCiphertext(kind.label, failure)
            } catch (failure: GeneralSecurityException) {
                throw platformFailure(failure)
            } catch (failure: RuntimeException) {
                if (failure.hasCause<AEADBadTagException>()) {
                    throw NMPAndroidCheckpointException.CorruptCiphertext(
                        kind.label,
                        failure,
                    )
                }
                throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                    wrappingKeyAlias,
                    failure,
                )
            }
        } finally {
            Arrays.fill(envelope.iv, 0)
            Arrays.fill(envelope.ciphertext, 0)
        }
    }

    private fun loadOrCreateKey(ciphertextExists: Boolean): SecretKey {
        val keyStore = openAndroidKeyStore()
        val existing =
            try {
                keyStore.getKey(wrappingKeyAlias, null)
            } catch (failure: GeneralSecurityException) {
                throw platformFailure(failure)
            } catch (failure: RuntimeException) {
                throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                    wrappingKeyAlias,
                    failure,
                )
            }
        if (existing != null) {
            return existing as? SecretKey
                ?: throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                    wrappingKeyAlias,
                    IllegalStateException("Android Keystore entry is not a SecretKey"),
                )
        }
        if (ciphertextExists) {
            throw NMPAndroidCheckpointException.WrappingKeyMissing(wrappingKeyAlias)
        }
        return try {
            val generator =
                KeyGenerator.getInstance(
                    KeyProperties.KEY_ALGORITHM_AES,
                    ANDROID_KEY_STORE,
                )
            generator.init(
                KeyGenParameterSpec.Builder(
                    wrappingKeyAlias,
                    KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
                )
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setKeySize(AES_KEY_BITS)
                    .setRandomizedEncryptionRequired(true)
                    .build(),
            )
            generator.generateKey()
        } catch (failure: GeneralSecurityException) {
            throw platformFailure(failure)
        } catch (failure: RuntimeException) {
            throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                wrappingKeyAlias,
                failure,
            )
        }
    }

    private fun openAndroidKeyStore(): KeyStore =
        try {
            KeyStore.getInstance(ANDROID_KEY_STORE).also { it.load(null) }
        } catch (failure: GeneralSecurityException) {
            throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                wrappingKeyAlias,
                failure,
            )
        } catch (failure: IOException) {
            throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                wrappingKeyAlias,
                failure,
            )
        } catch (failure: RuntimeException) {
            throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                wrappingKeyAlias,
                failure,
            )
        }

    private fun newCipher(): Cipher =
        try {
            Cipher.getInstance(AES_GCM)
        } catch (failure: GeneralSecurityException) {
            throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                wrappingKeyAlias,
                failure,
            )
        } catch (failure: RuntimeException) {
            throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                wrappingKeyAlias,
                failure,
            )
        }

    private fun initCipher(
        cipher: Cipher,
        mode: Int,
        key: SecretKey,
        parameters: GCMParameterSpec? = null,
    ) {
        try {
            if (parameters == null) {
                cipher.init(mode, key)
            } else {
                cipher.init(mode, key, parameters)
            }
        } catch (failure: KeyPermanentlyInvalidatedException) {
            throw NMPAndroidCheckpointException.WrappingKeyInvalidated(
                wrappingKeyAlias,
                failure,
            )
        } catch (failure: UserNotAuthenticatedException) {
            throw NMPAndroidCheckpointException.UserLocked(wrappingKeyAlias, failure)
        } catch (failure: GeneralSecurityException) {
            throw platformFailure(failure)
        } catch (failure: RuntimeException) {
            throw NMPAndroidCheckpointException.PlatformKeyUnavailable(
                wrappingKeyAlias,
                failure,
            )
        }
    }

    private fun platformFailure(failure: GeneralSecurityException):
        NMPAndroidCheckpointException =
        when (failure) {
            is KeyPermanentlyInvalidatedException ->
                NMPAndroidCheckpointException.WrappingKeyInvalidated(
                    wrappingKeyAlias,
                    failure,
                )
            is UserNotAuthenticatedException ->
                NMPAndroidCheckpointException.UserLocked(wrappingKeyAlias, failure)
            else ->
                NMPAndroidCheckpointException.PlatformKeyUnavailable(
                    wrappingKeyAlias,
                    failure,
                )
        }

    private fun aad(): ByteArray =
        "$FORMAT_VERSION\u0000${kind.label}\u0000$wrappingKeyAlias"
            .toByteArray(StandardCharsets.UTF_8)

    private fun encodeEnvelope(iv: ByteArray, ciphertext: ByteArray): ByteArray {
        val output = ByteArrayOutputStream()
        DataOutputStream(output).use { data ->
            data.writeInt(FORMAT_MAGIC)
            data.writeByte(FORMAT_VERSION)
            data.writeByte(kind.id)
            data.writeByte(iv.size)
            data.writeInt(ciphertext.size)
            data.write(iv)
            data.write(ciphertext)
        }
        return output.toByteArray()
    }

    private fun decodeEnvelope(bytes: ByteArray): CipherEnvelope {
        try {
            DataInputStream(ByteArrayInputStream(bytes)).use { data ->
                if (data.readInt() != FORMAT_MAGIC) {
                    throw malformedEnvelope()
                }
                if (data.readUnsignedByte() != FORMAT_VERSION) {
                    throw malformedEnvelope()
                }
                if (data.readUnsignedByte() != kind.id) {
                    throw malformedEnvelope()
                }
                val ivLength = data.readUnsignedByte()
                val ciphertextLength = data.readInt()
                if (ivLength !in MIN_IV_BYTES..MAX_IV_BYTES) {
                    throw malformedEnvelope()
                }
                if (ciphertextLength !in GCM_TAG_BYTES..MAX_CIPHERTEXT_BYTES) {
                    throw malformedEnvelope()
                }
                if (data.available() != ivLength + ciphertextLength) {
                    throw malformedEnvelope()
                }
                val iv = ByteArray(ivLength)
                val ciphertext = ByteArray(ciphertextLength)
                data.readFully(iv)
                data.readFully(ciphertext)
                return CipherEnvelope(iv, ciphertext)
            }
        } catch (failure: NMPAndroidCheckpointException.CorruptCiphertext) {
            throw failure
        } catch (failure: IOException) {
            throw NMPAndroidCheckpointException.CorruptCiphertext(kind.label, failure)
        }
    }

    private fun malformedEnvelope(): NMPAndroidCheckpointException.CorruptCiphertext =
        NMPAndroidCheckpointException.CorruptCiphertext(kind.label)

    private fun writeAtomically(bytes: ByteArray) {
        try {
            Files.createDirectories(directory.toPath())
            val temporary =
                Files.createTempFile(directory.toPath(), ".${file.name}.", ".tmp")
            try {
                FileOutputStream(temporary.toFile()).use { output ->
                    output.write(bytes)
                    output.fd.sync()
                }
                try {
                    Files.move(
                        temporary,
                        file.toPath(),
                        StandardCopyOption.ATOMIC_MOVE,
                        StandardCopyOption.REPLACE_EXISTING,
                    )
                } catch (_: AtomicMoveNotSupportedException) {
                    Files.move(
                        temporary,
                        file.toPath(),
                        StandardCopyOption.REPLACE_EXISTING,
                    )
                }
            } finally {
                Files.deleteIfExists(temporary)
            }
        } catch (failure: IOException) {
            throw NMPAndroidCheckpointException.PersistenceFailure(
                kind.label,
                NMPAndroidCheckpointException.PersistenceOperation.Write,
                failure,
            )
        }
    }

    private data class CipherEnvelope(
        val iv: ByteArray,
        val ciphertext: ByteArray,
    )

    private companion object {
        const val ANDROID_KEY_STORE = "AndroidKeyStore"
        const val AES_GCM = "AES/GCM/NoPadding"
        const val AES_KEY_BITS = 256
        const val GCM_TAG_BITS = 128
        const val GCM_TAG_BYTES = GCM_TAG_BITS / 8
        const val MIN_IV_BYTES = 12
        const val MAX_IV_BYTES = 32
        const val MAX_PLAINTEXT_BYTES = 64 * 1024
        const val MAX_CIPHERTEXT_BYTES = MAX_PLAINTEXT_BYTES + GCM_TAG_BYTES
        const val FORMAT_MAGIC = 0x4E4D504B
        const val FORMAT_VERSION = 1
        const val DIRECTORY = "nmp-checkpoints"
        val locks = ConcurrentHashMap<String, Any>()
    }
}

private fun validateLogicalName(name: String): String {
    require(LOGICAL_NAME.matches(name)) {
        "Android checkpoint name must match ${LOGICAL_NAME.pattern}"
    }
    return name
}

private fun sha256Hex(value: String): String =
    MessageDigest.getInstance("SHA-256")
        .digest(value.toByteArray(StandardCharsets.UTF_8))
        .joinToString("") { byte -> "%02x".format(byte) }

private inline fun <reified T : Throwable> Throwable.hasCause(): Boolean {
    var current: Throwable? = this
    while (current != null) {
        if (current is T) return true
        current = current.cause
    }
    return false
}

private val LOGICAL_NAME = Regex("[A-Za-z0-9._-]{1,80}")
