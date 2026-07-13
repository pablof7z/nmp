package com.nmp.sdk

import java.nio.charset.StandardCharsets
import java.nio.file.AtomicMoveNotSupportedException
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.StandardCopyOption
import java.nio.file.StandardOpenOption
import java.nio.file.attribute.PosixFilePermission

/**
 * An explicit convenience-over-security local account checkpoint.
 *
 * The secret is plaintext UTF-8 in the caller-selected app-sandbox file.
 * This type does not use Keystore, encryption, or hardware-backed protection.
 * Read/write operations stay internal to the NMP SDK so consuming code cannot
 * retrieve the secret through this public surface.
 */
class NMPInsecureFileAccountStore(private val file: Path) {
    private val lock = Any()

    internal fun loadSecretKey(): String? =
        synchronized(lock) {
            if (!Files.exists(file)) null else Files.readString(file, StandardCharsets.UTF_8)
        }

    internal fun saveSecretKey(secretKey: String) {
        synchronized(lock) {
            val directory = requireNotNull(file.parent) {
                "local account file must have a parent directory"
            }
            Files.createDirectories(directory)
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

    internal fun clear() {
        synchronized(lock) {
            Files.deleteIfExists(file)
        }
    }
}
