package com.nmp.sdk

import java.io.IOException
import java.nio.charset.StandardCharsets
import java.nio.file.AtomicMoveNotSupportedException
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.nio.file.attribute.PosixFilePermission
import java.security.GeneralSecurityException
import java.security.KeyStore
import java.security.KeyStore.PasswordProtection
import java.security.KeyStore.SecretKeyEntry
import java.util.Arrays
import javax.crypto.spec.SecretKeySpec

/**
 * A secure-at-rest local account checkpoint backed by the JVM's own
 * `java.security.KeyStore` -- the honest desktop-JVM alternative to
 * [NMPInsecureFileAccountStore]'s deliberate plaintext-file compromise.
 *
 * The secret key never touches disk unencrypted: [saveSecretKey] wraps it in
 * a [SecretKeySpec] and stores it as a [SecretKeyEntry] inside a
 * password-protected keystore ([keyStoreType], `JCEKS` by default -- chosen
 * over `PKCS12` because `JCEKS` has always had first-class, provider-uniform
 * support for arbitrary `SecretKeyEntry` storage, which is all this type
 * needs). [loadSecretKey] transparently reopens and decrypts that keystore
 * file if present -- this IS the "automatic restore on init" the checkpoint
 * contract requires: a caller never has to do anything beyond calling
 * [loadSecretKey] again after a restart, exactly like
 * [NMPInsecureFileAccountStore].
 *
 * This type implements [NMPLocalAccountCheckpoint], the same three-method seam
 * consumed directly by [NMPEngine], so desktop callers can use it for
 * automatic account restore.
 *
 * ### Honesty about what "secure" means here
 * This is NOT hardware-backed. The keystore file is only as strong as
 * [password]: whatever protects that password (OS keychain, user PIN,
 * derived passphrase, etc.) is what ultimately protects the secret key.
 * Callers must never hardcode the password or persist it beside the
 * keystore file -- doing so would degrade this back to
 * [NMPInsecureFileAccountStore] with extra steps. This constraint, and the
 * whole JVM-`KeyStore` approach, is why this provider is desktop-only and
 * excluded from the Android AAR.
 *
 * Android callers use the AAR's `NMPAndroidKeyStoreAccountStore` instead. It
 * generates a non-exportable AES/GCM wrapping key inside `AndroidKeyStore`,
 * needs no caller password, and persists only authenticated ciphertext in
 * app-private storage. Its measured hardware security level remains
 * device-dependent, and restoring a checkpoint still places the secp256k1
 * secret in the engine's live signer memory.
 */
class NMPSecureKeyStoreAccountStore(
    private val file: Path,
    password: CharArray,
    private val keyStoreType: String = "JCEKS",
) : NMPLocalAccountCheckpoint {
    private val lock = Any()

    // Defensive copy: this type owns its lifetime of the password
    // independently from whatever the caller does with their own array
    // afterwards.
    private val password: CharArray = password.copyOf()

    override fun loadSecretKey(): String? =
        synchronized(lock) {
            if (!Files.exists(file)) {
                return@synchronized null
            }
            val keyStore = openExisting()
            val entry =
                try {
                    keyStore.getEntry(ALIAS, entryProtection()) as? SecretKeyEntry
                } catch (unrecoverable: java.security.UnrecoverableEntryException) {
                    throw IllegalStateException(
                        "local account keystore entry could not be recovered",
                        unrecoverable,
                    )
                }
            val secretBytes = entry?.secretKey?.encoded ?: return@synchronized null
            try {
                String(secretBytes, StandardCharsets.UTF_8)
            } finally {
                Arrays.fill(secretBytes, 0)
            }
        }

    override fun saveSecretKey(secretKey: String) {
        synchronized(lock) {
            val directory =
                requireNotNull(file.parent) {
                    "local account keystore must have a parent directory"
                }
            Files.createDirectories(directory)
            try {
                Files.setPosixFilePermissions(
                    directory,
                    setOf(
                        PosixFilePermission.OWNER_READ,
                        PosixFilePermission.OWNER_WRITE,
                        PosixFilePermission.OWNER_EXECUTE,
                    ),
                )
            } catch (_: UnsupportedOperationException) {
                // The selected filesystem does not expose POSIX modes.
            }

            val keyStore = if (Files.exists(file)) openExisting() else emptyKeyStore()
            val secretBytes = secretKey.toByteArray(StandardCharsets.UTF_8)
            try {
                keyStore.setEntry(
                    ALIAS,
                    SecretKeyEntry(SecretKeySpec(secretBytes, KEY_SPEC_ALGORITHM)),
                    entryProtection(),
                )
            } catch (failure: GeneralSecurityException) {
                throw IllegalStateException(
                    "local account keystore entry could not be written",
                    failure,
                )
            } finally {
                Arrays.fill(secretBytes, 0)
            }
            writeAtomically(keyStore)
        }
    }

    override fun clear() {
        synchronized(lock) {
            Files.deleteIfExists(file)
        }
    }

    private fun emptyKeyStore(): KeyStore =
        KeyStore.getInstance(keyStoreType).also { it.load(null, password) }

    private fun openExisting(): KeyStore {
        val keyStore = KeyStore.getInstance(keyStoreType)
        try {
            Files.newInputStream(file).use { input -> keyStore.load(input, password) }
        } catch (failure: IOException) {
            // Covers both a corrupt/foreign file and a wrong password --
            // java.security.KeyStore surfaces both as IOException (an
            // integrity-check failure). Fail closed rather than silently
            // treating either as "no checkpoint".
            throw IllegalStateException(
                "local account keystore could not be opened (wrong password or corrupt file)",
                failure,
            )
        } catch (failure: GeneralSecurityException) {
            throw IllegalStateException("local account keystore could not be opened", failure)
        }
        return keyStore
    }

    private fun entryProtection(): PasswordProtection =
        // A fresh PasswordProtection per call: some KeyStore SPIs destroy
        // (zero) the ProtectionParameter they were handed, which would
        // otherwise poison this type's own long-lived `password` field on
        // the next call.
        PasswordProtection(password.copyOf())

    private fun writeAtomically(keyStore: KeyStore) {
        val directory = requireNotNull(file.parent)
        val temporary = Files.createTempFile(directory, ".${file.fileName}.", ".tmp")
        try {
            try {
                Files.newOutputStream(temporary).use { output -> keyStore.store(output, password) }
            } catch (failure: IOException) {
                throw IllegalStateException("local account keystore could not be written", failure)
            } catch (failure: GeneralSecurityException) {
                throw IllegalStateException("local account keystore could not be written", failure)
            }
            try {
                Files.setPosixFilePermissions(
                    temporary,
                    setOf(PosixFilePermission.OWNER_READ, PosixFilePermission.OWNER_WRITE),
                )
            } catch (_: UnsupportedOperationException) {
                // The selected filesystem does not expose POSIX modes.
            }
            try {
                Files.move(
                    temporary,
                    file,
                    StandardCopyOption.ATOMIC_MOVE,
                    StandardCopyOption.REPLACE_EXISTING,
                )
            } catch (_: AtomicMoveNotSupportedException) {
                Files.move(temporary, file, StandardCopyOption.REPLACE_EXISTING)
            }
        } finally {
            Files.deleteIfExists(temporary)
        }
    }

    private companion object {
        const val ALIAS = "nmp-local-account"

        // Only used as the algorithm tag on the SecretKeySpec wrapper --
        // this store round-trips raw bytes and never performs a
        // cryptographic operation with this "key" itself, so any stable
        // tag name is fine.
        const val KEY_SPEC_ALGORITHM = "RAW"
    }
}
