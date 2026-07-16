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
 * This type deliberately keeps the exact same three-method internal surface
 * as [NMPInsecureFileAccountStore] (`loadSecretKey(): String?`,
 * `saveSecretKey(String)`, `clear()`) so it is a shape-for-shape drop-in at
 * every call site inside `NMPEngine`. `NMPEngine`'s constructor currently
 * types its checkpoint parameter as the concrete
 * `NMPInsecureFileAccountStore?` rather than a shared interface -- widening
 * that parameter (or introducing a common `LocalAccountStore` interface
 * both types implement) is a follow-on edit to `Engine.kt`, deliberately
 * left untouched here to keep this change file-disjoint from the rest of
 * issue #47. Until that seam lands, callers exercise this type directly
 * (see `SecureKeyStoreAccountStoreTest`) rather than through `NMPEngine`.
 *
 * ### Honesty about what "secure" means here
 * This is NOT hardware-backed. The keystore file is only as strong as
 * [password]: whatever protects that password (OS keychain, user PIN,
 * derived passphrase, etc.) is what ultimately protects the secret key.
 * Callers must never hardcode the password or persist it beside the
 * keystore file -- doing so would degrade this back to
 * [NMPInsecureFileAccountStore] with extra steps. This constraint, and the
 * whole JVM-`KeyStore` approach, is a consequence of NMPKotlin currently
 * being desktop-JVM-only (no Android AAR target exists yet -- see the
 * Android seam note below).
 *
 * ### Android seam (future drop-in; #40 / platform gap)
 * A real Android build would replace this entirely with
 * `KeyStore.getInstance("AndroidKeyStore")`: a hardware/TEE-backed provider
 * that generates its key in place (never importing raw bytes the way this
 * type's [SecretKeySpec] does) and, paired with
 * `androidx.security.crypto.EncryptedSharedPreferences` for the small
 * amount of associated metadata, needs no caller-supplied [password] at
 * all -- the OS mediates access (biometric/device-credential gating)
 * instead. That variant is intentionally NOT implemented here because it
 * needs the Android AAR target that does not exist yet in this repo (#40).
 * The seam is kept clean on purpose: an `NMPAndroidKeyStoreAccountStore`
 * would only need to
 *   1. swap `KeyStore.getInstance(keyStoreType)` for
 *      `KeyStore.getInstance("AndroidKeyStore")`,
 *   2. generate (not import) its `SecretKey` via `KeyGenParameterSpec`,
 *   3. drop the [file]/[password]-based load/store round-trip in favor of
 *      `EncryptedSharedPreferences` for the (still non-secret) bookkeeping
 *      this type keeps in a file today,
 * while keeping the exact same `loadSecretKey` / `saveSecretKey` / `clear`
 * surface, so `NMPEngine` (once it accepts the shared seam described above)
 * would not need to change at all to pick either implementation.
 */
class NMPSecureKeyStoreAccountStore(
    private val file: Path,
    password: CharArray,
    private val keyStoreType: String = "JCEKS",
) {
    private val lock = Any()

    // Defensive copy: this type owns its lifetime of the password
    // independently from whatever the caller does with their own array
    // afterwards.
    private val password: CharArray = password.copyOf()

    internal fun loadSecretKey(): String? =
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

    internal fun saveSecretKey(secretKey: String) {
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

    internal fun clear() {
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
