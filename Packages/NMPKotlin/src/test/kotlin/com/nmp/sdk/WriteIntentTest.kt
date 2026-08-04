package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiEventBuilder
import uniffi.nmp_ffi.FfiIdentity
import uniffi.nmp_ffi.FfiWriteIntent
import uniffi.nmp_ffi.FfiWritePayload
import uniffi.nmp_ffi.FfiWriteRouting

// A conversion test of the ergonomic write noun -- the Kotlin mirror of
// FilterBuilderTests.swift's WriteIntent conversions. No network -- this
// only proves the Kotlin-value -> Ffi-value conversion is lossless,
// including the per-write identity override (#47).
class WriteIntentTest {
    private fun builderPayload() =
        WritePayload.Event(
            kind = 1u,
            tags = emptyList(),
            content = "hello from NMP",
            createdAt = 1_700_000_000uL,
        )

    /** #47: an explicit [Identity] crosses to `FfiWriteIntent`
     * intact -- the per-write identity is data, never rewritten or dropped
     * by the mirror. */
    @Test
    fun writeIntentConversionCarriesAnExplicitIdentity() {
        val named = "b".repeat(64)
        val intent =
            WriteIntent(
                payload = builderPayload(),
                routing = WriteRouting.Auto,
                identity = Identity.Explicit(named),
            )
        assertEquals(FfiIdentity.Explicit(named), intent.toFfi().identity)
    }

    /** #47: naming nobody is not the absence of a choice -- the default
     * construction means [Identity.Active], "whoever is active at
     * acceptance", all the way through `toFfi()`. There is no third
     * "unset" state to observe. */
    @Test
    fun writeIntentDefaultMeansTheActiveAccount() {
        val intent =
            WriteIntent(
                payload = builderPayload(),
                routing = WriteRouting.Auto,
            )
        assertEquals(Identity.Active, intent.identity)
        assertEquals(FfiIdentity.Active, intent.toFfi().identity)
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
                    routing = FfiWriteRouting.Auto,
                    identity = FfiIdentity.Explicit("a".repeat(64)),
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
        assertEquals(WriteRouting.Auto, unsigned.routing)
        assertEquals(Identity.Explicit("a".repeat(64)), unsigned.identity)
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
                    routing = FfiWriteRouting.Auto,
                    identity = FfiIdentity.Active,
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
        assertEquals(WriteRouting.Auto, signed.routing)
        assertEquals(Identity.Active, signed.identity)
        assertNull(signed.correlation)
    }

    /** #972: a Kotlin app can name the exact relays a write goes to -- the
     * relay list an app typed into a text field crosses the boundary
     * verbatim, in order, and comes back unchanged. */
    @Test
    fun explicitRoutingCarriesTheAppsExactRelayListBothWays() {
        val typed = listOf("wss://user-typed-relay.example", "wss://second.example")
        val intent =
            WriteIntent(
                payload = builderPayload(),
                routing = WriteRouting.Explicit(typed),
            )
        assertEquals(FfiWriteRouting.Explicit(typed), intent.toFfi().routing)

        val back =
            WriteIntent.from(
                FfiWriteIntent(
                    payload =
                        FfiWritePayload.Event(
                            FfiEventBuilder(
                                kind = 1u,
                                tags = emptyList(),
                                content = "for the archive",
                                createdAt = 42uL,
                            ),
                        ),
                    routing = FfiWriteRouting.Explicit(typed),
                    identity = FfiIdentity.Active,
                    correlation = null,
                ),
            )
        assertEquals(WriteRouting.Explicit(typed), back.routing)
    }
}
