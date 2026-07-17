package com.nmp.sdk

import java.nio.charset.StandardCharsets
import uniffi.nmp_ffi.FfiNip46Origin
import uniffi.nmp_ffi.FfiNip46SessionCheckpoint

/** Distinguishes a NIP-46 session paired via `nostrconnect://` from one
 * dialed via `bunker://` (#571). Restore mechanics are identical either way
 * -- kept because "absence of a reusable client checkpoint is observable
 * rather than guessed from partial metadata" is a hard requirement, not
 * because restore branches on it. */
enum class NMPNip46SessionOrigin {
    ClientInitiated,
    Bunker,
    ;

    internal fun toFfi(): FfiNip46Origin =
        when (this) {
            ClientInitiated -> FfiNip46Origin.CLIENT_INITIATED
            Bunker -> FfiNip46Origin.BUNKER
        }

    internal companion object {
        fun from(ffi: FfiNip46Origin): NMPNip46SessionOrigin =
            when (ffi) {
                FfiNip46Origin.CLIENT_INITIATED -> ClientInitiated
                FfiNip46Origin.BUNKER -> Bunker
            }
    }
}

/**
 * NMP-owned checkpoint of the minimum secrets and descriptor needed to
 * reconnect an already-authorized NIP-46 client session without another
 * pairing handshake (#571) -- read from [NMPNip46Connection.checkpoint]
 * once `Ready`, persisted by an [NMPNip46SessionCheckpointStore], and
 * consumed by [restoreNip46Session]/[importNip46Session].
 *
 * [clientSecretKey] hands the raw secret to whoever holds this value, by
 * the same design as [NMPLocalAccountCheckpoint] (see that interface's
 * doc): the app owns its keys, and the recommended secure checkpoint
 * stores keep it in a secure store. [toString] is redacted -- it never
 * prints [clientSecretKey].
 */
data class NMPNip46SessionCheckpoint(
    val clientSecretKey: String,
    val userPublicKey: String,
    val remoteSignerPublicKey: String,
    val relays: List<String>,
    val origin: NMPNip46SessionOrigin,
) {
    internal constructor(ffi: FfiNip46SessionCheckpoint) : this(
        clientSecretKey = ffi.clientSecretKey,
        userPublicKey = ffi.userPublicKey,
        remoteSignerPublicKey = ffi.remoteSignerPublicKey,
        relays = ffi.relays,
        origin = NMPNip46SessionOrigin.from(ffi.origin),
    )

    internal fun toFfi(): FfiNip46SessionCheckpoint =
        FfiNip46SessionCheckpoint(
            clientSecretKey = clientSecretKey,
            userPublicKey = userPublicKey,
            remoteSignerPublicKey = remoteSignerPublicKey,
            relays = relays,
            origin = origin.toFfi(),
        )

    /** Redacted -- never includes [clientSecretKey]. */
    override fun toString(): String =
        "NMPNip46SessionCheckpoint(userPublicKey=$userPublicKey, " +
            "remoteSignerPublicKey=$remoteSignerPublicKey, relays=$relays, " +
            "origin=$origin, clientSecretKey=[redacted])"

    /**
     * Encode this checkpoint as NMP's own versioned wire format: one
     * newline-delimited record (version tag, then every field, relays
     * length-prefixed) -- independent of the FFI-generated type's own field
     * order/renames, and dependency-free (no JSON library is a main-scope
     * dependency of this module). A checkpoint store persists these bytes
     * verbatim.
     */
    fun serialize(): ByteArray {
        val lines = mutableListOf(WIRE_VERSION.toString(), origin.name, clientSecretKey, userPublicKey, remoteSignerPublicKey, relays.size.toString())
        lines.addAll(relays)
        return lines.joinToString("\n").toByteArray(StandardCharsets.UTF_8)
    }

    /**
     * A serialized checkpoint blob did not decode as this format.
     *
     * This wire format is POSITIONAL (fixed line indices), unlike Swift's
     * keyed-JSON equivalent: a blob missing or shifted by one line can make
     * an unrelated field's raw content -- including the client secret --
     * land at the exact line index this decoder reads as "version" or
     * "origin". Every message below is therefore purely positional/
     * descriptive (line index, counts) and NEVER interpolates the actual
     * line content read from the blob, so this is always safe to log even
     * against a corrupt or adversarially shifted blob.
     */
    sealed class SerializationException(message: String) : Exception(message) {
        class UnsupportedVersion(val version: Int) :
            SerializationException("unsupported NIP-46 checkpoint wire version: $version")

        object InvalidOrigin :
            SerializationException("checkpoint wire line 1 (origin) did not match a known origin tag")

        class Malformed(reason: String) : SerializationException("malformed NIP-46 checkpoint: $reason")
    }

    companion object {
        private const val WIRE_VERSION = 1

        /** Decode a checkpoint previously produced by [serialize]. An
         * unrecognized version, corrupt bytes, or an unknown origin tag
         * fails closed with a typed [SerializationException] rather than
         * guessing. */
        fun deserialize(data: ByteArray): NMPNip46SessionCheckpoint {
            val lines = String(data, StandardCharsets.UTF_8).split("\n")
            if (lines.size < 6) {
                throw SerializationException.Malformed("expected at least 6 lines, got ${lines.size}")
            }
            val version = lines[0].toIntOrNull()
                ?: throw SerializationException.Malformed("wire line 0 (version) was not numeric")
            if (version != WIRE_VERSION) {
                throw SerializationException.UnsupportedVersion(version)
            }
            val origin = when (lines[1]) {
                NMPNip46SessionOrigin.ClientInitiated.name -> NMPNip46SessionOrigin.ClientInitiated
                NMPNip46SessionOrigin.Bunker.name -> NMPNip46SessionOrigin.Bunker
                else -> throw SerializationException.InvalidOrigin
            }
            val relayCount = lines[5].toIntOrNull()
                ?: throw SerializationException.Malformed("wire line 5 (relay count) was not numeric")
            val relays = lines.drop(6)
            if (relays.size != relayCount) {
                throw SerializationException.Malformed(
                    "declared $relayCount relays but found ${relays.size}",
                )
            }
            return NMPNip46SessionCheckpoint(
                clientSecretKey = lines[2],
                userPublicKey = lines[3],
                remoteSignerPublicKey = lines[4],
                relays = relays,
                origin = origin,
            )
        }
    }
}

/**
 * The seam an [NMPEngine] NIP-46 restore/import door reads/writes through,
 * mirroring [NMPLocalAccountCheckpoint]'s load/save/clear shape (#571): a
 * secure-store provider, an app-custom store, or an app-supplied
 * convenience type are all drop-ins. A checkpoint hands the raw client
 * secret to its holder by the same design as [NMPLocalAccountCheckpoint]
 * (see that interface's doc) -- the app owns its keys.
 */
interface NMPNip46SessionCheckpointStore {
    /** The checkpointed session, or `null` when none is stored. */
    fun loadCheckpoint(): NMPNip46SessionCheckpoint?

    /** Persist [checkpoint], replacing any previous one. */
    fun saveCheckpoint(checkpoint: NMPNip46SessionCheckpoint)

    /** Remove the checkpoint. Idempotent -- a later [loadCheckpoint] returns
     * `null`. Never called by [NMPNip46Connection.close]; only explicit
     * session removal clears it. */
    fun clear()
}
