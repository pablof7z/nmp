package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Test

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
}
