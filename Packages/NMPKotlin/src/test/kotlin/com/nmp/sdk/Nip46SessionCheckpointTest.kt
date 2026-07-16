package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

/**
 * #571: NMP-owned NIP-46 session checkpoint serialization and the
 * restore/import doors' no-checkpoint/refusal paths. The live pair ->
 * checkpoint -> restore -> resume-durable-write round trip is exercised by
 * the Rust-level falsifier
 * (`crates/nmp-engine/tests/nip46_restart.rs::checkpoint_restore_reattaches_durable_write_without_repairing`),
 * which this Kotlin wrapper composes verbatim -- there is no independent
 * Kotlin-owned NIP-46 transport to re-prove here, and no real remote-signer
 * counterpart is available in this environment for a live end-to-end
 * pairing test. Kotlin mirror of `Nip46SessionCheckpointTests.swift`.
 */
class Nip46SessionCheckpointTest {
    private fun makeCheckpoint(
        origin: NMPNip46SessionOrigin = NMPNip46SessionOrigin.Bunker,
    ) = NMPNip46SessionCheckpoint(
        clientSecretKey = "a".repeat(64),
        userPublicKey = "b".repeat(64),
        remoteSignerPublicKey = "c".repeat(64),
        relays = listOf("wss://relay.example", "wss://relay2.example"),
        origin = origin,
    )

    @Test
    fun serializeDeserializeRoundTripsEveryField() {
        for (origin in NMPNip46SessionOrigin.entries) {
            val checkpoint = makeCheckpoint(origin)
            val restored = NMPNip46SessionCheckpoint.deserialize(checkpoint.serialize())
            assertEquals(checkpoint, restored)
        }
    }

    @Test
    fun deserializeRejectsUnsupportedVersion() {
        val checkpoint = makeCheckpoint()
        val lines = String(checkpoint.serialize()).split("\n").toMutableList()
        lines[0] = "99"
        val data = lines.joinToString("\n").toByteArray()

        val error = assertThrows(
            NMPNip46SessionCheckpoint.SerializationException.UnsupportedVersion::class.java,
        ) {
            NMPNip46SessionCheckpoint.deserialize(data)
        }
        assertEquals(99, error.version)
    }

    @Test
    fun deserializeRejectsUnknownOrigin() {
        val checkpoint = makeCheckpoint()
        val lines = String(checkpoint.serialize()).split("\n").toMutableList()
        lines[1] = "CarrierPigeon"
        val data = lines.joinToString("\n").toByteArray()

        val error = assertThrows(
            NMPNip46SessionCheckpoint.SerializationException.InvalidOrigin::class.java,
        ) {
            NMPNip46SessionCheckpoint.deserialize(data)
        }
        assertEquals("CarrierPigeon", error.origin)
    }

    @Test
    fun deserializeRejectsMalformedBytes() {
        assertThrows(NMPNip46SessionCheckpoint.SerializationException.Malformed::class.java) {
            NMPNip46SessionCheckpoint.deserialize("not a checkpoint".toByteArray())
        }
    }

    /** #571 secrecy falsifier: `toString()` -- what `println`/string
     * interpolation surfaces -- may never contain the raw client secret. */
    @Test
    fun toStringRedactsTheClientSecret() {
        val checkpoint = makeCheckpoint()

        assertFalse(checkpoint.toString().contains(checkpoint.clientSecretKey))
        assertTrue(checkpoint.toString().contains("[redacted]"))
    }

    @Test
    fun restoreFromStoreReturnsNullWithoutConnectingWhenNoCheckpointExists() {
        val emptyStore = object : NMPNip46SessionCheckpointStore {
            override fun loadCheckpoint(): NMPNip46SessionCheckpoint? = null

            override fun saveCheckpoint(checkpoint: NMPNip46SessionCheckpoint) {}

            override fun clear() {}
        }

        NMPEngine(NMPConfig()).use { engine ->
            assertNull(engine.restoreNip46Session(emptyStore))
        }
    }

    /** A connection that has not reached `Ready` refuses `checkpoint()` with
     * a typed error rather than persisting meaningless material. */
    @Test
    fun checkpointBeforeReadyIsRefused() {
        val connection = NMPNip46Connection(NMPNip46Observer()) {}
        assertThrows(NMPError.InvalidSigner::class.java) {
            connection.checkpoint()
        }
    }
}
