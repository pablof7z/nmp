package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiDurability
import uniffi.nmp_ffi.FfiWriteIntent
import uniffi.nmp_ffi.FfiWritePayload
import uniffi.nmp_ffi.FfiWriteRouting

// A conversion test of the ergonomic write noun -- the Kotlin mirror of
// FilterBuilderTests.swift's WriteIntent conversions. No network -- this
// only proves the Kotlin-value -> Ffi-value conversion is lossless,
// including the per-write identity override (#47).
class WriteIntentTest {
    private fun unsignedPayload(pubkey: String) =
        WritePayload.Event(
            kind = 1u,
            tags = emptyList(),
            content = "hello from NMP",
            createdAt = 1_700_000_000uL,
        )

    /** #47: an [WriteIntent.identityOverride] crosses to `FfiWriteIntent`
     * intact -- the per-write identity is data, never rewritten or dropped
     * by the mirror. */
    @Test
    fun writeIntentConversionCarriesIdentityOverride() {
        val overridePubkey = "b".repeat(64)
        val intent =
            WriteIntent(
                payload = unsignedPayload(overridePubkey),
                durability = Durability.Durable,
                routing = WriteRouting.Auto,
                identityOverride = overridePubkey,
            )
        assertEquals(overridePubkey, intent.toFfi().identityOverride)
    }

    /** #47: the pre-existing construction shape stays source-compatible AND
     * semantically identical -- no override means `null`, the
     * active-account default, all the way through `toFfi()`. */
    @Test
    fun writeIntentDefaultLeavesIdentityOverrideNull() {
        val intent =
            WriteIntent(
                payload = unsignedPayload("b".repeat(64)),
                durability = Durability.Durable,
                routing = WriteRouting.Auto,
            )
        assertNull(intent.identityOverride)
        assertNull(intent.toFfi().identityOverride)
    }

    @Test
    fun writeIntentReverseProjectionPreservesEveryGenericField() {
        val unsigned =
            WriteIntent.from(
                FfiWriteIntent(
                    payload =
                        FfiWritePayload.Event(
                            FfiEventBuilder(
                                kind = 1111u,
                                tags = listOf(listOf("I", "podcast:item:guid:42")),
                                content = "unsigned",
                                createdAt = 42uL,
                            ),
                        ),
                    durability = FfiDurability.AT_MOST_ONCE,
                    routing = FfiWriteRouting.Auto,
                    identityOverride = "a".repeat(64),
                    correlation = "correlation-42",
                ),
            )
        assertEquals(
            WritePayload.Event(
                kind = 1111u,
                tags = listOf(listOf("I", "podcast:item:guid:42")),
                content = "unsigned",
                createdAt = 42uL,
            ),
            unsigned.payload,
        )
        assertEquals(Durability.AtMostOnce, unsigned.durability)
        assertEquals(WriteRouting.Auto, unsigned.routing)
        assertEquals("a".repeat(64), unsigned.identityOverride)
        assertEquals("correlation-42", unsigned.correlation)

        val signed =
            WriteIntent.from(
                FfiWriteIntent(
                    payload =
                        FfiWritePayload.Signed(
                            id = "b".repeat(64),
                            pubkey = "c".repeat(64),
                            createdAt = 43uL,
                            kind = 1u,
                            tags = listOf(listOf("e", "d".repeat(64))),
                            content = "signed",
                            sig = "e".repeat(128),
                        ),
                    durability = FfiDurability.EPHEMERAL,
                    routing = FfiWriteRouting.Auto,
                    identityOverride = null,
                    correlation = null,
                ),
            )
        assertEquals(
            WritePayload.Signed(
                id = "b".repeat(64),
                pubkey = "c".repeat(64),
                createdAt = 43uL,
                kind = 1u,
                tags = listOf(listOf("e", "d".repeat(64))),
                content = "signed",
                sig = "e".repeat(128),
            ),
            signed.payload,
        )
        assertEquals(Durability.Ephemeral, signed.durability)
        assertEquals(WriteRouting.Auto, signed.routing)
        assertNull(signed.identityOverride)
        assertNull(signed.correlation)

        assertEquals(Durability.Durable, Durability.from(FfiDurability.DURABLE))
    }

    /** #972: a Kotlin app can name the exact relays a write goes to -- the
     * relay list an app typed into a text field crosses the boundary
     * verbatim, in order, and comes back unchanged. */
    @Test
    fun explicitRoutingCarriesTheAppsExactRelayListBothWays() {
        val typed = listOf("wss://user-typed-relay.example", "wss://second.example")
        val intent =
            WriteIntent(
                payload = unsignedPayload("a".repeat(64)),
                durability = Durability.Durable,
                routing = WriteRouting.Explicit(typed),
            )
        assertEquals(FfiWriteRouting.Explicit(typed), intent.toFfi().routing)

        val back =
            WriteIntent.from(
                FfiWriteIntent(
                    payload =
                        FfiWritePayload.Unsigned(
                            pubkey = "a".repeat(64),
                            createdAt = 42uL,
                            kind = 1u,
                            tags = emptyList(),
                            content = "for the archive",
                        ),
                    durability = FfiDurability.DURABLE,
                    routing = FfiWriteRouting.Explicit(typed),
                    identityOverride = null,
                    correlation = null,
                ),
            )
        assertEquals(WriteRouting.Explicit(typed), back.routing)
    }
}
