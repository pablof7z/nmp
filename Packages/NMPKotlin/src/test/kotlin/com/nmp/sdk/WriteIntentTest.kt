package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiWritePayload

// A conversion test of the ergonomic write noun -- the Kotlin mirror of
// FilterBuilderTests.swift's WriteIntent conversions. No network -- this
// only proves the Kotlin-value -> Ffi-value conversion is lossless,
// including the per-write identity override (#47).
class WriteIntentTest {
    private fun unsignedPayload(pubkey: String) =
        WritePayload.Unsigned(
            pubkey = pubkey,
            createdAt = 1_700_000_000uL,
            kind = 1u,
            tags = emptyList(),
            content = "hello from NMP",
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
                routing = WriteRouting.AuthorOutbox,
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
                routing = WriteRouting.AuthorOutbox,
            )
        assertNull(intent.identityOverride)
        assertNull(intent.toFfi().identityOverride)
    }

    /** #597: the generic native guard stays one closed write payload; the
     * complete caller-owned body and nullable exact base cross FFI together. */
    @Test
    fun guardedReplaceableWriteIntentConversionPreservesBodyAndBase() {
        val expectedBase = "a".repeat(64)
        val intent =
            WriteIntent(
                payload =
                    WritePayload.UnsignedReplaceableEdit(
                        pubkey = "b".repeat(64),
                        createdAt = 1_700_000_001uL,
                        kind = 10_042u,
                        tags = listOf(listOf("x", "caller-owned")),
                        content = "replacement",
                        expectedBase = expectedBase,
                    ),
                durability = Durability.Durable,
                routing = WriteRouting.AuthorOutbox,
            )

        val ffi = intent.toFfi().payload as FfiWritePayload.UnsignedReplaceableEdit
        assertEquals("b".repeat(64), ffi.pubkey)
        assertEquals(1_700_000_001uL, ffi.createdAt)
        assertEquals(10_042u.toUShort(), ffi.kind)
        assertEquals(listOf(listOf("x", "caller-owned")), ffi.tags)
        assertEquals("replacement", ffi.content)
        assertEquals(expectedBase, ffi.expectedBase)

        val first =
            WritePayload.UnsignedReplaceableEdit(
                pubkey = "b".repeat(64),
                createdAt = 1_700_000_002uL,
                kind = 10_043u,
                tags = emptyList(),
                content = "first",
                expectedBase = null,
            ).toFfi() as FfiWritePayload.UnsignedReplaceableEdit
        assertNull(first.expectedBase)
    }
}
