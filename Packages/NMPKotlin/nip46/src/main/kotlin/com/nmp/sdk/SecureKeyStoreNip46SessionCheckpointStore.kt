package com.nmp.sdk

import java.io.IOException
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
 * A secure-at-rest NIP-46 session checkpoint store backed by the JVM's own
 * `java.security.KeyStore` (#571) -- the desktop-JVM sibling of
 * [NMPSecureKeyStoreAccountStore], storing
 * [NMPNip46SessionCheckpoint.serialize]'s versioned bytes as a
 * [SecretKeyEntry] instead of a raw account secret string. See that type's
 * doc for the full honesty note about what "secure" means here (JVM
 * `KeyStore` + password, not hardware-backed) and the Android seam this
 * would eventually become (`AndroidKeyStore` + `EncryptedSharedPreferences`,
 * #40).
 */
class NMPSecureKeyStoreNip46SessionCheckpointStore(
    private val file: Path,
    password: CharArray,
    private val keyStoreType: String = "JCEKS",
) : NMPNip46SessionCheckpointStore {
    private val lock = Any()
    private val password: CharArray = password.copyOf()

    override fun loadCheckpoint(): NMPNip46SessionCheckpoint? =
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
                        "NIP-46 session keystore entry could not be recovered",
                        unrecoverable,
                    )
                }
            val bytes = entry?.secretKey?.encoded ?: return@synchronized null
            try {
                NMPNip46SessionCheckpoint.deserialize(bytes)
            } finally {
                Arrays.fill(bytes, 0)
            }
        }

    override fun saveCheckpoint(checkpoint: NMPNip46SessionCheckpoint) {
        synchronized(lock) {
            val directory =
                requireNotNull(file.parent) {
                    "NIP-46 session keystore must have a parent directory"
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
            val bytes = checkpoint.serialize()
            try {
                keyStore.setEntry(
                    ALIAS,
                    SecretKeyEntry(SecretKeySpec(bytes, KEY_SPEC_ALGORITHM)),
                    entryProtection(),
                )
            } catch (failure: GeneralSecurityException) {
                throw IllegalStateException(
                    "NIP-46 session keystore entry could not be written",
                    failure,
                )
            } finally {
                Arrays.fill(bytes, 0)
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
            throw IllegalStateException(
                "NIP-46 session keystore could not be opened (wrong password or corrupt file)",
                failure,
            )
        } catch (failure: GeneralSecurityException) {
            throw IllegalStateException("NIP-46 session keystore could not be opened", failure)
        }
        return keyStore
    }

    private fun entryProtection(): PasswordProtection = PasswordProtection(password.copyOf())

    private fun writeAtomically(keyStore: KeyStore) {
        val directory = requireNotNull(file.parent)
        val temporary = Files.createTempFile(directory, ".${file.fileName}.", ".tmp")
        try {
            try {
                Files.newOutputStream(temporary).use { output -> keyStore.store(output, password) }
            } catch (failure: IOException) {
                throw IllegalStateException("NIP-46 session keystore could not be written", failure)
            } catch (failure: GeneralSecurityException) {
                throw IllegalStateException("NIP-46 session keystore could not be written", failure)
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
        const val ALIAS = "nmp-nip46-session"
        const val KEY_SPEC_ALGORITHM = "RAW"
    }
}
