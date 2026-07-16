package com.nmp.sdk

import java.nio.charset.StandardCharsets
import java.nio.file.AtomicMoveNotSupportedException
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.nio.file.StandardOpenOption
import java.nio.file.attribute.PosixFilePermission

/**
 * A pluggable local account checkpoint -- the seam [NMPEngine] uses to
 * restore the checkpointed account at construction and to persist one on
 * [NMPEngine.addAccount]. ANY conformer is a drop-in through the public
 * [NMPEngine] constructor: a platform-vault provider (Android Keystore), an
 * app-custom store, or the plaintext-file convenience below.
 *
 * A checkpoint hands the raw secret key to its holder by design -- the
 * engine needs it for account registration, and the app owns its keys
 * (#47): import, removal, backup, and consent are app policy, never SDK
 * policy. The recommended secure providers keep material in the platform
 * vault; [NMPInsecureFileAccountStore] remains the explicit
 * convenience-over-security option.
 */
interface NMPLocalAccountCheckpoint {
    /** The checkpointed secret key (hex or bech32 `nsec`), or `null` when
     * no account is checkpointed. */
    fun loadSecretKey(): String?

    /** Persist [secretKey] as the one checkpointed account, replacing any
     * previous checkpoint. */
    fun saveSecretKey(secretKey: String)

    /** Remove the checkpoint. A later [loadSecretKey] returns `null`. */
    fun clear()
}

/**
 * An explicit convenience-over-security local account checkpoint.
 *
 * The secret is plaintext UTF-8 in the caller-selected app-sandbox file.
 * This type does not use Keystore, encryption, or hardware-backed protection
 * -- and like every [NMPLocalAccountCheckpoint] its methods hand the raw
 * secret to whoever holds the store (see the interface's doc for why that is
 * the design, not a leak). Prefer a platform-vault conformer; this store
 * exists for the explicit convenience-over-security call.
 */
class NMPInsecureFileAccountStore(private val file: Path) : NMPLocalAccountCheckpoint {
    private val lock = Any()

    override fun loadSecretKey(): String? =
        synchronized(lock) {
            if (!Files.exists(file)) null else Files.readString(file, StandardCharsets.UTF_8)
        }

    override fun saveSecretKey(secretKey: String) {
        synchronized(lock) {
            val directory = requireNotNull(file.parent) {
                "local account file must have a parent directory"
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
            val temporary = Files.createTempFile(directory, ".${file.fileName}.", ".tmp")
            try {
                Files.writeString(
                    temporary,
                    secretKey,
                    StandardCharsets.UTF_8,
                    StandardOpenOption.TRUNCATE_EXISTING,
                    StandardOpenOption.WRITE,
                )
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
    }

    override fun clear() {
        synchronized(lock) {
            Files.deleteIfExists(file)
        }
    }
}
